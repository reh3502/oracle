//! Host composition root. Feature modules enter only through runtime installation.
#![forbid(unsafe_code)]
mod cli;
mod cli_ops;
mod command_runtime;
mod config;
mod control;
mod host;
mod human_ops;
mod server;
use clap::Parser;
use cli::{Action, Cli, Command};
use config::Config;
use control::{Request, Response, remote};
use host::Host;
use oracle_core::*;
use oracle_storage::{PgTools, Storage};
use serde::Serialize;
use server::serve;
use tokio::net::UnixStream;

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
