//! Concrete host repositories. Native backups include the complete database.
#![forbid(unsafe_code)]
use async_trait::async_trait;
use fs2::FileExt;
use oracle_contracts::*;
use oracle_core::Repository;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{
    Connection, PgConnection, PgPool, SqlitePool,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

const MIGRATION: &str = include_str!("../migrations/0001.sql");
const MIGRATION_SQLITE_2: &str = include_str!("../migrations/0002-sqlite.sql");
const MIGRATION_POSTGRES_2: &str = include_str!("../migrations/0002-postgres.sql");
const MIGRATION_SQLITE_3: &str = include_str!("../migrations/0003-sqlite.sql");
const MIGRATION_POSTGRES_3: &str = include_str!("../migrations/0003-postgres.sql");
const LOCK_KEY: i64 = 0x4f5241434c453031;
#[derive(Clone)]
pub enum DatabaseConfig {
    Sqlite { path: PathBuf },
    Postgres { url: String },
}
impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite { path } => f.debug_tuple("Sqlite").field(path).finish(),
            Self::Postgres { .. } => f.write_str("Postgres([redacted])"),
        }
    }
}
#[derive(Clone, Debug)]
pub struct PgTools {
    pub pg_dump: PathBuf,
    pub pg_restore: PathBuf,
}
impl Default for PgTools {
    fn default() -> Self {
        Self {
            pg_dump: "pg_dump".into(),
            pg_restore: "pg_restore".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: u32,
    pub backend: String,
    pub source_deployment: DeploymentId,
    pub database_sha256: String,
    pub migration_sha256: String,
    #[serde(default)]
    pub migrations: Vec<(i64, String)>,
    pub host_version: String,
    pub table_counts: BTreeMap<String, u64>,
}
enum Backend {
    Sqlite {
        pool: SqlitePool,
        lease: Mutex<Option<File>>,
    },
    Postgres {
        pool: PgPool,
        lease: AsyncMutex<Option<PgConnection>>,
    },
}
struct Inner {
    backend: Backend,
    config: DatabaseConfig,
    barrier: RwLock<()>,
    writer: Option<AsyncMutex<()>>,
    closed: AtomicBool,
}
#[derive(Clone)]
pub struct Storage {
    inner: Arc<Inner>,
}
fn err(code: ErrorCode) -> Error {
    Error::new(code)
}
fn db(error: sqlx::Error) -> Error {
    let code = if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        ErrorCode::Conflict
    } else {
        ErrorCode::StorageUnavailable
    };
    Error::with_source(code, error)
}
fn io(error: std::io::Error) -> Error {
    Error::with_source(ErrorCode::Io, error)
}
fn integrity(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::with_source(ErrorCode::Integrity, error)
}
fn revision(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| err(ErrorCode::InvalidInput))
}
fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn normalize(config: DatabaseConfig) -> Result<DatabaseConfig> {
    match config {
        DatabaseConfig::Sqlite { path } => {
            let path = if path.exists() {
                std::fs::canonicalize(path).map_err(io)?
            } else {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                std::fs::canonicalize(parent).map_err(io)?.join(
                    path.file_name()
                        .ok_or_else(|| err(ErrorCode::InvalidInput))?,
                )
            };
            Ok(DatabaseConfig::Sqlite { path })
        }
        other => Ok(other),
    }
}
fn restore_marker(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".oracle-restore-in-progress");
    PathBuf::from(value)
}
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().ok_or_else(|| err(ErrorCode::InvalidInput))?)
        .map_err(io)?
        .sync_all()
        .map_err(io)
}

fn sqlite_lock(path: &Path) -> Result<File> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".oracle-lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock))
        .map_err(io)?;
    file.try_lock_exclusive()
        .map_err(|e| Error::with_source(ErrorCode::AlreadyRunning, e))?;
    Ok(file)
}

async fn pg_health(inner: &Inner, conn: &mut PgConnection) -> Result<()> {
    let query = sqlx::query_as::<_,(bool,)>("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND granted AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1)")
        .bind(LOCK_KEY >> 32).bind(LOCK_KEY & 0xffffffff).fetch_one(conn);
    match tokio::time::timeout(Duration::from_secs(5), query).await {
        Ok(Ok((true,))) => Ok(()),
        result => {
            inner.closed.store(true, Ordering::SeqCst);
            Err(match result {
                Ok(Err(e)) => db(e),
                Err(e) => Error::with_source(ErrorCode::StorageUnavailable, e),
                _ => err(ErrorCode::AlreadyRunning),
            })
        }
    }
}

fn native_pg(program: &Path, connection: &str) -> Result<tokio::process::Command> {
    use std::str::FromStr;
    let options = sqlx::postgres::PgConnectOptions::from_str(connection).map_err(db)?;
    let uri =
        url::Url::parse(connection).map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    let mut command = tokio::process::Command::new(program);
    command
        .kill_on_drop(true)
        .env_remove("PGHOSTADDR")
        .env_remove("PGSERVICE")
        .env(
            "PGHOST",
            options
                .get_socket()
                .map(|s| s.as_os_str())
                .unwrap_or_else(|| std::ffi::OsStr::new(options.get_host())),
        )
        .env("PGPORT", options.get_port().to_string())
        .env("PGUSER", options.get_username())
        .env(
            "PGDATABASE",
            options.get_database().unwrap_or(options.get_username()),
        )
        .env(
            "PGSSLMODE",
            match options.get_ssl_mode() {
                sqlx::postgres::PgSslMode::Disable => "disable",
                sqlx::postgres::PgSslMode::Allow => "allow",
                sqlx::postgres::PgSslMode::Prefer => "prefer",
                sqlx::postgres::PgSslMode::Require => "require",
                sqlx::postgres::PgSslMode::VerifyCa => "verify-ca",
                sqlx::postgres::PgSslMode::VerifyFull => "verify-full",
            },
        )
        .env("PGCONNECT_TIMEOUT", "5");
    if let Some(password) = uri.password() {
        let decoded = percent_encoding::percent_decode_str(password)
            .decode_utf8()
            .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
        command.env("PGPASSWORD", decoded.as_ref());
    }
    for (name, value) in uri.query_pairs() {
        match name.as_ref() {
            "password" => {
                command.env("PGPASSWORD", value.as_ref());
            }
            "sslrootcert" | "ssl-root-cert" | "ssl-ca" => {
                command.env("PGSSLROOTCERT", value.as_ref());
            }
            "sslcert" | "ssl-cert" => {
                command.env("PGSSLCERT", value.as_ref());
            }
            "sslkey" | "ssl-key" => {
                command.env("PGSSLKEY", value.as_ref());
            }
            _ => {}
        }
    }
    if let Some(options) = options.get_options() {
        command.env("PGOPTIONS", options);
    }
    Ok(command)
}

