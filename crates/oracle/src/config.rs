use oracle_core::{Error, ErrorCode, GuildPolicy, Result};
use oracle_storage::DatabaseConfig;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    pub version: u32,
    pub state_dir: PathBuf,
    pub database: Database,
    pub guilds: Vec<GuildPolicy>,
    pub discord: Option<Discord>,
    #[serde(default)]
    pub ai: Option<crate::ai::AiConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub module_runtime:
        BTreeMap<oracle_core::ModuleId, oracle_modules::runtime_settings::ModuleRuntimeSettings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_reads: Vec<MemberReads>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_mutations: Vec<MemberMutations>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_card_destinations: Vec<SharedCardDestination>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedCardDestination {
    pub guild: oracle_core::GuildId,
    pub module: oracle_core::ModuleId,
    pub destination: String,
    pub channel: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberReads {
    pub guild: oracle_core::GuildId,
    pub module: oracle_core::ModuleId,
    pub policy: oracle_core::member_read::MemberReadPolicy,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberMutations {
    pub guild: oracle_core::GuildId,
    pub module: oracle_core::ModuleId,
    pub policy: oracle_core::member_mutation::MemberMutationPolicy,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum Database {
    Sqlite { path: PathBuf },
    Postgres { url_env: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discord {
    pub token_env: String,
    #[serde(default = "default_intents")]
    pub intents: Vec<String>,
}
fn default_intents() -> Vec<String> {
    vec!["guilds".into()]
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        let mut config: Self = serde_json::from_slice(&bytes)
            .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
        config.source_path =
            Some(std::fs::canonicalize(path).map_err(|e| Error::with_source(ErrorCode::Io, e))?);
        if config.version != 1 || config.guilds.len() > 100 {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let mut ids = std::collections::BTreeSet::new();
        for policy in &config.guilds {
            if !ids.insert(&policy.guild) || policy.operators.len() > 100 {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut member_scopes = std::collections::BTreeSet::new();
        if config.shared_card_destinations.len() > 1024
            || config.member_mutations.len() > 1024
            || config.member_reads.len() > 1024
            || config.module_runtime.len() > 128
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        for reads in &config.member_reads {
            if !ids.contains(&reads.guild) || !member_scopes.insert((&reads.guild, &reads.module)) {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        let mut mutation_scopes = std::collections::BTreeSet::new();
        for mutations in &config.member_mutations {
            if !ids.contains(&mutations.guild)
                || !mutation_scopes.insert((&mutations.guild, &mutations.module))
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        let mut destinations = std::collections::BTreeSet::new();
        for binding in &config.shared_card_destinations {
            if oracle_core::GuildId::new(&binding.channel).is_err()
                || !ids.contains(&binding.guild)
                || binding.destination.is_empty()
                || binding.destination.len() > 64
                || !binding.destination.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-')
                })
                || !destinations.insert((&binding.guild, &binding.module, &binding.destination))
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        if config.state_dir.is_relative() {
            config.state_dir = parent.join(&config.state_dir);
        }
        match &mut config.database {
            Database::Sqlite { path } => {
                if path.is_relative() {
                    *path = parent.join(&*path);
                }
            }
            Database::Postgres { url_env } => validate_env(url_env)?,
        }
        if let Some(discord) = &config.discord {
            validate_env(&discord.token_env)?;
            let unique: std::collections::BTreeSet<_> = discord.intents.iter().collect();
            if !discord.intents.iter().any(|i| i == "guilds")
                || unique.len() != discord.intents.len()
                || discord
                    .intents
                    .iter()
                    .any(|i| !matches!(i.as_str(), "guilds" | "guild_members" | "guild_moderation"))
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        if let Some(ai) = &config.ai {
            ai.validate()?;
        }
        Ok(config)
    }
    pub fn database(&self) -> Result<DatabaseConfig> {
        Ok(match &self.database {
            Database::Sqlite { path } => DatabaseConfig::Sqlite { path: path.clone() },
            Database::Postgres { url_env } => DatabaseConfig::Postgres {
                url: secret_env(url_env)?,
            },
        })
    }
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("control.sock")
    }
    pub fn prepare_state_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        let m = std::fs::symlink_metadata(&self.state_dir)
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::with_source(ErrorCode::Io, e))
    }
}
pub(crate) fn validate_env(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        || name.as_bytes()[0].is_ascii_digit()
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}
pub fn secret_env(name: &str) -> Result<String> {
    validate_env(name)?;
    std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))
}
pub fn initialize(path: &Path, postgres_env: Option<String>) -> Result<()> {
    let config = Config {
        source_path: None,
        module_runtime: BTreeMap::new(),
        member_reads: vec![],
        member_mutations: vec![],
        shared_card_destinations: vec![],
        version: 1,
        state_dir: PathBuf::from("state"),
        database: match postgres_env {
            Some(url_env) => {
                validate_env(&url_env)?;
                Database::Postgres { url_env }
            }
            None => Database::Sqlite {
                path: PathBuf::from("state/oracle.sqlite"),
            },
        },
        guilds: vec![],
        discord: None,
        ai: None,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    file.write_all(
        &serde_json::to_vec_pretty(&config)
            .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?,
    )
    .and_then(|_| file.sync_all())
    .map_err(|e| Error::with_source(ErrorCode::Io, e))
}

#[cfg(test)]
#[path = "member_config_tests.rs"]
mod member_config_tests;
