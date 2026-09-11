//! Host composition, ownership and local administration.
use crate::{
    command_runtime,
    config::Config,
    control::{ModuleRequest, Request},
    value,
};
use oracle_core::{tasks::HostTasks, *};
use oracle_modules::ModuleManager;
use oracle_operations::ingress::HumanOperations;
use oracle_storage::{PgTools, Storage};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

pub(crate) struct Host {
    pub(crate) ai: std::sync::OnceLock<Arc<oracle_ai::coordinator::Coordinator>>,
    pub(crate) operations: std::sync::OnceLock<Arc<oracle_operations::executor::StructureExecutor>>,
    pub(crate) command_sync:
        std::sync::OnceLock<Arc<oracle_operations::commands::CommandReconciler>>,
    pub(crate) command_status: std::sync::Mutex<BTreeMap<GuildId, serde_json::Value>>,
    pub(crate) operation_tasks: HostTasks,
    pub(crate) storage: Arc<Storage>,
    pub(crate) core: Arc<CoreService>,
    pub(crate) modules: Arc<ModuleManager>,
    pub(crate) tools: PgTools,
    _state_lock: std::fs::File,
}
impl Host {
    pub(crate) async fn open(config: &Config, tools: PgTools) -> Result<Self> {
        config.prepare_state_dir()?;
        use std::os::unix::fs::OpenOptionsExt;
        let state_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(config.state_dir.join("host.lock"))
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        fs2::FileExt::try_lock_exclusive(&state_lock)
            .map_err(|_| Error::new(ErrorCode::AlreadyRunning))?;
        let storage = Arc::new(
            Storage::open_with_options(
                config.database()?,
                &tools,
                &config.state_dir.join("migration-backups"),
            )
            .await?,
        );
        if let Err(error) = storage
            .initialize_guilds(
                &config
                    .guilds
                    .iter()
                    .map(|g| g.guild.clone())
                    .collect::<Vec<_>>(),
            )
            .await
        {
            let _ = storage.close().await;
            return Err(error);
        }
        let core = Arc::new(CoreService::new(storage.clone(), config.guilds.clone()));
        // Recover semantic AI state even when Gemini is disabled or no key exists.
        // This host-only pass never reconstructs Discord authority or invokes tools.
        let recovery = async {
            let runs = oracle_ai::state::RunStore::new(core.clone(), storage.clone());
            let spend = oracle_ai::spend::SpendStore::new(storage.clone());
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::new(ErrorCode::Integrity))?
                .as_millis()
                .try_into()
                .map_err(|_| Error::new(ErrorCode::Integrity))?;
            for guild in storage.status(None).await?.guilds {
                oracle_ai::recovery::recover_guild(
                    &runs,
                    &spend,
                    storage.as_ref(),
                    &guild.guild,
                    now_ms,
                )
                .await?;
            }
            Ok::<(), Error>(())
        };
        let recovered = tokio::time::timeout(Duration::from_secs(30), recovery)
            .await
            .map_err(|_| Error::new(ErrorCode::RecoveryRequired))
            .and_then(|result| result);
        if let Err(error) = recovered {
            let _ = storage.close().await;
            return Err(error);
        }
        let modules = match ModuleManager::new(
            storage.clone(),
            core.clone(),
            config.state_dir.join("modules"),
        ) {
            Ok(modules) => modules,
            Err(error) => {
                let _ = storage.close().await;
                return Err(error);
            }
        };
        Ok(Self {
            ai: std::sync::OnceLock::new(),
            operations: std::sync::OnceLock::new(),
            command_sync: std::sync::OnceLock::new(),
            command_status: std::sync::Mutex::new(BTreeMap::new()),
            operation_tasks: HostTasks::new(),
            storage,
            core,
            modules,
            tools,
            _state_lock: state_lock,
        })
    }
    pub(crate) async fn close(&self) -> Result<()> {
        self.operation_tasks.shutdown(Duration::from_secs(30)).await;
        let modules = self.modules.shutdown().await;
        let storage = self.storage.close().await;
        modules.and(storage)
    }
    pub(crate) async fn handle(&self, request: Request) -> Result<serde_json::Value> {
        match request {
            Request::PublishCommands => command_runtime::publish_once(self).await,
            Request::Operation { guild, request } => {
                self.execute(
                    &PolicyContext::LocalOperator,
                    &guild,
                    request,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
            }
            Request::Module { request } => match request {
                ModuleRequest::Install {
                    source,
                    trust_native,
                } => value(self.modules.install(&source, trust_native).await?),
                ModuleRequest::List {} => value(self.modules.installations().await?),
                ModuleRequest::Load { digest } => {
                    self.modules.load(&digest).await?;
                    Ok(serde_json::json!({"loaded":true,"digest":digest}))
                }
                ModuleRequest::Upgrade {
                    module,
                    digest,
                    grace_ms,
                } => {
                    let grace = module_grace(grace_ms)?;
                    self.modules.upgrade(&module, &digest, grace).await?;
                    Ok(serde_json::json!({"upgraded":true,"module":module,"digest":digest}))
                }
                ModuleRequest::Activate {
                    module,
                    guild,
                    grants,
                    bindings,
                } => {
                    self.modules
                        .activate(
                            &PolicyContext::LocalOperator,
                            DesiredActivation {
                                module: module.clone(),
                                guild: guild.clone(),
                                active: true,
                                grants,
                                bindings,
                            },
                        )
                        .await?;
                    Ok(serde_json::json!({"activated":true,"module":module,"guild":guild}))
                }
                ModuleRequest::Invoke {
                    module,
                    guild,
                    operation,
                    input,
                } => {
                    self.modules
                        .invoke(
                            &PolicyContext::LocalOperator,
                            &module,
                            &guild,
                            &operation,
                            input,
                        )
                        .await
                }
                ModuleRequest::Deactivate {
                    module,
                    guild,
                    grace_ms,
                } => {
                    let grace = module_grace(grace_ms)?;
                    value(
                        self.modules
                            .deactivate(&PolicyContext::LocalOperator, &module, &guild, grace)
                            .await?,
                    )
                }
                ModuleRequest::Unload { module, grace_ms } => {
                    let grace = module_grace(grace_ms)?;
                    value(self.modules.unload(&module, grace).await?)
                }
                ModuleRequest::Health {} => value(self.modules.health().await),
            },
            Request::Status { guild } => {
                let mut status = value(
                    self.core
                        .status(&PolicyContext::LocalOperator, guild.as_ref())
                        .await?,
                )?;
                let events: Vec<_> = self
                    .modules
                    .event_health()
                    .into_iter()
                    .filter(|event| guild.as_ref().is_none_or(|id| id == &event.guild))
                    .collect();
                let object = status
                    .as_object_mut()
                    .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
                object.insert("ai_available".into(), value(self.ai.get().is_some())?);
                object.insert("module_events".into(), value(events)?);
                let commands: BTreeMap<_, _> = self
                    .command_status
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(id, _)| guild.as_ref().is_none_or(|guild| guild == *id))
                    .map(|(id, status)| (id.clone(), status.clone()))
                    .collect();
                object.insert("module_commands".into(), value(commands)?);
                object.insert(
                    "available_event_intents".into(),
                    value(self.modules.event_intents())?,
                );
                Ok(status)
            }
            Request::Control {
                guild,
                paused,
                expected_revision,
            } => {
                let rev = match expected_revision {
                    Some(r) => r,
                    None => {
                        self.core
                            .status(&PolicyContext::LocalOperator, Some(&guild))
                            .await?
                            .guilds
                            .iter()
                            .find(|g| g.guild == guild)
                            .ok_or_else(|| Error::new(ErrorCode::NotFound))?
                            .revision
                    }
                };
                value(
                    self.core
                        .control(&PolicyContext::LocalOperator, &guild, paused, rev)
                        .await?,
                )
            }
            Request::Recovery { guild, limit } => value(
                self.core
                    .recovery(&PolicyContext::LocalOperator, &guild, limit)
                    .await?,
            ),
            Request::Backup { output } => value(self.storage.backup(&output, &self.tools).await?),
        }
    }
}

fn module_grace(milliseconds: u64) -> Result<Duration> {
    if milliseconds > 30_000 {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(Duration::from_millis(milliseconds))
}