macro_rules! rows {($store:expr,$ty:ty,$sql:expr $(,$bind:expr)* $(,)?)=>{{match &$store.inner.backend{Backend::Sqlite{pool,..}=>sqlx::query_as::<_,$ty>($sql)$(.bind($bind))*.fetch_all(pool).await.map_err(db)?,Backend::Postgres{pool,..}=>sqlx::query_as::<_,$ty>($sql)$(.bind($bind))*.fetch_all(pool).await.map_err(db)?}}};}
macro_rules! write_tx {($store:expr,$tx:ident,$body:block)=>{{
 let _barrier=$store.inner.barrier.read().await;$store.ensure_open()?;
 let _writer=match &$store.inner.writer{Some(lock)=>Some(lock.lock().await),None=>None};
 match &$store.inner.backend{
 Backend::Sqlite{pool,..}=>{let mut $tx=pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;let result:Result<_>=async $body.await;match result{Ok(value)=>{$tx.commit().await.map_err(db)?;Ok(value)},Err(e)=>{$tx.rollback().await.map_err(db)?;Err(e)}}},
 Backend::Postgres{lease,..}=>{let mut owner=lease.lock().await;let conn=owner.as_mut().ok_or_else(||err(ErrorCode::StorageUnavailable))?;pg_health(&$store.inner,conn).await?;let mut $tx=conn.begin().await.map_err(db)?;let result:Result<_>=async $body.await;match result{Ok(value)=>{$tx.commit().await.map_err(db)?;Ok(value)},Err(e)=>{$tx.rollback().await.map_err(db)?;Err(e)}}}
 }
}};}
impl Storage {
    pub async fn open(config: DatabaseConfig) -> Result<Self> {
        Self::open_internal(config, None).await
    }
    pub async fn open_with_options(
        config: DatabaseConfig,
        tools: &PgTools,
        migration_backup_root: &Path,
    ) -> Result<Self> {
        Self::open_internal(config, Some((tools, migration_backup_root))).await
    }
    async fn open_internal(
        config: DatabaseConfig,
        options: Option<(&PgTools, &Path)>,
    ) -> Result<Self> {
        let store = Self::connect(normalize(config)?, None).await?;
        store.ensure_not_restoring().await?;
        store.migrate_base().await?;
        let version = store.schema_version().await?;
        if version < 3 {
            let defaults = PgTools::default();
            let default_root;
            let (tools, root) = match options {
                Some(value) => value,
                None => match &store.inner.config {
                    DatabaseConfig::Sqlite { path } => {
                        default_root = path
                            .parent()
                            .ok_or_else(|| err(ErrorCode::Backup))?
                            .join("oracle-migration-backups");
                        (&defaults, default_root.as_path())
                    }
                    DatabaseConfig::Postgres { .. } => return Err(err(ErrorCode::Backup)),
                },
            };
            std::fs::create_dir_all(root).map_err(io)?;
            let backup = root.join(format!("schema{version}-{}", uuid::Uuid::new_v4()));
            store.backup(&backup, tools).await?;
            if version == 1 {
                store.migrate_second().await?;
            }
            store.migrate_third().await?;
        }
        store.verify_integrity().await?;
        store.startup_recovery(false).await?;
        Ok(store)
    }
    fn second_migration(&self) -> &'static str {
        match self.inner.backend {
            Backend::Sqlite { .. } => MIGRATION_SQLITE_2,
            Backend::Postgres { .. } => MIGRATION_POSTGRES_2,
        }
    }
    fn third_migration(&self) -> &'static str {
        match self.inner.backend {
            Backend::Sqlite { .. } => MIGRATION_SQLITE_3,
            Backend::Postgres { .. } => MIGRATION_POSTGRES_3,
        }
    }
    async fn migrate_third(&self) -> Result<()> {
        write_tx!(self, tx, {
            sqlx::raw_sql(self.third_migration())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(3,$1)")
                .bind(checksum(self.third_migration().as_bytes()))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            Ok(())
        })
    }
    fn expected_migrations(&self) -> Vec<(i64, String)> {
        vec![
            (1, checksum(MIGRATION.as_bytes())),
            (2, checksum(self.second_migration().as_bytes())),
            (3, checksum(self.third_migration().as_bytes())),
        ]
    }
    async fn schema_version(&self) -> Result<i64> {
        let migrations = rows!(
            self,
            (i64, String),
            "SELECT version,checksum FROM oracle_migrations ORDER BY version"
        );
        let expected = self.expected_migrations();
        if migrations == expected {
            Ok(3)
        } else if migrations == expected[..2] {
            Ok(2)
        } else if migrations == expected[..1] {
            Ok(1)
        } else {
            Err(err(ErrorCode::MigrationMismatch))
        }
    }
    async fn migrate_second(&self) -> Result<()> {
        write_tx!(self, tx, {
            sqlx::raw_sql(self.second_migration())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(2,$1)")
                .bind(checksum(self.second_migration().as_bytes()))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            Ok(())
        })
    }
    async fn connect(config: DatabaseConfig, sqlite_lease: Option<File>) -> Result<Self> {
        let backend = match &config {
            DatabaseConfig::Sqlite { path } => {
                let lease = match sqlite_lease {
                    Some(file) => file,
                    None => sqlite_lock(path)?,
                };
                let options = SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(sqlx::sqlite::SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(5));
                let pool = SqlitePoolOptions::new()
                    .max_connections(4)
                    .min_connections(2)
                    .connect_with(options)
                    .await
                    .map_err(db)?;
                Backend::Sqlite {
                    pool,
                    lease: Mutex::new(Some(lease)),
                }
            }
            DatabaseConfig::Postgres { url } => {
                let mut lease =
                    tokio::time::timeout(Duration::from_secs(5), PgConnection::connect(url))
                        .await
                        .map_err(|e| Error::with_source(ErrorCode::StorageUnavailable, e))?
                        .map_err(db)?;
                sqlx::raw_sql("SET search_path TO public; SET statement_timeout TO '10s'; SET synchronous_commit TO on").execute(&mut lease).await.map_err(db)?;
                let locked: (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
                    .bind(LOCK_KEY)
                    .fetch_one(&mut lease)
                    .await
                    .map_err(db)?;
                if !locked.0 {
                    return Err(err(ErrorCode::AlreadyRunning));
                }
                let pool = PgPoolOptions::new()
                    .max_connections(5)
                    .after_connect(|conn, _| {
                        Box::pin(async move {
                            sqlx::query("SET search_path TO public")
                                .execute(&mut *conn)
                                .await?;
                            sqlx::query("SET statement_timeout TO '10s'")
                                .execute(conn)
                                .await?;
                            Ok(())
                        })
                    })
                    .connect(url)
                    .await
                    .map_err(db)?;
                Backend::Postgres {
                    pool,
                    lease: AsyncMutex::new(Some(lease)),
                }
            }
        };
        let writer = matches!(backend, Backend::Sqlite { .. }).then(|| AsyncMutex::new(()));
        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                config,
                barrier: RwLock::new(()),
                writer,
                closed: AtomicBool::new(false),
            }),
        })
    }
    fn ensure_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            Err(err(ErrorCode::StorageUnavailable))
        } else {
            Ok(())
        }
    }
    async fn ensure_healthy(&self) -> Result<()> {
        self.ensure_open()?;
        if let Backend::Postgres { lease, .. } = &self.inner.backend {
            let mut guard = lease.lock().await;
            let conn = guard
                .as_mut()
                .ok_or_else(|| err(ErrorCode::StorageUnavailable))?;
            pg_health(&self.inner, conn).await?;
        }
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        let _exclusive = self.inner.barrier.write().await;
        self.inner.closed.store(true, Ordering::SeqCst);
        match &self.inner.backend {
            Backend::Sqlite { pool, lease } => {
                pool.close().await;
                if let Some(file) = lease.lock().unwrap().take() {
                    FileExt::unlock(&file).map_err(io)?;
                }
            }
            Backend::Postgres { pool, lease } => {
                pool.close().await;
                if let Some(conn) = lease.lock().await.take() {
                    conn.close().await.map_err(db)?;
                }
            }
        }
        Ok(())
    }
    async fn ensure_not_restoring(&self) -> Result<()> {
        match &self.inner.config {
            DatabaseConfig::Sqlite { path } => {
                if restore_marker(path).exists() {
                    return Err(err(ErrorCode::RecoveryRequired));
                }
            }
            DatabaseConfig::Postgres { .. } => {
                let found = rows!(
                    self,
                    (Option<String>,),
                    "SELECT to_regclass('public.oracle_restore_in_progress')::text"
                );
                if found[0].0.is_some() {
                    return Err(err(ErrorCode::RecoveryRequired));
                }
            }
        }
        Ok(())
    }
    async fn migrate_base(&self) -> Result<()> {
        write_tx!(self, tx, {
            sqlx::query("CREATE TABLE IF NOT EXISTS oracle_migrations(version BIGINT PRIMARY KEY,checksum TEXT NOT NULL)").execute(&mut *tx).await.map_err(db)?;
            let migrations: Vec<(i64, String)> =
                sqlx::query_as("SELECT version,checksum FROM oracle_migrations ORDER BY version")
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db)?;
            if !migrations.is_empty()
                && migrations != self.expected_migrations()[..1]
                && migrations != self.expected_migrations()[..2]
                && migrations != self.expected_migrations()
            {
                return Err(err(ErrorCode::MigrationMismatch));
            }
            if migrations.is_empty() {
                sqlx::raw_sql(MIGRATION)
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(1,$1)")
                    .bind(checksum(MIGRATION.as_bytes()))
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("INSERT INTO oracle_deployment(singleton,id,restored) VALUES(1,$1,0)")
                    .bind(DeploymentId::generate().as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::raw_sql(self.second_migration())
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(2,$1)")
                    .bind(checksum(self.second_migration().as_bytes()))
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::raw_sql(self.third_migration())
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(3,$1)")
                    .bind(checksum(self.third_migration().as_bytes()))
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
            }
            Ok(())
        })
    }
    async fn startup_recovery(&self, restored: bool) -> Result<()> {
        write_tx!(self, tx, {
            if restored {
                sqlx::query("UPDATE oracle_module_desired SET loaded=0")
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("UPDATE oracle_deployment SET id=$1,restored=1 WHERE singleton=1")
                    .bind(DeploymentId::generate().as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("UPDATE oracle_guilds SET paused=1,revision=revision+1")
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                sqlx::query("UPDATE oracle_effects SET state='unknown',revision=revision+1 WHERE state IN('prepared','sent')").execute(&mut *tx).await.map_err(db)?;
            } else {
                sqlx::query("UPDATE oracle_effects SET state='unknown',revision=revision+1 WHERE state='sent'").execute(&mut *tx).await.map_err(db)?;
            }
            sqlx::query("UPDATE oracle_operations SET state='recovery_required',revision=revision+1 WHERE state='running' OR (state!='recovery_required' AND EXISTS(SELECT 1 FROM oracle_effects e WHERE e.guild=oracle_operations.guild AND e.operation=oracle_operations.id AND e.state='unknown'))").execute(&mut *tx).await.map_err(db)?;
            Ok(())
        })
    }
    pub async fn initialize_guilds(&self, guilds: &[GuildId]) -> Result<()> {
        if guilds.len() > 1000 {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            for guild in guilds {
                sqlx::query("INSERT INTO oracle_guilds(id,paused,revision) SELECT $1,restored,1 FROM oracle_deployment WHERE singleton=1 ON CONFLICT DO NOTHING").bind(guild.as_str()).execute(&mut *tx).await.map_err(db)?;
            }
            Ok(())
        })
    }
}
type EffectRow = (String, String, String, String, String, i64, Option<String>);
fn decode_effect(row: EffectRow) -> Result<Effect> {
    let (id, operation, guild, purpose, state, revision, receipt) = row;
    let state = match state.as_str() {
        "prepared" => EffectState::Prepared,
        "sent" => EffectState::Sent,
        "verified" => EffectState::Verified,
        "unknown" => EffectState::Unknown,
        "failed" => EffectState::Failed,
        _ => return Err(err(ErrorCode::Integrity)),
    };
    Ok(Effect {
        id: EffectId::new(id)?,
        operation: OperationId::new(operation)?,
        guild: GuildId::new(guild)?,
        purpose,
        state,
        revision: u64::try_from(revision).map_err(integrity)?,
        receipt: receipt
            .map(|v| serde_json::from_str(&v).map_err(integrity))
            .transpose()?,
    })
}
#[async_trait]
impl Repository for Storage {
    async fn status(&self, guild: Option<&GuildId>) -> Result<Status> {
        let _barrier = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let deployment = rows!(
            self,
            (String,),
            "SELECT id FROM oracle_deployment WHERE singleton=1"
        )
        .pop()
        .ok_or_else(|| err(ErrorCode::Integrity))?;
        let guild_rows = match guild {
            Some(g) => rows!(
                self,
                (String, i64, i64),
                "SELECT id,paused,revision FROM oracle_guilds WHERE id=$1",
                g.as_str()
            ),
            None => rows!(
                self,
                (String, i64, i64),
                "SELECT id,paused,revision FROM oracle_guilds ORDER BY id LIMIT 1001"
            ),
        };
        if guild_rows.len() > 1000 {
            return Err(err(ErrorCode::InvalidInput));
        }
        let guilds = guild_rows
            .into_iter()
            .map(|(id, p, r)| {
                Ok(GuildState {
                    guild: GuildId::new(id)?,
                    paused: p != 0,
                    revision: u64::try_from(r).map_err(integrity)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let count = match guild {
            Some(g) => rows!(
                self,
                (i64,),
                "SELECT COUNT(*) FROM oracle_operations WHERE guild=$1 AND state='recovery_required'",
                g.as_str()
            ),
            None => rows!(
                self,
                (i64,),
                "SELECT COUNT(*) FROM oracle_operations WHERE state='recovery_required'"
            ),
        };
        Ok(Status {
            deployment: DeploymentId::new(deployment.0)?,
            guilds,
            modules_loaded: 0,
            recovery_required: count[0].0 as u64,
            ai_available: false,
        })
    }
    async fn set_paused(
        &self,
        guild: &GuildId,
        paused: bool,
        expected_revision: u64,
        actor: &str,
        operation: &OperationId,
    ) -> Result<ControlReceipt> {
        let expected = revision(expected_revision)?;
        if actor.is_empty() || actor.len() > 256 {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let affected = if expected == 0 {
                sqlx::query("INSERT INTO oracle_guilds(id,paused,revision) VALUES($1,$2,1) ON CONFLICT DO NOTHING").bind(guild.as_str()).bind(i64::from(paused)).execute(&mut *tx).await.map_err(db)?.rows_affected()
            } else {
                sqlx::query("UPDATE oracle_guilds SET paused=$2,revision=revision+1 WHERE id=$1 AND revision=$3").bind(guild.as_str()).bind(i64::from(paused)).bind(expected).execute(&mut *tx).await.map_err(db)?.rows_affected()
            };
            if affected != 1 {
                return Err(err(ErrorCode::Conflict));
            }
            sqlx::query("INSERT INTO oracle_operations(id,guild,actor,state,revision) VALUES($1,$2,$3,'succeeded',1)").bind(operation.as_str()).bind(guild.as_str()).bind(actor).execute(&mut *tx).await.map_err(db)?;
            Ok(ControlReceipt {
                operation: operation.clone(),
                guild: GuildState {
                    guild: guild.clone(),
                    paused,
                    revision: expected_revision + 1,
                },
            })
        })
    }
    async fn begin_operation(&self, operation: &Operation) -> Result<()> {
        if operation.state != OperationState::Running
            || operation.revision != 0
            || operation.actor.is_empty()
            || operation.actor.len() > 256
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let guild: Option<(i64,)> =
                sqlx::query_as("SELECT paused FROM oracle_guilds WHERE id=$1")
                    .bind(operation.guild.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            if guild != Some((0,)) {
                return Err(err(ErrorCode::RecoveryRequired));
            }
            sqlx::query("INSERT INTO oracle_operations(id,guild,actor,state,revision) VALUES($1,$2,$3,'running',1)").bind(operation.id.as_str()).bind(operation.guild.as_str()).bind(&operation.actor).execute(&mut *tx).await.map_err(db)?;
            Ok(())
        })
    }
    async fn reserve_effect(&self, effect: &Effect) -> Result<Effect> {
        if effect.state != EffectState::Prepared
            || effect.revision != 0
            || effect.receipt.is_some()
            || effect.purpose.is_empty()
            || effect.purpose.len() > 256
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let operation: Option<(String,)> =
                sqlx::query_as("SELECT state FROM oracle_operations WHERE id=$1 AND guild=$2")
                    .bind(effect.operation.as_str())
                    .bind(effect.guild.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            if operation != Some(("running".into(),)) {
                return Err(err(ErrorCode::NotFound));
            }
            sqlx::query("INSERT INTO oracle_effects(id,operation,guild,purpose,state,revision,receipt) VALUES($1,$2,$3,$4,'prepared',1,NULL) ON CONFLICT(guild,purpose) DO NOTHING").bind(effect.id.as_str()).bind(effect.operation.as_str()).bind(effect.guild.as_str()).bind(&effect.purpose).execute(&mut *tx).await.map_err(db)?;
            let row:EffectRow=sqlx::query_as("SELECT id,operation,guild,purpose,state,revision,receipt FROM oracle_effects WHERE guild=$1 AND purpose=$2").bind(effect.guild.as_str()).bind(&effect.purpose).fetch_one(&mut *tx).await.map_err(db)?;
            decode_effect(row)
        })
    }
    async fn effect(&self, guild: &GuildId, id: &EffectId) -> Result<Effect> {
        let _barrier = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let row=rows!(self,EffectRow,"SELECT id,operation,guild,purpose,state,revision,receipt FROM oracle_effects WHERE guild=$1 AND id=$2",guild.as_str(),id.as_str()).pop().ok_or_else(||err(ErrorCode::NotFound))?;
        decode_effect(row)
    }
    async fn transition_effect(
        &self,
        guild: &GuildId,
        id: &EffectId,
        expected_revision: u64,
        next: EffectState,
        receipt: Option<Value>,
    ) -> Result<Effect> {
        let expected = revision(expected_revision)?;
        let receipt = receipt
            .map(|v| serde_json::to_string(&v).map_err(integrity))
            .transpose()?;
        if receipt.as_ref().is_some_and(|v| v.len() > 65536) {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let row:Option<EffectRow>=sqlx::query_as("SELECT id,operation,guild,purpose,state,revision,receipt FROM oracle_effects WHERE guild=$1 AND id=$2").bind(guild.as_str()).bind(id.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
            let old = decode_effect(row.ok_or_else(|| err(ErrorCode::NotFound))?)?;
            let allowed = matches!(
                (&old.state, &next),
                (
                    EffectState::Prepared,
                    EffectState::Sent | EffectState::Failed
                ) | (
                    EffectState::Sent,
                    EffectState::Unknown | EffectState::Verified | EffectState::Failed
                ) | (
                    EffectState::Unknown,
                    EffectState::Verified | EffectState::Failed
                )
            );
            if !allowed || old.revision != expected_revision {
                return Err(err(ErrorCode::Conflict));
            }
            if matches!(next, EffectState::Verified | EffectState::Failed) && receipt.is_none() {
                return Err(err(ErrorCode::InvalidInput));
            }
            if next == EffectState::Sent {
                let eligible:Option<(i64,)>=sqlx::query_as("SELECT g.paused FROM oracle_guilds g JOIN oracle_operations o ON o.guild=g.id WHERE o.id=$1 AND g.id=$2 AND o.state='running'").bind(old.operation.as_str()).bind(guild.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
                if eligible != Some((0,)) {
                    return Err(err(ErrorCode::RecoveryRequired));
                }
            }
            let count=sqlx::query("UPDATE oracle_effects SET state=$3,receipt=$4,revision=revision+1 WHERE guild=$1 AND id=$2 AND revision=$5").bind(guild.as_str()).bind(id.as_str()).bind(next.as_str()).bind(&receipt).bind(expected).execute(&mut *tx).await.map_err(db)?.rows_affected();
            if count != 1 {
                return Err(err(ErrorCode::Conflict));
            }
            let row:EffectRow=sqlx::query_as("SELECT id,operation,guild,purpose,state,revision,receipt FROM oracle_effects WHERE guild=$1 AND id=$2").bind(guild.as_str()).bind(id.as_str()).fetch_one(&mut *tx).await.map_err(db)?;
            decode_effect(row)
        })
    }
    async fn finish_operation(
        &self,
        guild: &GuildId,
        id: &OperationId,
        state: OperationState,
    ) -> Result<()> {
        if state == OperationState::Running {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            if state == OperationState::Failed {
                let unresolved:(i64,)=sqlx::query_as("SELECT COUNT(*) FROM oracle_effects WHERE guild=$1 AND operation=$2 AND state IN('prepared','sent','unknown')").bind(guild.as_str()).bind(id.as_str()).fetch_one(&mut *tx).await.map_err(db)?;
                if unresolved.0 != 0 {
                    return Err(err(ErrorCode::RecoveryRequired));
                }
            }
            if state == OperationState::Succeeded {
                let count:(i64,)=sqlx::query_as("SELECT COUNT(*) FROM oracle_effects WHERE guild=$1 AND operation=$2 AND state!='verified'").bind(guild.as_str()).bind(id.as_str()).fetch_one(&mut *tx).await.map_err(db)?;
                if count.0 != 0 {
                    return Err(err(ErrorCode::RecoveryRequired));
                }
            }
            let count=sqlx::query("UPDATE oracle_operations SET state=$3,revision=revision+1 WHERE guild=$1 AND id=$2 AND state IN('running','recovery_required')").bind(guild.as_str()).bind(id.as_str()).bind(state.as_str()).execute(&mut *tx).await.map_err(db)?.rows_affected();
            if count != 1 {
                return Err(err(ErrorCode::Conflict));
            }
            Ok(())
        })
    }
    async fn recovery(&self, guild: &GuildId, limit: u32) -> Result<Vec<Effect>> {
        if limit == 0 || limit > 100 {
            return Err(err(ErrorCode::InvalidInput));
        }
        let _barrier = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        rows!(self,EffectRow,"SELECT e.id,e.operation,e.guild,e.purpose,e.state,e.revision,e.receipt FROM oracle_effects e JOIN oracle_operations o ON o.guild=e.guild AND o.id=e.operation WHERE e.guild=$1 AND (e.state='unknown' OR (e.state='prepared' AND o.state='recovery_required')) ORDER BY e.id LIMIT $2",guild.as_str(),i64::from(limit)).into_iter().map(decode_effect).collect()
    }
}

impl Storage {
    fn backend_name(&self) -> &'static str {
        match self.inner.backend {
            Backend::Sqlite { .. } => "sqlite",
            Backend::Postgres { .. } => "postgres",
        }
    }
    async fn table_counts(&self) -> Result<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        match &self.inner.backend {
            Backend::Sqlite { pool, .. } => {
                let tables:Vec<(String,)>=sqlx::query_as("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").fetch_all(pool).await.map_err(db)?;
                for (name,) in tables {
                    let sql = format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\""));
                    let count: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                        .fetch_one(pool)
                        .await
                        .map_err(db)?;
                    counts.insert(name, count.0 as u64);
                }
            }
            Backend::Postgres { pool, .. } => {
                let tables:Vec<(String,String)>=sqlx::query_as("SELECT schemaname,tablename FROM pg_tables WHERE schemaname NOT IN('pg_catalog','information_schema') ORDER BY schemaname,tablename").fetch_all(pool).await.map_err(db)?;
                for (schema, name) in tables {
                    let sql = format!(
                        "SELECT COUNT(*) FROM \"{}\".\"{}\"",
                        schema.replace('"', "\"\""),
                        name.replace('"', "\"\"")
                    );
                    let count: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                        .fetch_one(pool)
                        .await
                        .map_err(db)?;
                    counts.insert(format!("{schema}.{name}"), count.0 as u64);
                }
            }
        }
        Ok(counts)
    }
    async fn verify_integrity(&self) -> Result<()> {
        self.schema_version().await?;
        if let Backend::Sqlite { pool, .. } = &self.inner.backend {
            let result: Vec<(String,)> = sqlx::query_as("PRAGMA integrity_check")
                .fetch_all(pool)
                .await
                .map_err(db)?;
            if result != vec![("ok".into(),)] {
                return Err(err(ErrorCode::Integrity));
            }
            let errors = sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(pool)
                .await
                .map_err(db)?;
            if !errors.is_empty() {
                return Err(err(ErrorCode::Integrity));
            }
        }
        let deployment = rows!(
            self,
            (String,),
            "SELECT id FROM oracle_deployment WHERE singleton=1"
        );
        if deployment.len() != 1 {
            return Err(err(ErrorCode::Integrity));
        }
        DeploymentId::new(deployment[0].0.clone())?;
        Ok(())
    }
    /// Consistent full native database snapshot; the barrier drains this host's writers.
    pub async fn backup(&self, directory: &Path, tools: &PgTools) -> Result<BackupManifest> {
        let _exclusive = self.inner.barrier.write().await;
        self.ensure_healthy().await?;
        self.verify_integrity().await?;
        std::fs::create_dir(directory).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(io)?;
        }
        let database = directory.join(if self.backend_name() == "sqlite" {
            "database.sqlite"
        } else {
            "database.dump"
        });
        match &self.inner.backend {
            Backend::Sqlite { pool, .. } => {
                sqlx::query("VACUUM main INTO $1")
                    .bind(
                        database
                            .to_str()
                            .ok_or_else(|| err(ErrorCode::InvalidInput))?,
                    )
                    .execute(pool)
                    .await
                    .map_err(db)?;
            }
            Backend::Postgres { .. } => {
                let DatabaseConfig::Postgres { url } = &self.inner.config else {
                    unreachable!()
                };
                let status = native_pg(&tools.pg_dump, url)?
                    .args(["--format=custom", "--no-owner", "--no-privileges", "--file"])
                    .arg(&database)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map_err(io)?;
                if !status.success() {
                    return Err(err(ErrorCode::Backup));
                }
            }
        }
        self.ensure_healthy().await?;
        File::open(&database).map_err(io)?.sync_all().map_err(io)?;
        let deployment = rows!(
            self,
            (String,),
            "SELECT id FROM oracle_deployment WHERE singleton=1"
        )
        .pop()
        .ok_or_else(|| err(ErrorCode::Integrity))?;
        let manifest = BackupManifest {
            format: 2,
            backend: self.backend_name().into(),
            source_deployment: DeploymentId::new(deployment.0)?,
            database_sha256: file_hash(&database)?,
            migration_sha256: checksum(MIGRATION.as_bytes()),
            migrations: rows!(
                self,
                (i64, String),
                "SELECT version,checksum FROM oracle_migrations ORDER BY version"
            ),
            host_version: env!("CARGO_PKG_VERSION").into(),
            table_counts: self.table_counts().await?,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("manifest.json"))
            .map_err(io)?;
        file.write_all(&serde_json::to_vec_pretty(&manifest).map_err(integrity)?)
            .map_err(io)?;
        file.sync_all().map_err(io)?;
        File::open(directory).map_err(io)?.sync_all().map_err(io)?;
        Ok(manifest)
    }
    /// Restore only to a fresh target, with no host service published until it is
    /// integrity-checked, assigned a new identity, paused, and in recovery mode.
    pub async fn restore(
        config: DatabaseConfig,
        directory: &Path,
        tools: &PgTools,
    ) -> Result<Self> {
        let config = normalize(config)?;
        let manifest_path = directory.join("manifest.json");
        if std::fs::metadata(&manifest_path).map_err(io)?.len() > 65536 {
            return Err(err(ErrorCode::Integrity));
        }
        let manifest: BackupManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).map_err(io)?)
                .map_err(integrity)?;
        let backend = match config {
            DatabaseConfig::Sqlite { .. } => "sqlite",
            DatabaseConfig::Postgres { .. } => "postgres",
        };
        if !matches!(manifest.format, 1 | 2)
            || manifest.backend != backend
            || manifest.migration_sha256 != checksum(MIGRATION.as_bytes())
        {
            return Err(err(ErrorCode::MigrationMismatch));
        }
        let database = directory.join(if backend == "sqlite" {
            "database.sqlite"
        } else {
            "database.dump"
        });
        if file_hash(&database)? != manifest.database_sha256 {
            return Err(err(ErrorCode::Integrity));
        }
        let store = match &config {
            DatabaseConfig::Sqlite { path } => {
                let lease = sqlite_lock(path)?;
                if path.exists() || restore_marker(path).exists() {
                    return Err(err(ErrorCode::Integrity));
                }
                let mut marker = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(restore_marker(path))
                    .map_err(io)?;
                marker
                    .write_all(b"Oracle restore in progress; do not boot this target\n")
                    .map_err(io)?;
                marker.sync_all().map_err(io)?;
                sync_parent(path)?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(io)?;
                std::io::copy(&mut File::open(&database).map_err(io)?, &mut output).map_err(io)?;
                output.sync_all().map_err(io)?;
                drop(output);
                Self::connect(config, Some(lease)).await?
            }
            DatabaseConfig::Postgres { url } => {
                let store = Self::connect(config.clone(), None).await?;
                if !store.table_counts().await?.is_empty() {
                    return Err(err(ErrorCode::Integrity));
                }
                write_tx!(&store, tx, {
                    sqlx::query("CREATE TABLE oracle_restore_in_progress(marker TEXT NOT NULL)")
                        .execute(&mut *tx)
                        .await
                        .map_err(db)?;
                    sqlx::query("INSERT INTO oracle_restore_in_progress(marker) VALUES('restore in progress')").execute(&mut *tx).await.map_err(db)?;
                    Ok(())
                })?;
                let status = native_pg(&tools.pg_restore, url)?
                    .args([
                        "--no-owner",
                        "--no-privileges",
                        "--exit-on-error",
                        "--single-transaction",
                        "--dbname",
                    ])
                    .arg(url_to_env_placeholder())
                    .arg(&database)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map_err(io)?;
                if !status.success() {
                    return Err(err(ErrorCode::Backup));
                }
                store
            }
        };
        store.verify_integrity().await?;
        let incoming_version = store.schema_version().await?;
        let actual_migrations = rows!(
            &store,
            (i64, String),
            "SELECT version,checksum FROM oracle_migrations ORDER BY version"
        );
        if (manifest.format == 1 && incoming_version != 1)
            || (manifest.format == 2 && manifest.migrations != actual_migrations)
        {
            return Err(err(ErrorCode::MigrationMismatch));
        }
        let id = rows!(
            &store,
            (String,),
            "SELECT id FROM oracle_deployment WHERE singleton=1"
        )
        .pop()
        .ok_or_else(|| err(ErrorCode::Integrity))?;
        if id.0 != manifest.source_deployment.as_str() {
            return Err(err(ErrorCode::Integrity));
        }
        let mut restored_counts = store.table_counts().await?;
        restored_counts.remove("public.oracle_restore_in_progress");
        if restored_counts != manifest.table_counts {
            return Err(err(ErrorCode::Integrity));
        }
        // The validated native source bundle is the pre-upgrade backup.
        if incoming_version == 1 {
            store.migrate_second().await?;
        }
        if incoming_version < 3 {
            store.migrate_third().await?;
        }
        store.startup_recovery(true).await?;
        store.verify_integrity().await?;
        match &store.inner.config {
            DatabaseConfig::Sqlite { path } => {
                std::fs::remove_file(restore_marker(path)).map_err(io)?;
                sync_parent(path)?;
            }
            DatabaseConfig::Postgres { .. } => {
                write_tx!(&store, tx, {
                    sqlx::query("DROP TABLE oracle_restore_in_progress")
                        .execute(&mut *tx)
                        .await
                        .map_err(db)?;
                    Ok(())
                })?;
            }
        }
        Ok(store)
    }
}
// libpq's empty conninfo uses PGDATABASE; the actual DSN never enters argv.
fn url_to_env_placeholder() -> &'static str {
    ""
}

