use oracle_core::{Error, ErrorCode, GuildPolicy, Result};
use oracle_storage::DatabaseConfig;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub state_dir: PathBuf,
    pub database: Database,
    pub guilds: Vec<GuildPolicy>,
    pub discord: Option<Discord>,
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
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        let mut config: Self = serde_json::from_slice(&bytes)
            .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
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
fn validate_env(name: &str) -> Result<()> {
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
