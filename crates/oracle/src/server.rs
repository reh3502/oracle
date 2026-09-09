//! Running host lifecycle, task supervision and local socket admission.
use crate::{
    cli_ops, command_runtime,
    config::{self, Config},
    control,
    host::Host,
    output,
};
use oracle_core::{
    tasks::{HostTasks, TaskError},
    *,
};
use oracle_storage::PgTools;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::net::{UnixListener, UnixStream};

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub(crate) async fn serve(config: Config, tools: PgTools) -> Result<()> {
    // Resolve required credentials before acquiring resources or publishing readiness.
    let token = config
        .discord
        .as_ref()
        .map(|discord| {
            oracle_discord::Token::from_str(&config::secret_env(&discord.token_env)?)
                .map_err(|_| Error::new(ErrorCode::InvalidInput))
        })
        .transpose()?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let host = Arc::new(Host::open(&config, tools).await?);
    if let Some(token) = &token {
        let adapter = Arc::new(oracle_discord::operations::DiscordOperations::new(
            token.clone(),
            host.core.clone(),
        )?);
        host.operations
            .set(Arc::new(
                oracle_operations::executor::StructureExecutor::new(
                    host.core.clone(),
                    host.storage.clone(),
                    adapter.clone(),
                ),
            ))
            .map_err(|_| Error::new(ErrorCode::Conflict))?;
        host.command_sync
            .set(Arc::new(
                oracle_operations::commands::CommandReconciler::new(
                    host.storage.clone(),
                    adapter.clone(),
                ),
            ))
            .map_err(|_| Error::new(ErrorCode::Conflict))?;
        host.modules
            .set_configuration_services(host.storage.clone(), adapter.clone())?;
        host.modules
            .set_event_services(Default::default(), adapter)?;
    } else {
        host.modules
            .set_configuration_services(host.storage.clone(), Arc::new(cli_ops::OfflinePolicy))?;
    }
    let mut stopped = None;
    let result: Result<()> = async {
        let socket = config.socket();
        if UnixStream::connect(&socket).await.is_ok() {
            return Err(Error::new(ErrorCode::AlreadyRunning));
        }
        if socket.exists() {
            std::fs::remove_file(&socket).map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        }
        let listener =
            UnixListener::bind(&socket).map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        let socket_guard = SocketGuard(socket.clone());
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        let tasks = HostTasks::new();
        // Every startup/run error after spawning work converges on joined cleanup.
        let result = run(
            &host,
            &config,
            token,
            &mut term,
            &mut interrupt,
            &listener,
            &tasks,
        )
        .await;
        drop(listener);
        let summary = tasks.shutdown(Duration::from_secs(15)).await;
        drop(socket_guard);
        stopped = Some(serde_json::json!({"event":"stopped","tasks":summary}));
        result
    }
    .await;
    let closed = host.close().await;
    if let Some(event) = stopped {
        output(event)?;
    }
    result.and(closed)
}