#[cfg(test)]
mod module_tests;
mod modules;
#[cfg(test)]
mod workflow_tests;
mod workflows;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinSet;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("oracle-storage-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn operation(guild: &GuildId) -> Operation {
        Operation {
            id: OperationId::generate(),
            guild: guild.clone(),
            actor: "local-test".into(),
            state: OperationState::Running,
            revision: 0,
        }
    }
    async fn prepare(store: &Storage, guild: &GuildId, purpose: &str) -> Effect {
        let operation = operation(guild);
        store.begin_operation(&operation).await.unwrap();
        store
            .reserve_effect(&Effect {
                id: EffectId::generate(),
                operation: operation.id,
                guild: guild.clone(),
                purpose: purpose.into(),
                state: EffectState::Prepared,
                revision: 0,
                receipt: None,
            })
            .await
            .unwrap()
    }
    fn tools() -> PgTools {
        match std::env::var_os("ORACLE_TEST_PG_BIN") {
            Some(path) => PgTools {
                pg_dump: PathBuf::from(&path).join("pg_dump"),
                pg_restore: PathBuf::from(path).join("pg_restore"),
            },
            None => PgTools::default(),
        }
    }
    async fn failed_restore_guard(
        config: &DatabaseConfig,
        bundle: &Path,
        manifest: &BackupManifest,
        scratch: &Scratch,
    ) {
        for field in ["source", "counts"] {
            let mut altered = manifest.clone();
            if field == "source" {
                altered.source_deployment = DeploymentId::generate();
            } else {
                *altered.table_counts.values_mut().next().unwrap() += 1;
            }
            std::fs::write(
                bundle.join("manifest.json"),
                serde_json::to_vec(&altered).unwrap(),
            )
            .unwrap();
            let mut database_name = None;
            let target = match config {
                DatabaseConfig::Sqlite { .. } => DatabaseConfig::Sqlite {
                    path: scratch.0.join(format!("bad-{field}.sqlite")),
                },
                DatabaseConfig::Postgres { url } => {
                    let name = format!("oracle_guard_{}", uuid::Uuid::new_v4().simple());
                    let mut admin = PgConnection::connect(url).await.unwrap();
                    let create = format!("CREATE DATABASE {name}");
                    sqlx::query(sqlx::AssertSqlSafe(create.as_str()))
                        .execute(&mut admin)
                        .await
                        .unwrap();
                    admin.close().await.unwrap();
                    let mut destination = url::Url::parse(url).unwrap();
                    destination.set_path(&name);
                    let params: Vec<_> = destination
                        .query_pairs()
                        .filter(|(key, _)| key != "dbname")
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect();
                    destination.set_query(None);
                    destination.query_pairs_mut().extend_pairs(params);
                    database_name = Some(name);
                    DatabaseConfig::Postgres {
                        url: destination.to_string(),
                    }
                }
            };
            match Storage::restore(target.clone(), bundle, &tools()).await {
                Err(e) => assert_eq!(e.code, ErrorCode::Integrity),
                Ok(_) => panic!("invalid restore metadata accepted"),
            }
            let mut guarded = false;
            for _ in 0..20 {
                match Storage::open(target.clone()).await {
                    Err(e) if e.code == ErrorCode::RecoveryRequired => {
                        guarded = true;
                        break;
                    }
                    Err(e) if e.code == ErrorCode::AlreadyRunning => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(e) => panic!("unexpected failed-restore boot result: {e}"),
                    Ok(_) => panic!("failed restore booted source deployment"),
                }
            }
            assert!(guarded, "durable restore guard did not fence startup");
            if let (Some(name), DatabaseConfig::Postgres { url }) = (database_name, config) {
                let mut admin = PgConnection::connect(url).await.unwrap();
                let drop_db = format!("DROP DATABASE {name} WITH (FORCE)");
                sqlx::query(sqlx::AssertSqlSafe(drop_db.as_str()))
                    .execute(&mut admin)
                    .await
                    .unwrap();
                admin.close().await.unwrap();
            }
        }
        std::fs::write(
            bundle.join("manifest.json"),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn native_credentials_and_tls_are_environment_only() {
        let command=native_pg(Path::new("pg_dump"),"postgresql://admin:p%40ss%25word@example.test/source?sslmode=verify-full&sslrootcert=%2Fprivate%2Fca.pem").unwrap();
        let environment: BTreeMap<_, _> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(environment["PGPASSWORD"].as_deref(), Some("p@ss%word"));
        assert_eq!(environment["PGSSLMODE"].as_deref(), Some("verify-full"));
        assert_eq!(
            environment["PGSSLROOTCERT"].as_deref(),
            Some("/private/ca.pem")
        );
        assert_eq!(environment["PGDATABASE"].as_deref(), Some("source"));
        assert_eq!(command.as_std().get_args().count(), 0);
    }
    async fn contract(config: DatabaseConfig, destination: DatabaseConfig, scratch: &Scratch) {
        module_tests::upgrades(&config, &scratch.0, &tools())
            .await
            .unwrap();
        let guild = GuildId::new("123").unwrap();
        let other = GuildId::new("456").unwrap();
        let store = Storage::open(config.clone()).await.unwrap();
        store
            .initialize_guilds(&[guild.clone(), other.clone()])
            .await
            .unwrap();
        assert!(
            Storage::open(config.clone()).await.is_err(),
            "second host obtained ownership"
        );
        let store = module_tests::exercise(&config, store).await;
        let store = workflow_tests::exercise(&config, store).await;
        let before = store.status(None).await.unwrap();
        assert_eq!(before.modules_loaded, 0);
        assert!(!before.ai_available);
        assert_eq!(before.guilds.len(), 2);
        let mut tasks = JoinSet::new();
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        for _ in 0..16 {
            let store = store.clone();
            let guild = guild.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                store
                    .set_paused(&guild, true, 1, "local-test", &OperationId::generate())
                    .await
            });
        }
        let mut wins = 0;
        while let Some(outcome) = tasks.join_next().await {
            match outcome.unwrap() {
                Ok(receipt) => {
                    wins += 1;
                    assert_eq!(receipt.guild.revision, 2)
                }
                Err(e) => assert_eq!(e.code, ErrorCode::Conflict),
            }
        }
        assert_eq!(wins, 1);
        store
            .set_paused(&guild, false, 2, "local-test", &OperationId::generate())
            .await
            .unwrap();
        let effect = prepare(&store, &guild, "channel:one").await;
        assert_eq!(effect.revision, 1);
        assert_eq!(
            store.effect(&other, &effect.id).await.unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            store
                .transition_effect(&other, &effect.id, 1, EffectState::Sent, None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        let sent = store
            .transition_effect(&guild, &effect.id, 1, EffectState::Sent, None)
            .await
            .unwrap();
        assert_eq!(sent.revision, 2);
        assert_eq!(
            store
                .transition_effect(
                    &guild,
                    &effect.id,
                    1,
                    EffectState::Verified,
                    Some(serde_json::json!({}))
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            store
                .finish_operation(&guild, &effect.operation, OperationState::Succeeded)
                .await
                .unwrap_err()
                .code,
            ErrorCode::RecoveryRequired
        );
        store.close().await.unwrap();
        assert_eq!(
            store.status(None).await.unwrap_err().code,
            ErrorCode::StorageUnavailable
        );
        let store = Storage::open(config.clone()).await.unwrap();
        assert_eq!(
            store.status(None).await.unwrap().deployment,
            before.deployment
        );
        let unknown = store.effect(&guild, &effect.id).await.unwrap();
        assert_eq!(unknown.state, EffectState::Unknown);
        assert_eq!(unknown.revision, 3);
        assert_eq!(
            store.status(Some(&guild)).await.unwrap().recovery_required,
            1
        );
        assert_eq!(
            store
                .transition_effect(&guild, &effect.id, 3, EffectState::Sent, None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let replay = prepare(&store, &guild, "channel:one").await;
        assert_eq!(replay.id, effect.id);
        assert_eq!(replay.state, EffectState::Unknown);
        let prepared = prepare(&store, &guild, "channel:not-yet-sent").await;
        let in_flight = prepare(&store, &guild, "channel:in-flight").await;
        store
            .transition_effect(&guild, &in_flight.id, 1, EffectState::Sent, None)
            .await
            .unwrap();
        // Native snapshots must include every table, not just a hand-selected ledger export.
        match &store.inner.backend {
            Backend::Sqlite { pool, .. } => {
                sqlx::query("CREATE TABLE oracle_test_extra(value TEXT NOT NULL)")
                    .execute(pool)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO oracle_test_extra(value) VALUES('preserved')")
                    .execute(pool)
                    .await
                    .unwrap();
            }
            Backend::Postgres { pool, .. } => {
                sqlx::query("CREATE TABLE oracle_test_extra(value TEXT NOT NULL)")
                    .execute(pool)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO oracle_test_extra(value) VALUES('preserved')")
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
        let bundle = scratch.0.join("bundle");
        let manifest = store.backup(&bundle, &tools()).await.unwrap();
        assert_eq!(manifest.source_deployment, before.deployment);
        assert_eq!(manifest.table_counts.len(), 13);
        failed_restore_guard(&config, &bundle, &manifest, scratch).await;
        assert_eq!(
            store.effect(&guild, &prepared.id).await.unwrap().state,
            EffectState::Prepared,
            "backup changed source state"
        );
        let restored = Storage::restore(destination.clone(), &bundle, &tools())
            .await
            .unwrap();
        module_tests::restored(&restored).await;
        workflow_tests::restored(&restored).await;
        let status = restored.status(None).await.unwrap();
        assert_ne!(status.deployment, before.deployment);
        assert!(status.guilds.iter().all(|g| g.paused));
        assert_eq!(
            restored.effect(&guild, &prepared.id).await.unwrap().state,
            EffectState::Unknown
        );
        assert_eq!(
            restored.effect(&guild, &in_flight.id).await.unwrap().state,
            EffectState::Unknown
        );
        let fresh = GuildId::new("789").unwrap();
        restored
            .initialize_guilds(std::slice::from_ref(&fresh))
            .await
            .unwrap();
        assert!(restored.status(Some(&fresh)).await.unwrap().guilds[0].paused);
        let extra = match &restored.inner.backend {
            Backend::Sqlite { pool, .. } => {
                sqlx::query_as::<_, (String,)>("SELECT value FROM oracle_test_extra")
                    .fetch_one(pool)
                    .await
                    .unwrap()
                    .0
            }
            Backend::Postgres { pool, .. } => {
                sqlx::query_as::<_, (String,)>("SELECT value FROM oracle_test_extra")
                    .fetch_one(pool)
                    .await
                    .unwrap()
                    .0
            }
        };
        assert_eq!(extra, "preserved");
        assert!(
            Storage::restore(destination.clone(), &bundle, &tools())
                .await
                .is_err()
        );
        restored.close().await.unwrap();
        // Altered backup bytes must be rejected before opening or overwriting a target.
        let file = bundle.join(if manifest.backend == "sqlite" {
            "database.sqlite"
        } else {
            "database.dump"
        });
        OpenOptions::new()
            .append(true)
            .open(file)
            .unwrap()
            .write_all(b"tampered")
            .unwrap();
        assert!(
            Storage::restore(destination, &bundle, &tools())
                .await
                .is_err()
        );
        if let DatabaseConfig::Postgres { url } = &config {
            let mut admin = PgConnection::connect(url).await.unwrap();
            let killed:(bool,)=sqlx::query_as("SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype='advisory' AND granted AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1").bind(LOCK_KEY>>32).bind(LOCK_KEY&0xffffffff).fetch_one(&mut admin).await.unwrap();
            assert!(killed.0);
            assert!(
                store.status(None).await.is_err(),
                "dead owner continued reads"
            );
            assert!(
                store
                    .set_paused(&guild, true, 3, "local-test", &OperationId::generate())
                    .await
                    .is_err(),
                "dead owner continued writes"
            );
            let new_owner = Storage::open(config.clone()).await.unwrap();
            new_owner.close().await.unwrap();
            let _ = store.close().await;
        } else {
            store.close().await.unwrap();
        }
        // Existing migration metadata cannot be silently replaced after a source edit.
        match &config {
            DatabaseConfig::Sqlite { path } => {
                let mut conn = sqlx::SqliteConnection::connect_with(
                    &SqliteConnectOptions::new().filename(path),
                )
                .await
                .unwrap();
                sqlx::query("UPDATE oracle_migrations SET checksum='wrong'")
                    .execute(&mut conn)
                    .await
                    .unwrap();
                conn.close().await.unwrap();
            }
            DatabaseConfig::Postgres { url } => {
                let mut conn = PgConnection::connect(url).await.unwrap();
                sqlx::query("UPDATE oracle_migrations SET checksum='wrong'")
                    .execute(&mut conn)
                    .await
                    .unwrap();
                conn.close().await.unwrap();
            }
        }
        match Storage::open(config).await {
            Err(e) => assert_eq!(e.code, ErrorCode::MigrationMismatch),
            Ok(_) => panic!("mismatched migration admitted"),
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sqlite_contract() {
        let scratch = Scratch::new();
        contract(
            DatabaseConfig::Sqlite {
                path: scratch.0.join("source.sqlite"),
            },
            DatabaseConfig::Sqlite {
                path: scratch.0.join("restored.sqlite"),
            },
            &scratch,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postgres_contract() {
        let Ok(url) = std::env::var("ORACLE_TEST_POSTGRES_URL") else {
            eprintln!("PostgreSQL contract not run: ORACLE_TEST_POSTGRES_URL absent");
            return;
        };
        let destination = std::env::var("ORACLE_TEST_POSTGRES_RESTORE_URL")
            .expect("ORACLE_TEST_POSTGRES_RESTORE_URL is required alongside source URL");
        let scratch = Scratch::new();
        contract(
            DatabaseConfig::Postgres { url },
            DatabaseConfig::Postgres { url: destination },
            &scratch,
        )
        .await;
    }
}
