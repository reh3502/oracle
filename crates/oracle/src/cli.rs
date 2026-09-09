//! Command-line arguments and conversion into local control requests.
use crate::{cli_ops, control::ModuleRequest};
use clap::{Parser, Subcommand};
use oracle_core::{Error, ErrorCode, GuildId, ModuleId, Result};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Oracle: an empty, durable Discord framework host")]
pub(crate) struct Cli {
    #[arg(long, default_value = "oracle.json", global = true)]
    pub(crate) config: PathBuf,
    #[arg(long, default_value = "pg_dump", global = true)]
    pub(crate) pg_dump: PathBuf,
    #[arg(long, default_value = "pg_restore", global = true)]
    pub(crate) pg_restore: PathBuf,
    #[command(subcommand)]
    pub(crate) command: Command,
}
#[derive(Subcommand)]
pub(crate) enum Command {
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
pub(crate) enum ModuleCommand {
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
impl ModuleCommand {
    pub(crate) fn request(self) -> Result<ModuleRequest> {
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
pub(crate) enum Action {
    Pause,
    Resume,
}