async fn run(
    host: &Arc<Host>,
    config: &Config,
    token: Option<oracle_discord::Token>,
    term: &mut tokio::signal::unix::Signal,
    interrupt: &mut tokio::signal::unix::Signal,
    listener: &UnixListener,
    tasks: &HostTasks,
) -> Result<()> {
    let stop = tasks.token();
    let health_host = host.clone();
    let health_stop = stop.clone();
    tasks
        .spawn("storage_health", async move {
            loop {
                tokio::select! { biased;
                    _ = health_stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {
                        if health_host.storage.status(None).await.is_err() {
                            health_stop.cancel();
                            return Err(TaskError);
                        }
                    }
                }
            }
        })
        .map_err(|_| Error::new(ErrorCode::Cancelled))?;
    let module_host = host.clone();
    let module_stop = stop.clone();
    tasks
        .spawn("module_health", async move {
            loop {
                tokio::select! { biased;
                    _ = module_stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                tokio::select! { biased;
                    _ = module_stop.cancelled() => return Ok(()),
                    outcome = module_host.modules.tick() => {
                        if outcome.is_err() {
                            module_stop.cancel();
                            return Err(TaskError);
                        }
                    }
                }
            }
        })
        .map_err(|_| Error::new(ErrorCode::Cancelled))?;
    let gateway = if let Some(token) = token {
        let gateway = Arc::new(oracle_discord::DiscordBootstrap::with_runtime(
            host.core.clone(),
            host.clone(),
            host.modules.clone(),
            config
                .discord
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?
                .intents
                .iter()
                .cloned()
                .collect(),
        )?);
        let running = gateway.clone();
        let cancel = stop.clone();
        tasks
            .spawn("discord_gateway", async move {
                let outcome = running.run_gateway(token, cancel.clone()).await;
                cancel.cancel();
                outcome.map_err(|_| TaskError)
            })
            .map_err(|_| Error::new(ErrorCode::Cancelled))?;
        tokio::select! { biased;
            _ = term.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = stop.cancelled() => return Err(Error::new(ErrorCode::Io)),
            ready = gateway.wait_ready(Duration::from_secs(30)) => {
                ready.map_err(|e| Error::with_source(ErrorCode::Io, e))?;
            }
        }
        Some(gateway)
    } else {
        None
    };
    let restored = host.modules.restore_desired().await?;
    for (module, error) in &restored {
        tracing::warn!(module = %module, error = ?error, "module desired state was not restored");
    }
    if gateway.is_some() {
        let command_host = host.clone();
        let command_stop = stop.clone();
        let command_guilds = config.guilds.iter().map(|p| p.guild.clone()).collect();
        tasks
            .spawn("module_command_publication", async move {
                command_runtime::run(command_host, command_guilds, command_stop)
                    .await
                    .map_err(|_| TaskError)
            })
            .map_err(|_| Error::new(ErrorCode::Cancelled))?;
    }
    let maintenance_modules = host.modules.clone();
    let maintenance_stop = stop.clone();
    let maintenance_guilds: Vec<_> = config.guilds.iter().map(|p| p.guild.clone()).collect();
    tasks.spawn("module_maintenance", async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Restoration finishes before the first maintenance event.
            interval.tick().await;
            loop {
                tokio::select! { biased;
                    _ = maintenance_stop.cancelled() => return Ok(()),
                    _ = interval.tick() => {}
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| TaskError)?.as_millis() as u64;
                for guild in &maintenance_guilds {
                    let event = GuildEvent {
                        id: format!("maintenance:{now}"),
                        kind: GuildEventKind::Maintenance,
                        occurred_at_ms: now,
                        origin: GuildEventOrigin::Oracle,
                        subject_id: None, actor_id: None, related_id: None,
                    };
                    tokio::select! { biased;
                        _ = maintenance_stop.cancelled() => return Ok(()),
                        result = maintenance_modules.deliver_event(guild, event) => {
                            if let Err(error) = result {
                                tracing::debug!(guild = %guild, error = ?error.code, "module maintenance unavailable");
                            }
                        }
                    }
                }
            }
        }).map_err(|_| Error::new(ErrorCode::Cancelled))?;
    output(
        serde_json::json!({"event":"ready","discord_connected":gateway.is_some(),"status":host.core.status(&PolicyContext::LocalOperator,None).await?}),
    )?;
    let slots = Arc::new(tokio::sync::Semaphore::new(16));
    loop {
        tokio::select! { biased;
            _ = stop.cancelled() => return Err(Error::new(ErrorCode::Io)),
            _ = interrupt.recv() => return Ok(()),
            _ = term.recv() => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| Error::with_source(ErrorCode::Io, e))?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let host = host.clone();
                let cancel = stop.clone();
                tasks.spawn("local_control", async move {
                    let _permit = permit;
                    let work = control::serve_connection(&host, stream);
                    tokio::select! {
                        _ = cancel.cancelled() => Ok(()),
                        result = work => result,
                    }
                }).map_err(|_| Error::new(ErrorCode::Cancelled))?;
            }
        }
    }
}
