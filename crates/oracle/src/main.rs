//! Host composition root. Feature modules enter only through runtime installation.
#![forbid(unsafe_code)]
mod cli;
mod cli_ops;
mod command_runtime;
mod config;
mod control;
mod host;
mod human_ops;
use clap::Parser;
use cli::{Action, Cli, Command};
use config::Config;
use control::{Request, Response, remote};
use host::Host;
use oracle_core::{
    tasks::{HostTasks, TaskError},
    *,
};
use oracle_storage::{PgTools, Storage};
use serde::Serialize;
use std::str::FromStr;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::net::{UnixListener, UnixStream};

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
            output(control::send(&config.socket(), Request::PublishCommands).await?)
        }
        Command::Structure(args) => output(control::send(&config.socket(), args.request()?).await?),
        Command::Configuration(args) => {
            output(control::send(&config.socket(), args.request()?).await?)
        }
        Command::Module { command } => output(
            control::send(
                &config.socket(),
                Request::Module {
                    request: command.request()?,
                },
            )
            .await?,
        ),
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
                    let (stream,_)=accepted.map_err(|e|Error::with_source(ErrorCode::Io,e))?;
                    let Ok(permit)=slots.clone().try_acquire_owned() else {continue};
                    let host=host.clone();let cancel=stop.clone();
                    tasks.spawn("local_control",async move {
                        let _permit=permit;
                        let work = control::serve_connection(&host, stream);
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
    use crate::cli::ModuleCommand;
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
