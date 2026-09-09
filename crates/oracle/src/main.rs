//! Host composition root. Feature modules enter only through runtime installation.
#![forbid(unsafe_code)]
mod cli_ops;
mod command_runtime;
mod config;
use clap::{Parser, Subcommand};
use config::Config;
use oracle_core::{
    tasks::{HostTasks, TaskError},
    *,
};
use oracle_modules::ModuleManager;
use oracle_operations::ingress::{HumanOperations, OperationRequest};
use oracle_storage::{PgTools, Storage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[derive(Parser)]
#[command(version, about = "Oracle: an empty, durable Discord framework host")]
struct Cli {
    #[arg(long, default_value = "oracle.json", global = true)]
    config: PathBuf,
    #[arg(long, default_value = "pg_dump", global = true)]
    pg_dump: PathBuf,
    #[arg(long, default_value = "pg_restore", global = true)]
    pg_restore: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Structure(cli_ops::StructureArgs),
    #[command(name = "module-config")]
    Configuration(cli_ops::ConfigurationArgs),
    /// Create a config and initialize/migrate an empty deployment. Never overwrites config.
    Init {
        #[arg(long)]
        postgres_url_env: Option<String>,
    },
    /// Boot the host and optional Discord Gateway; no Gemini key is required.
    Serve,
    Status {
        #[arg(long)]
        guild: Option<GuildId>,
    },
    Control {
        #[arg(long)]
        guild: GuildId,
        #[arg(value_enum)]
        action: Action,
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    Recovery {
        #[arg(long)]
        guild: GuildId,
        #[arg(long, default_value_t = 25)]
        limit: u32,
    },
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore into an absent SQLite file or empty PostgreSQL database, with mutations paused.
    Restore {
        #[arg(long)]
        backup: PathBuf,
    },
    /// Explicitly publish /oracle in configured guilds. Requires a configured Discord token.
    PublishCommands,
    /// Manage trusted native modules through a running host's private control socket.
    Module {
        #[command(subcommand)]
        command: ModuleCommand,
    },
}
#[derive(Subcommand)]
enum ModuleCommand {
    Install {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        trust_native: bool,
    },
    List,
    Load {
        #[arg(long)]
        digest: String,
    },
    /// Stop the old generation, migrate data and restore desired activations using an installed artifact.
    Upgrade {
        #[arg(long)]
        module: ModuleId,
        #[arg(long)]
        digest: String,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(0..=30000))]
        grace_ms: u64,
    },
    Activate {
        #[arg(long)]
        module: ModuleId,
        #[arg(long)]
        guild: GuildId,
        #[arg(long = "grant")]
        grants: Vec<String>,
        #[arg(long, default_value = "{}")]
        bindings: String,
    },
    Invoke {
        #[arg(long)]
        module: ModuleId,
        #[arg(long)]
        guild: GuildId,
        #[arg(long)]
        operation: String,
        #[arg(long, default_value = "{}")]
        input: String,
    },
    Deactivate {
        #[arg(long)]
        module: ModuleId,
        #[arg(long)]
        guild: GuildId,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(0..=30000))]
        grace_ms: u64,
    },
    Unload {
        #[arg(long)]
        module: ModuleId,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(0..=30000))]
        grace_ms: u64,
    },
    Health,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ModuleRequest {
    Install {
        source: PathBuf,
        trust_native: bool,
    },
    List {},
    Load {
        digest: String,
    },
    Upgrade {
        module: ModuleId,
        digest: String,
        grace_ms: u64,
    },
    Activate {
        module: ModuleId,
        guild: GuildId,
        grants: Vec<String>,
        bindings: BTreeMap<String, ModuleId>,
    },
    Invoke {
        module: ModuleId,
        guild: GuildId,
        operation: String,
        input: serde_json::Value,
    },
    Deactivate {
        module: ModuleId,
        guild: GuildId,
        grace_ms: u64,
    },
    Unload {
        module: ModuleId,
        grace_ms: u64,
    },
    Health {},
}
impl ModuleCommand {
    fn request(self) -> Result<ModuleRequest> {
        Ok(match self {
            Self::Install {
                source,
                trust_native,
            } => ModuleRequest::Install {
                source: std::path::absolute(source)
                    .map_err(|e| Error::with_source(ErrorCode::Io, e))?,
                trust_native,
            },
            Self::List => ModuleRequest::List {},
            Self::Load { digest } => ModuleRequest::Load { digest },
            Self::Upgrade {
                module,
                digest,
                grace_ms,
            } => ModuleRequest::Upgrade {
                module,
                digest,
                grace_ms,
            },
            Self::Activate {
                module,
                guild,
                grants,
                bindings,
            } => ModuleRequest::Activate {
                module,
                guild,
                grants,
                bindings: serde_json::from_str(&bindings)
                    .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?,
            },
            Self::Invoke {
                module,
                guild,
                operation,
                input,
            } => ModuleRequest::Invoke {
                module,
                guild,
                operation,
                input: serde_json::from_str(&input)
                    .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?,
            },
            Self::Deactivate {
                module,
                guild,
                grace_ms,
            } => ModuleRequest::Deactivate {
                module,
                guild,
                grace_ms,
            },
            Self::Unload { module, grace_ms } => ModuleRequest::Unload { module, grace_ms },
            Self::Health => ModuleRequest::Health {},
        })
    }
}
#[derive(Clone, clap::ValueEnum)]
enum Action {
    Pause,
    Resume,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    PublishCommands,
    Operation {
        guild: GuildId,
        request: OperationRequest,
    },
    Module {
        request: ModuleRequest,
    },
    Status {
        guild: Option<GuildId>,
    },
    Control {
        guild: GuildId,
        paused: bool,
        expected_revision: Option<u64>,
    },
    Recovery {
        guild: GuildId,
        limit: u32,
    },
    Backup {
        output: PathBuf,
    },
}
#[derive(Serialize, Deserialize)]
struct Response {
    // Explicit JSON null is a valid invocation result; only a missing field is absent.
    #[serde(default, deserialize_with = "present_result")]
    result: Option<serde_json::Value>,
    error: Option<ErrorCode>,
}
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}
struct Host {
    operations: std::sync::OnceLock<Arc<oracle_operations::executor::StructureExecutor>>,
    command_sync: std::sync::OnceLock<Arc<oracle_operations::commands::CommandReconciler>>,
    command_status: std::sync::Mutex<BTreeMap<GuildId, serde_json::Value>>,
    operation_tasks: HostTasks,
    storage: Arc<Storage>,
    core: Arc<CoreService>,
    modules: Arc<ModuleManager>,
    tools: PgTools,
    _state_lock: std::fs::File,
}
impl Host {
    async fn open(config: &Config, tools: PgTools) -> Result<Self> {
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
    async fn close(&self) -> Result<()> {
        self.operation_tasks.shutdown(Duration::from_secs(30)).await;
        let modules = self.modules.shutdown().await;
        let storage = self.storage.close().await;
        modules.and(storage)
    }
    async fn handle(&self, request: Request) -> Result<serde_json::Value> {
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
                    if grace_ms > 30000 {
                        return Err(Error::new(ErrorCode::InvalidInput));
                    }
                    self.modules
                        .upgrade(&module, &digest, Duration::from_millis(grace_ms))
                        .await?;
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
                    if grace_ms > 30000 {
                        return Err(Error::new(ErrorCode::InvalidInput));
                    }
                    value(
                        self.modules
                            .deactivate(
                                &PolicyContext::LocalOperator,
                                &module,
                                &guild,
                                Duration::from_millis(grace_ms),
                            )
                            .await?,
                    )
                }
                ModuleRequest::Unload { module, grace_ms } => {
                    if grace_ms > 30000 {
                        return Err(Error::new(ErrorCode::InvalidInput));
                    }
                    value(
                        self.modules
                            .unload(&module, Duration::from_millis(grace_ms))
                            .await?,
                    )
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
fn value(data: impl Serialize) -> Result<serde_json::Value> {
    serde_json::to_value(data).map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))
}
fn output(data: impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut bytes = serde_json::to_vec_pretty(&data)
        .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    bytes.push(b'\n');
    std::io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(|e| Error::with_source(ErrorCode::Io, e))
}
#[tokio::main]
async fn main() {
    // Arbitrary panic payloads may contain secrets. Health exposes task IDs/counts.
    std::panic::set_hook(Box::new(|_| {
        eprintln!("Oracle task panicked; payload redacted")
    }));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .json()
        .with_max_level(tracing::Level::WARN)
        .init();
    if let Err(error) = run(Cli::parse()).await {
        let _ = output(Response {
            result: None,
            error: Some(error.code),
        });
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> Result<()> {
    let tools = PgTools {
        pg_dump: cli.pg_dump,
        pg_restore: cli.pg_restore,
    };
    if let Command::Init { postgres_url_env } = cli.command {
        config::initialize(&cli.config, postgres_url_env)?;
        let config = Config::load(&cli.config)?;
        let host = Host::open(&config, tools).await?;
        let result = host.handle(Request::Status { guild: None }).await;
        let closed = host.close().await;
        return result.and_then(output).and(closed);
    }
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Serve => serve(config, tools).await,
        Command::Restore { backup } => {
            config.prepare_state_dir()?;
            let restored = Storage::restore(config.database()?, &backup, &tools).await?;
            restored
                .initialize_guilds(
                    &config
                        .guilds
                        .iter()
                        .map(|g| g.guild.clone())
                        .collect::<Vec<_>>(),
                )
                .await?;
            output(restored.status(None).await?)
        }
        Command::PublishCommands => {
            let stream = UnixStream::connect(config.socket())
                .await
                .map_err(|_| Error::new(ErrorCode::ModuleUnavailable))?;
            output(remote(stream, Request::PublishCommands).await?)
        }
        Command::Structure(args) => {
            let request = args.request()?;
            let stream = UnixStream::connect(config.socket())
                .await
                .map_err(|_| Error::new(ErrorCode::ModuleUnavailable))?;
            output(remote(stream, request).await?)
        }
        Command::Configuration(args) => {
            let request = args.request()?;
            let stream = UnixStream::connect(config.socket())
                .await
                .map_err(|_| Error::new(ErrorCode::ModuleUnavailable))?;
            output(remote(stream, request).await?)
        }
        Command::Module { command } => {
            let request = Request::Module {
                request: command.request()?,
            };
            let stream = UnixStream::connect(config.socket())
                .await
                .map_err(|e| Error::with_source(ErrorCode::ModuleUnavailable, e))?;
            output(remote(stream, request).await?)
        }
        command => {
            let request = match command {
                Command::Status { guild } => Request::Status { guild },
                Command::Control {
                    guild,
                    action,
                    expected_revision,
                } => Request::Control {
                    guild,
                    paused: matches!(action, Action::Pause),
                    expected_revision,
                },
                Command::Recovery { guild, limit } => Request::Recovery { guild, limit },
                Command::Backup { output } => Request::Backup {
                    output: std::path::absolute(output)
                        .map_err(|e| Error::with_source(ErrorCode::Io, e))?,
                },
                _ => unreachable!(),
            };
            match UnixStream::connect(config.socket()).await {
                Ok(stream) => output(remote(stream, request).await?),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    let host = Host::open(&config, tools).await?;
                    let result = host.handle(request).await;
                    let closed = host.close().await;
                    result.and_then(output).and(closed)
                }
                Err(e) => Err(Error::with_source(ErrorCode::Io, e)),
            }
        }
    }
}
const MAX_FRAME: u64 = 1024 * 1024;
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(stream: R) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    BufReader::new(stream.take(MAX_FRAME + 1))
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    if bytes.len() > MAX_FRAME as usize || bytes.last() != Some(&b'\n') {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(bytes)
}
async fn remote(mut stream: UnixStream, request: Request) -> Result<serde_json::Value> {
    let mut bytes =
        serde_json::to_vec(&request).map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME as usize {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let bytes = tokio::time::timeout(Duration::from_secs(300), read_frame(stream))
        .await
        .map_err(|_| Error::new(ErrorCode::Cancelled))??;
    let response: Response = serde_json::from_slice(&bytes)
        .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(code)) => Err(Error::new(code)),
        _ => Err(Error::new(ErrorCode::Integrity)),
    }
}
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
async fn serve(config: Config, tools: PgTools) -> Result<()> {
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
    let listener = UnixListener::bind(&socket).map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let socket_guard = SocketGuard(socket.clone());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let tasks = HostTasks::new();
    let stop = tasks.token();
    // Every startup/run error after spawning work converges on the joined cleanup below.
    let result:Result<()> = async {
        let health_host=host.clone();let health_stop=stop.clone();
        tasks.spawn("storage_health",async move {
            loop {
                tokio::select! { biased;
                    _=health_stop.cancelled()=>return Ok(()),
                    _=tokio::time::sleep(Duration::from_secs(2))=>{
                        if health_host.storage.status(None).await.is_err() { health_stop.cancel();return Err(TaskError); }
                    }
                }
            }
        }).map_err(|_|Error::new(ErrorCode::Cancelled))?;
        let module_host = host.clone(); let module_stop = stop.clone();
        tasks.spawn("module_health", async move {
            loop {
                tokio::select! { biased;
                    _ = module_stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                tokio::select! { biased;
                    _ = module_stop.cancelled() => return Ok(()),
                    outcome = module_host.modules.tick() => {
                        if outcome.is_err() { module_stop.cancel(); return Err(TaskError); }
                    }
                }
            }
        }).map_err(|_| Error::new(ErrorCode::Cancelled))?;
        let gateway=if let Some(token)=token {
            let gateway=Arc::new(oracle_discord::DiscordBootstrap::with_runtime(host.core.clone(),host.clone(),host.modules.clone(),config.discord.as_ref().ok_or_else(|| Error::new(ErrorCode::InvalidInput))?.intents.iter().cloned().collect())?);
            let running=gateway.clone();let cancel=stop.clone();
            tasks.spawn("discord_gateway",async move {
                let outcome=running.run_gateway(token,cancel.clone()).await;cancel.cancel();outcome.map_err(|_|TaskError)
            }).map_err(|_|Error::new(ErrorCode::Cancelled))?;
            tokio::select! { biased;
                _=term.recv()=>return Ok(()),
                _=interrupt.recv()=>return Ok(()),
                _=stop.cancelled()=>return Err(Error::new(ErrorCode::Io)),
                ready=gateway.wait_ready(Duration::from_secs(30))=>ready.map_err(|e|Error::with_source(ErrorCode::Io,e))?,
            }
            Some(gateway)
        } else {None};
        let restored = host.modules.restore_desired().await?;
        for (module, error) in &restored { tracing::warn!(module = %module, error = ?error, "module desired state was not restored"); }
        if gateway.is_some() {
            let command_host = host.clone();
            let command_stop = stop.clone();
            let command_guilds = config.guilds.iter().map(|p|p.guild.clone()).collect();
            tasks.spawn("module_command_publication", async move {
                command_runtime::run(command_host, command_guilds, command_stop).await.map_err(|_|TaskError)
            }).map_err(|_| Error::new(ErrorCode::Cancelled))?;
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
        output(serde_json::json!({"event":"ready","discord_connected":gateway.is_some(),"status":host.core.status(&PolicyContext::LocalOperator,None).await?}))?;
        let slots=Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            tokio::select! { biased;
                _=stop.cancelled()=>return Err(Error::new(ErrorCode::Io)),
                _=interrupt.recv()=>return Ok(()),
                _=term.recv()=>return Ok(()),
                accepted=listener.accept()=>{
                    let (mut stream,_)=accepted.map_err(|e|Error::with_source(ErrorCode::Io,e))?;
                    let Ok(permit)=slots.clone().try_acquire_owned() else {continue};
                    let host=host.clone();let cancel=stop.clone();
                    tasks.spawn("local_control",async move {
                        let _permit=permit;
                        let work=async {
                            let result=match tokio::time::timeout(Duration::from_secs(5),read_frame(&mut stream)).await {
                                Ok(Ok(bytes))=>match serde_json::from_slice(&bytes) {Ok(request)=>host.handle(request).await,Err(_)=>Err(Error::new(ErrorCode::InvalidInput))},
                                _=>Err(Error::new(ErrorCode::InvalidInput)),
                            };
                            let response=match result {Ok(result)=>Response{result:Some(result),error:None},Err(error)=>Response{result:None,error:Some(error.code)}};
                            let mut bytes=serde_json::to_vec(&response).map_err(|_|TaskError)?;bytes.push(b'\n');
                            if bytes.len() > MAX_FRAME as usize {
                                bytes = serde_json::to_vec(&Response { result: None, error: Some(ErrorCode::QuotaExceeded) }).map_err(|_| TaskError)?;
                                bytes.push(b'\n');
                            }
                            stream.write_all(&bytes).await.map_err(|_|TaskError)
                        };
                        tokio::select! {_=cancel.cancelled()=>Ok(()),result=work=>result}
                    }).map_err(|_|Error::new(ErrorCode::Cancelled))?;
                }
            }
        }
    }.await;
    drop(listener);
    let summary = tasks.shutdown(Duration::from_secs(15)).await;
    drop(socket_guard);
    stopped = Some(serde_json::json!({"event":"stopped","tasks":summary}));
    result
    }.await;
    let closed = host.close().await;
    if let Some(event) = stopped {
        output(event)?;
    }
    result.and(closed)
}
#[cfg(test)]
mod module_cli_tests {
    use super::*;
    #[test]
    fn module_success_reply_roundtrip() {
        for result in [
            serde_json::json!({"loaded":true,"digest":"a".repeat(64)}),
            serde_json::json!({"activated":true,"module":"sample.echo","guild":"123"}),
            serde_json::Value::Null,
        ] {
            let bytes = serde_json::to_vec(&Response {
                result: Some(result.clone()),
                error: None,
            })
            .unwrap();
            let reply: Response = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                reply.result,
                Some(result),
                "successful response was lost in socket serialization"
            );
            assert!(reply.error.is_none());
        }
    }
    #[test]
    fn upgrade_arguments_and_socket_contract() {
        let digest = "a".repeat(64);
        let arguments = [
            "oracle",
            "module",
            "upgrade",
            "--module",
            "sample.echo",
            "--digest",
            digest.as_str(),
        ];
        let cli = Cli::try_parse_from(arguments).unwrap();
        let Command::Module { command } = cli.command else {
            panic!("module command")
        };
        let request = Request::Module {
            request: command.request().unwrap(),
        };
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"command":"module","request":{"action":"upgrade","module":"sample.echo","digest":digest,"grace_ms":5000}})
        );
        assert!(serde_json::from_value::<Request>(wire).is_ok());
        let mut arguments = arguments.to_vec();
        arguments.extend(["--grace-ms", "30001"]);
        assert!(Cli::try_parse_from(arguments).is_err());
    }
    #[test]
    fn module_arguments_and_private_socket_contract() {
        let cli = Cli::try_parse_from([
            "oracle",
            "module",
            "activate",
            "--module",
            "sample.echo",
            "--guild",
            "123",
            "--grant",
            "documents.write",
            "--bindings",
            "{\"profile\":\"sample.profile\"}",
        ])
        .unwrap();
        let Command::Module { command } = cli.command else {
            panic!("module command")
        };
        let request = Request::Module {
            request: command.request().unwrap(),
        };
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["command"], "module");
        assert_eq!(wire["request"]["action"], "activate");
        assert_eq!(wire["request"]["bindings"]["profile"], "sample.profile");
        assert!(serde_json::from_value::<Request>(serde_json::json!({"command":"module","request":{"action":"health","unexpected":true}})).is_err());
        assert!(
            Cli::try_parse_from([
                "oracle",
                "module",
                "unload",
                "--module",
                "sample.echo",
                "--grace-ms",
                "30001"
            ])
            .is_err()
        );
        assert!(
            ModuleCommand::Invoke {
                module: ModuleId::new("sample.echo").unwrap(),
                guild: GuildId::new("123").unwrap(),
                operation: "echo".into(),
                input: "invalid json".into()
            }
            .request()
            .is_err()
        );
    }
}
