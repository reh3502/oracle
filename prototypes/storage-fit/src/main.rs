//! Disposable P4 experiment. This is a host-owned API, not an arbitrary SQL API.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    PgPool, SqlitePool,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

const DOC_MAX: usize = 64 * 1024;
const PAGE_MAX: usize = 512 * 1024;
const PAGE_ROWS: usize = 100;
const BATCH_MAX: usize = 100;
const CLIENTS: usize = 16;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Scope {
    module: String,
    kind: String,
    id: String,
}
impl Scope {
    fn guild(module: &str, id: &str) -> Self {
        Self {
            module: module.into(),
            kind: "guild".into(),
            id: id.into(),
        }
    }
}
#[derive(Clone)]
enum Backend {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}
struct Database {
    backend: Backend,
    writer: Option<tokio::sync::Mutex<()>>,
    cursors: Mutex<HashMap<String, Cursor>>,
}
#[derive(Clone)]
struct Store {
    db: Arc<Database>,
    scope: Scope,
}
struct Cursor {
    scope: Scope,
    collection: String,
    index: String,
    equals: String,
    last: (i64, String, String),
    expires: Instant,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Document {
    key: String,
    revision: i64,
    version: i64,
    body: Value,
}
#[derive(Clone, Serialize)]
struct Put {
    key: String,
    expected: i64,
    version: i64,
    body: Value,
}
#[derive(Debug)]
struct Page {
    records: Vec<Document>,
    next: Option<String>,
    encoded_bytes: usize,
}
type DocRow = (String, i64, i64, String);

// Each branch uses its concrete backend/transaction type. Identical static SQL
// contracts use $n placeholders supported by both engines; no Any driver.
macro_rules! read_rows {
    ($store:expr, $ty:ty, $sql:expr $(, $bind:expr)* $(,)?) => {{
        match &$store.db.backend {
            Backend::Sqlite(pool)=>sqlx::query_as::<_, $ty>($sql)$(.bind($bind))*.fetch_all(pool).await?,
            Backend::Postgres(pool)=>sqlx::query_as::<_, $ty>($sql)$(.bind($bind))*.fetch_all(pool).await?,
        }
    }};
}
macro_rules! transaction {
    ($store:expr, $tx:ident, $body:block) => {{
        let _writer = match &$store.db.writer {Some(lock)=>Some(lock.lock().await),None=>None};
        match &$store.db.backend {
            Backend::Sqlite(pool)=>{
                let mut $tx=pool.begin_with("BEGIN IMMEDIATE").await?;
                let result:Result<_>=async $body.await;
                match result {Ok(value)=>{$tx.commit().await?;Ok(value)},Err(error)=>{$tx.rollback().await?;Err(error)}}
            },
            Backend::Postgres(pool)=>{
                let mut $tx=pool.begin().await?;
                let result:Result<_>=async $body.await;
                match result {Ok(value)=>{$tx.commit().await?;Ok(value)},Err(error)=>{$tx.rollback().await?;Err(error)}}
            }
        }
    }};
}
const DDL: &str = include_str!("schema.sql");
impl Store {
    async fn connect_sqlite(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .min_connections(4)
            .connect_with(options)
            .await?;
        Ok(Self::from_backend(Backend::Sqlite(pool)))
    }
    async fn connect_postgres(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .min_connections(5)
            .connect(url)
            .await?;
        Ok(Self::from_backend(Backend::Postgres(pool)))
    }
    fn from_backend(backend: Backend) -> Self {
        let writer = matches!(backend, Backend::Sqlite(_)).then(|| tokio::sync::Mutex::new(()));
        Self {
            db: Arc::new(Database {
                backend,
                writer,
                cursors: Mutex::new(HashMap::new()),
            }),
            scope: Scope::guild("moderation", "123"),
        }
    }
    fn scoped(&self, module: &str, guild: &str) -> Self {
        Self {
            db: self.db.clone(),
            scope: Scope::guild(module, guild),
        }
    }
    async fn initialize(&self) -> Result<()> {
        // Framework schema is a fixed, checksummed input. The DB is fresh and
        // exclusive to this disposable harness; production startup locking is P4-external.
        let schema = match self.db.backend {
            Backend::Sqlite(_) => DDL.replace("__COLLATION__", "BINARY"),
            Backend::Postgres(_) => DDL.replace("__COLLATION__", "\"C\""),
        };
        match &self.db.backend {
            Backend::Sqlite(pool) => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(schema.as_str()))
                    .execute(pool)
                    .await?;
            }
            Backend::Postgres(pool) => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(schema.as_str()))
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }
    async fn get(&self, collection: &str, key: &str) -> Result<Option<Document>> {
        let s = &self.scope;
        let rows = read_rows!(
            self,
            DocRow,
            "SELECT key,revision,version,body FROM documents WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5",
            &s.module,
            &s.kind,
            &s.id,
            collection,
            key
        );
        rows.into_iter().map(decode).next().transpose()
    }
    async fn batch(&self, collection: &str, puts: &[Put]) -> Result<()> {
        ensure!(
            !puts.is_empty() && puts.len() <= BATCH_MAX,
            "QuotaExceeded: mutation count"
        );
        ensure!(
            serde_json::to_vec(puts)?.len() <= PAGE_MAX,
            "QuotaExceeded: encoded batch"
        );
        for p in puts {
            validate(p)?;
        }
        let s = &self.scope;
        transaction!(self, tx, {
            for p in puts {
                let body = serde_json::to_string(&p.body)?;
                let affected = if p.expected == 0 {
                    sqlx::query("INSERT INTO documents(module,scope_kind,scope_id,collection,key,revision,version,body) VALUES($1,$2,$3,$4,$5,1,$6,$7) ON CONFLICT DO NOTHING")
                        .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(&p.key).bind(p.version).bind(&body).execute(&mut *tx).await?.rows_affected()
                } else {
                    sqlx::query("UPDATE documents SET revision=revision+1,version=$6,body=$7 WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5 AND revision=$8")
                        .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(&p.key).bind(p.version).bind(&body).bind(p.expected).execute(&mut *tx).await?.rows_affected()
                };
                ensure!(affected == 1, "Conflict");
                sqlx::query("DELETE FROM document_indexes WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5")
                    .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(&p.key).execute(&mut *tx).await?;
                for (name, equals, number, text) in indexes(&p.body)? {
                    sqlx::query("INSERT INTO document_indexes(module,scope_kind,scope_id,collection,key,index_name,eq_text,sort_int,sort_text) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                        .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(&p.key).bind(name).bind(equals).bind(number).bind(text).execute(&mut *tx).await?;
                }
            }
            Ok(())
        })
    }
    async fn delete(&self, collection: &str, key: &str, expected: i64) -> Result<()> {
        let s = &self.scope;
        transaction!(self, tx, {
            let count=sqlx::query("DELETE FROM documents WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5 AND revision=$6")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(key).bind(expected).execute(&mut *tx).await?.rows_affected();
            ensure!(count == 1, "Conflict");
            Ok(())
        })
    }
    async fn query(
        &self,
        collection: &str,
        index: &str,
        equals: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page> {
        ensure!(limit > 0 && limit <= PAGE_ROWS, "QuotaExceeded: read limit");
        ensure!(
            matches!(index, "subject_created" | "label"),
            "SchemaInvalid: undeclared index"
        );
        let last = if let Some(token) = cursor {
            let mut cursors = self.db.cursors.lock().unwrap();
            let found = cursors.get(token).context("SchemaInvalid: cursor")?;
            ensure!(
                found.scope == self.scope
                    && found.collection == collection
                    && found.index == index
                    && found.equals == equals
                    && found.expires > Instant::now(),
                "SchemaInvalid: cursor scope/query/expiry"
            );
            cursors.remove(token).unwrap().last
        } else {
            (i64::MIN, String::new(), String::new())
        };
        let s = &self.scope;
        let rows = read_rows!(
            self,
            (String, i64, i64, String, i64, String),
            "SELECT d.key,d.revision,d.version,d.body,i.sort_int,i.sort_text FROM document_indexes i JOIN documents d ON d.module=i.module AND d.scope_kind=i.scope_kind AND d.scope_id=i.scope_id AND d.collection=i.collection AND d.key=i.key WHERE i.module=$1 AND i.scope_kind=$2 AND i.scope_id=$3 AND i.collection=$4 AND i.index_name=$5 AND i.eq_text=$6 AND (i.sort_int,i.sort_text,i.key)>($7,$8,$9) ORDER BY i.sort_int,i.sort_text,i.key LIMIT $10",
            &s.module,
            &s.kind,
            &s.id,
            collection,
            index,
            equals,
            last.0,
            &last.1,
            &last.2,
            limit as i64
        );
        let mut records = Vec::new();
        let mut position = None;
        for (key, rev, version, body, number, text) in rows {
            let record = decode((key.clone(), rev, version, body))?;
            records.push(record);
            if serde_json::to_vec(&records)?.len() > PAGE_MAX {
                records.pop();
                break;
            }
            position = Some((number, text, key));
        }
        let encoded_bytes = serde_json::to_vec(&records)?.len();
        // A final nonempty page may carry a cursor whose next page is empty.
        let next = if let Some(last) = position {
            let mut cursors = self.db.cursors.lock().unwrap();
            cursors.retain(|_, c| c.expires > Instant::now());
            ensure!(cursors.len() < 128, "QuotaExceeded: live cursors");
            // Opaque references have no client-controllable scope/query fields. Random
            // token source is /dev/urandom; table size and lifetime are bounded.
            use std::io::Read;
            let mut random = [0; 24];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
            let token = format!("{:x}", Sha256::digest(random));
            cursors.insert(
                token.clone(),
                Cursor {
                    scope: s.clone(),
                    collection: collection.into(),
                    index: index.into(),
                    equals: equals.into(),
                    last,
                    expires: Instant::now() + Duration::from_secs(60),
                },
            );
            Some(token)
        } else {
            None
        };
        Ok(Page {
            records,
            next,
            encoded_bytes,
        })
    }
    async fn append(&self, stream: &str, dedup: &str, body: Value) -> Result<i64> {
        ensure!(
            stream.len() <= 64 && dedup.len() <= 128,
            "QuotaExceeded: stream key"
        );
        let kind = body["kind"]
            .as_str()
            .context("SchemaInvalid: stream kind")?;
        let created = body["created"]
            .as_i64()
            .context("SchemaInvalid: stream timestamp")?;
        let text = serde_json::to_string(&body)?;
        ensure!(text.len() <= DOC_MAX, "QuotaExceeded: stream body");
        let s = &self.scope;
        transaction!(self, tx, {
            sqlx::query("INSERT INTO streams(module,scope_kind,scope_id,stream,next_seq) VALUES($1,$2,$3,$4,0) ON CONFLICT DO NOTHING")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(stream).execute(&mut *tx).await?;
            // UPDATE locks this scoped stream on PostgreSQL; SQLite uses its writer lane.
            let next:(i64,)=sqlx::query_as("UPDATE streams SET next_seq=next_seq+1 WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND stream=$4 RETURNING next_seq")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(stream).fetch_one(&mut *tx).await?;
            let seq:(i64,)=sqlx::query_as("INSERT INTO stream_records(module,scope_kind,scope_id,stream,seq,dedup,body,kind,created) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(module,scope_kind,scope_id,stream,dedup) DO UPDATE SET dedup=excluded.dedup RETURNING seq")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(stream).bind(next.0).bind(dedup).bind(&text).bind(kind).bind(created).fetch_one(&mut *tx).await?;
            Ok(seq.0)
        })
    }
    async fn stream(&self, name: &str, after: i64, limit: usize) -> Result<Vec<(i64, Value)>> {
        ensure!(
            limit > 0 && limit <= PAGE_ROWS,
            "QuotaExceeded: stream page"
        );
        let s = &self.scope;
        let rows = read_rows!(
            self,
            (i64, String),
            "SELECT seq,body FROM stream_records WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND stream=$4 AND seq>$5 ORDER BY seq LIMIT $6",
            &s.module,
            &s.kind,
            &s.id,
            name,
            after,
            limit as i64
        );
        let mut page = Vec::new();
        for (seq, body) in rows {
            page.push((seq, serde_json::from_str(&body)?));
            if serde_json::to_vec(&page)?.len() > PAGE_MAX {
                page.pop();
                break;
            }
        }
        Ok(page)
    }
    async fn logging_filter(
        &self,
        stream: &str,
        kind: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Value)>> {
        ensure!(
            limit > 0 && limit <= PAGE_ROWS,
            "QuotaExceeded: stream page"
        );
        let s = &self.scope;
        let rows = read_rows!(
            self,
            (i64, String),
            "SELECT seq,body FROM stream_records WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND stream=$4 AND kind=$5 AND created>=$6 ORDER BY created,seq LIMIT $7",
            &s.module,
            &s.kind,
            &s.id,
            stream,
            kind,
            since,
            limit as i64
        );
        let mut page = Vec::new();
        for (seq, body) in rows {
            page.push((seq, serde_json::from_str(&body)?));
            if serde_json::to_vec(&page)?.len() > PAGE_MAX {
                page.pop();
                break;
            }
        }
        Ok(page)
    }
    async fn reopen(&self) -> Result<Self> {
        let backend = match &self.db.backend {
            Backend::Sqlite(pool) => Backend::Sqlite(
                SqlitePoolOptions::new()
                    .max_connections(4)
                    .connect_with((*pool.connect_options()).clone())
                    .await?,
            ),
            Backend::Postgres(pool) => Backend::Postgres(
                PgPoolOptions::new()
                    .max_connections(5)
                    .connect_with((*pool.connect_options()).clone())
                    .await?,
            ),
        };
        let mut store = Self::from_backend(backend);
        store.scope = self.scope.clone();
        Ok(store)
    }
    async fn migrate_batch(
        &self,
        collection: &str,
        target: i64,
        limit: usize,
        fail_before_commit: bool,
    ) -> Result<usize> {
        ensure!(target == 2, "SchemaInvalid: unsupported upgrade/downgrade");
        ensure!(limit > 0 && limit <= 100, "QuotaExceeded: migration batch");
        let s = &self.scope;
        transaction!(self, tx, {
            sqlx::query("INSERT INTO migrations(module,scope_kind,scope_id,collection,target,last_key) VALUES($1,$2,$3,$4,$5,'') ON CONFLICT DO NOTHING")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(target).execute(&mut *tx).await?;
            // No external effects or live writers while this collection is migrating.
            let checkpoint:(String,)=sqlx::query_as("SELECT last_key FROM migrations WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND target=$5")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(target).fetch_one(&mut *tx).await?;
            let rows:Vec<(String,i64,String)>=sqlx::query_as("SELECT key,version,body FROM documents WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key>$5 ORDER BY key LIMIT $6")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(&checkpoint.0).bind(limit as i64).fetch_all(&mut *tx).await?;
            let mut migrated = 0;
            let mut encoded = 0;
            let mut last_key = None;
            for (key, version, text) in &rows {
                ensure!(
                    *version == 1 || *version == 2,
                    "SchemaInvalid: unreadable migration source"
                );
                let mut body: Value = serde_json::from_str(text)?;
                body["migrated"] = json!(true);
                let body_bytes = serde_json::to_vec(&body)?.len();
                ensure!(body_bytes <= DOC_MAX, "QuotaExceeded: migrated document");
                if encoded + body_bytes > PAGE_MAX {
                    break;
                }
                encoded += body_bytes;
                sqlx::query("UPDATE documents SET version=2,revision=revision+1,body=$6 WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5")
                    .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(key).bind(serde_json::to_string(&body)?).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM document_indexes WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND key=$5")
                    .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(key).execute(&mut *tx).await?;
                for (name, equals, number, sort_text) in indexes(&body)? {
                    sqlx::query("INSERT INTO document_indexes(module,scope_kind,scope_id,collection,key,index_name,eq_text,sort_int,sort_text) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                        .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(key).bind(name).bind(equals).bind(number).bind(sort_text).execute(&mut *tx).await?;
                }
                migrated += 1;
                last_key = Some(key);
            }
            if let Some(key) = last_key {
                sqlx::query("UPDATE migrations SET last_key=$6 WHERE module=$1 AND scope_kind=$2 AND scope_id=$3 AND collection=$4 AND target=$5")
                .bind(&s.module).bind(&s.kind).bind(&s.id).bind(collection).bind(target).bind(key).execute(&mut *tx).await?;
            }
            ensure!(!fail_before_commit, "injected migration interruption");
            Ok(migrated)
        })
    }
}
fn validate(p: &Put) -> Result<()> {
    ensure!(
        p.key.len() <= 128 && !p.key.is_empty() && p.expected >= 0 && matches!(p.version, 1 | 2),
        "SchemaInvalid"
    );
    ensure!(
        serde_json::to_vec(&p.body)?.len() <= DOC_MAX,
        "QuotaExceeded: document"
    );
    indexes(&p.body)?;
    Ok(())
}
fn indexes(body: &Value) -> Result<Vec<(&'static str, String, i64, String)>> {
    let subject = body["subject"]
        .as_str()
        .context("SchemaInvalid: subject string")?;
    let created = body["created"]
        .as_i64()
        .context("SchemaInvalid: created integer")?;
    let label = body["label"]
        .as_str()
        .context("SchemaInvalid: label string")?;
    ensure!(
        subject.len() <= 128 && label.len() <= 128,
        "SchemaInvalid: index string length"
    );
    Ok(vec![
        ("subject_created", subject.into(), created, String::new()),
        ("label", "*".into(), 0, label.into()),
    ])
}
fn decode((key, revision, version, body): DocRow) -> Result<Document> {
    Ok(Document {
        key,
        revision,
        version,
        body: serde_json::from_str(&body)?,
    })
}
fn put(key: &str, expected: i64, created: i64, subject: &str, label: &str) -> Put {
    Put {
        key: key.into(),
        expected,
        version: 1,
        body: json!({"subject":subject,"created":created,"label":label}),
    }
}

async fn all_cases(
    store: &Store,
    collection: &str,
    index: &str,
    equals: &str,
    limit: usize,
) -> Result<Vec<Document>> {
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let page = store
            .query(collection, index, equals, cursor.as_deref(), limit)
            .await?;
        ensure!(
            page.records.len() <= limit && page.encoded_bytes <= PAGE_MAX,
            "page bound"
        );
        out.extend(page.records);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}
async fn contract(store: &Store, name: &str) -> Result<Value> {
    let started = Instant::now();
    store.initialize().await?;
    let twitch = store.scoped("twitch", "123");
    twitch
        .batch("notifications", &[put("stream:99", 0, 0, "99", "live")])
        .await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(CLIENTS));
    let mut clients = JoinSet::new();
    for _ in 0..CLIENTS {
        let scoped = twitch.clone();
        let barrier = barrier.clone();
        clients.spawn(async move {
            barrier.wait().await;
            scoped
                .batch("notifications", &[put("stream:99", 1, 1, "99", "sent")])
                .await
        });
    }
    let mut winners = 0;
    let mut conflicts = 0;
    while let Some(result) = clients.join_next().await {
        match result? {
            Ok(()) => winners += 1,
            Err(e) => {
                ensure!(e.to_string() == "Conflict", "unexpected CAS failure: {e}");
                conflicts += 1
            }
        }
    }
    ensure!(
        winners == 1 && conflicts == CLIENTS - 1,
        "CAS did not choose exactly one winner"
    );
    ensure!(
        twitch
            .get("notifications", "stream:99")
            .await?
            .unwrap()
            .revision
            == 2,
        "CAS revision"
    );
    ensure!(
        twitch
            .scoped("twitch", "999")
            .get("notifications", "stream:99")
            .await?
            .is_none(),
        "guild leak"
    );
    ensure!(
        twitch
            .scoped("other", "123")
            .get("notifications", "stream:99")
            .await?
            .is_none(),
        "module leak"
    );
    let failed = twitch
        .batch(
            "notifications",
            &[
                put("must-rollback", 0, 0, "99", "draft"),
                put("stream:99", 1, 0, "99", "wrong"),
            ],
        )
        .await;
    ensure!(
        failed.is_err()
            && twitch
                .get("notifications", "must-rollback")
                .await?
                .is_none(),
        "atomic batch rollback"
    );
    // Index ordering: signed integer ranges, ties, and explicit bytewise UTF-8.
    let labels = ["z", "A", "ä", "a", "é", "Z"];
    let numbers = [-9, 0, 2, -9, i64::MAX, i64::MIN];
    let puts: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| put(&format!("case:{i}"), 0, numbers[i], "subject:7", label))
        .collect();
    store.batch("cases", &puts).await?;
    let mut tampered = put("injected-scope", 0, 3, "subject:other", "tamper");
    tampered.body["module"] = json!("other");
    tampered.body["scope_id"] = json!("999");
    store.batch("cases", &[tampered]).await?;
    ensure!(
        store
            .scoped("other", "999")
            .get("cases", "injected-scope")
            .await?
            .is_none(),
        "payload changed host scope"
    );
    let by_number = all_cases(store, "cases", "subject_created", "subject:7", 2).await?;
    let mut expected = puts.clone();
    expected.sort_by_key(|p| (p.body["created"].as_i64().unwrap(), p.key.clone()));
    ensure!(
        by_number
            .iter()
            .map(|d| &d.key)
            .eq(expected.iter().map(|p| &p.key)),
        "integer ordering differs"
    );
    let by_label = all_cases(store, "cases", "label", "*", 2).await?;
    let labels: Vec<_> = by_label
        .iter()
        .map(|d| d.body["label"].as_str().unwrap())
        .collect();
    ensure!(
        labels == vec!["A", "Z", "a", "tamper", "z", "ä", "é"],
        "bytewise ordering differs: {labels:?}"
    );
    let page = store
        .query("cases", "subject_created", "subject:7", None, 2)
        .await?;
    let token = page.next.unwrap();
    ensure!(
        store
            .scoped("moderation", "999")
            .query("cases", "subject_created", "subject:7", Some(&token), 2)
            .await
            .is_err(),
        "cursor scope leak"
    );
    ensure!(
        store
            .query("cases", "label", "*", Some(&token), 2)
            .await
            .is_err(),
        "cursor query leak"
    );
    ensure!(
        store
            .query("cases", "subject_created", "subject:7", Some("forged"), 2)
            .await
            .is_err(),
        "forged cursor"
    );
    store
        .db
        .cursors
        .lock()
        .unwrap()
        .get_mut(&token)
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    ensure!(
        store
            .query("cases", "subject_created", "subject:7", Some(&token), 2)
            .await
            .is_err(),
        "expired cursor"
    );
    store
        .batch("cases", &[put("case:0", 1, 55, "moved", "moved")])
        .await?;
    ensure!(
        all_cases(store, "cases", "subject_created", "subject:7", 100)
            .await?
            .len()
            == 5,
        "stale update index"
    );
    ensure!(
        all_cases(store, "cases", "subject_created", "moved", 100)
            .await?
            .len()
            == 1,
        "updated index absent"
    );
    store.delete("cases", "case:0", 2).await?;
    ensure!(
        all_cases(store, "cases", "subject_created", "moved", 100)
            .await?
            .is_empty(),
        "delete left index"
    );
    // Enforce all public limits before performing a mutation.
    ensure!(
        store
            .batch("limits", &vec![put("x", 0, 0, "x", "x"); 101])
            .await
            .is_err(),
        "batch count limit"
    );
    let mut big = put("oversize", 0, 0, "x", "x");
    big.body["padding"] = json!("x".repeat(DOC_MAX));
    ensure!(
        store.batch("limits", &[big]).await.is_err(),
        "document bytes limit"
    );
    let mut large_docs = Vec::new();
    for i in 0..12 {
        let mut p = put(&format!("large:{i:02}"), 0, i, "large", "large");
        p.body["padding"] = json!("x".repeat(60 * 1024));
        large_docs.push(p);
    }
    ensure!(
        store.batch("limits", &large_docs).await.is_err(),
        "encoded batch bytes limit"
    );
    for chunk in large_docs.chunks(4) {
        store.batch("large", chunk).await?;
    }
    let page = store
        .query("large", "subject_created", "large", None, 100)
        .await?;
    ensure!(
        page.records.len() < 12 && page.encoded_bytes <= PAGE_MAX,
        "read page byte truncation"
    );
    ensure!(
        all_cases(store, "large", "subject_created", "large", 100)
            .await?
            .len()
            == 12,
        "page byte truncation lost rows"
    );
    let large_migration_first = store.migrate_batch("large", 2, 100, false).await?;
    let large_migration_second = store.migrate_batch("large", 2, 100, false).await?;
    ensure!(
        large_migration_first == 8 && large_migration_second == 4,
        "migration byte boundary ignored"
    );
    ensure!(
        store.query("cases", "label", "*", None, 101).await.is_err(),
        "unbounded query"
    );
    ensure!(
        store
            .query("cases", "arbitrary", "*", None, 1)
            .await
            .is_err(),
        "undeclared index admitted"
    );
    let mut invalid = put("typed", 0, 0, "x", "x");
    invalid.body["created"] = json!("0");
    ensure!(
        store.batch("limits", &[invalid]).await.is_err(),
        "index type confusion"
    );
    // 16 real client tasks, each issuing 32 unique appends and duplicate replay.
    let logging = store.scoped("logging", "123");
    let mut clients = JoinSet::new();
    for client in 0..CLIENTS {
        let logging = logging.clone();
        clients.spawn(async move {
            for event in 0..32 {
                let dedup = format!("client:{client}/event:{event}");
                let body = json!({"client":client,"event":event,"kind":"message","created":event});
                let seq = logging.append("activity", &dedup, body.clone()).await?;
                let replay = logging.append("activity", &dedup, body).await?;
                ensure!(seq == replay, "stream dedup changed sequence");
            }
            Ok::<_, anyhow::Error>(())
        });
    }
    while let Some(result) = clients.join_next().await {
        result??;
    }
    let mut stream = Vec::new();
    let mut after = 0;
    loop {
        let page = logging.stream("activity", after, 100).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().unwrap().0;
        stream.extend(page);
    }
    ensure!(
        stream.len() == CLIENTS * 32,
        "logging duplicate or dropped rows"
    );
    ensure!(
        stream.windows(2).all(|w| w[0].0 < w[1].0),
        "stream order not monotonic"
    );
    let filtered = logging
        .logging_filter("activity", "message", 16, 100)
        .await?;
    ensure!(
        filtered.len() == 100
            && filtered
                .iter()
                .all(|(_, v)| v["created"].as_i64().unwrap() >= 16),
        "logging kind/time lookup"
    );
    ensure!(
        filtered
            .windows(2)
            .all(|w| w[0].1["created"].as_i64() <= w[1].1["created"].as_i64()),
        "logging timestamp ordering"
    );
    ensure!(
        logging
            .scoped("logging", "999")
            .stream("activity", 0, 100)
            .await?
            .is_empty(),
        "stream scope leak"
    );
    let seq = logging
        .scoped("logging", "999")
        .append(
            "activity",
            "client:0/event:0",
            json!({"kind":"independent","created":0}),
        )
        .await?;
    ensure!(seq == 1, "dedup crossed scope");
    // Seed 6,000 moderate cases in bounded batches: index plans must scale with
    // selected subject and keyset page, not scan the full document collection.
    for chunk in 0..60 {
        let rows: Vec<_> = (0..100)
            .map(|j| {
                let i = chunk * 100 + j;
                put(
                    &format!("seed:{i:05}"),
                    0,
                    i as i64,
                    &format!("subject:{}", i % 50),
                    "seed",
                )
            })
            .collect();
        store.batch("seed", &rows).await?;
    }
    let selected = all_cases(store, "seed", "subject_created", "subject:7", 37).await?;
    ensure!(selected.len() == 120, "representative subject lookup");
    let migration = store.scoped("migration", "123");
    let rows: Vec<_> = (0..137)
        .map(|i| put(&format!("row:{i:04}"), 0, i, "m", "m"))
        .collect();
    for chunk in rows.chunks(100) {
        migration.batch("cases", chunk).await?;
    }
    ensure!(
        migration.migrate_batch("cases", 2, 37, true).await.is_err(),
        "injected interruption did not fail"
    );
    ensure!(
        migration.get("cases", "row:0000").await?.unwrap().version == 1,
        "interruption committed data"
    );
    let first = migration.migrate_batch("cases", 2, 37, false).await?;
    ensure!(first == 37, "migration row bound");
    // Resume through a new host Store handle, using the durable DB checkpoint.
    let resumed = migration.reopen().await?;
    let mut migrated = first;
    let mut migration_batches = vec![first];
    loop {
        let n = resumed.migrate_batch("cases", 2, 37, false).await?;
        if n == 0 {
            break;
        }
        ensure!(n <= 37, "migration batch unbounded");
        migrated += n;
        migration_batches.push(n);
    }
    ensure!(migrated == 137, "migration resume skipped/repeated records");
    let migrated_docs = all_cases(&migration, "cases", "subject_created", "m", 100).await?;
    ensure!(
        migrated_docs.len() == 137
            && migrated_docs
                .iter()
                .all(|d| d.version == 2 && d.revision == 2 && d.body["migrated"] == true),
        "migration/index rebuild inconsistent"
    );
    ensure!(
        migration
            .migrate_batch("cases", 1, 37, false)
            .await
            .is_err(),
        "invalid downgrade admitted"
    );
    let (version, plans, connections) = inspect(store).await?;
    let mut canonical_stream: Vec<_> = stream.iter().map(|(_, value)| value.clone()).collect();
    canonical_stream.sort_by_key(|v| (v["client"].as_u64().unwrap(), v["event"].as_u64().unwrap()));
    let semantic = json!({"cas_winners":winners,"cas_conflicts":conflicts,"ordered_keys":by_number.iter().map(|d|&d.key).collect::<Vec<_>>(),"ordered_labels":labels,"logging_rows":canonical_stream,"migration_rows":migrated,"migration_batches":migration_batches,"selected_cases":selected.len()});
    Ok(
        json!({"backend":name,"status":"passed","version":version,"real_pool_connections":connections,"elapsed_ms":started.elapsed().as_millis(),"semantic_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&semantic)?)),"summary":{"cas_winners":winners,"cas_conflicts":conflicts,"seed_documents":6000,"stream_records":stream.len(),"migration_rows":migrated,"migration_batches":migration_batches,"bounded_page_rows":100,"bounded_page_bytes":PAGE_MAX,"bounded_document_bytes":DOC_MAX},"explain":plans}),
    )
}

async fn inspect(store: &Store) -> Result<(Value, Value, usize)> {
    let queries = [
        (
            "twitch_key",
            "SELECT revision,body FROM documents WHERE module='twitch' AND scope_kind='guild' AND scope_id='123' AND collection='notifications' AND key='stream:99'",
        ),
        (
            "moderation_index",
            "SELECT d.key,d.body FROM document_indexes i JOIN documents d ON d.module=i.module AND d.scope_kind=i.scope_kind AND d.scope_id=i.scope_id AND d.collection=i.collection AND d.key=i.key WHERE i.module='moderation' AND i.scope_kind='guild' AND i.scope_id='123' AND i.collection='seed' AND i.index_name='subject_created' AND i.eq_text='subject:7' AND (i.sort_int,i.sort_text,i.key)>(1000,'','') ORDER BY i.sort_int,i.sort_text,i.key LIMIT 37",
        ),
        (
            "logging_page",
            "SELECT seq,body FROM stream_records WHERE module='logging' AND scope_kind='guild' AND scope_id='123' AND stream='activity' AND seq>100 ORDER BY seq LIMIT 100",
        ),
        (
            "logging_kind_time",
            "SELECT seq,body FROM stream_records WHERE module='logging' AND scope_kind='guild' AND scope_id='123' AND stream='activity' AND kind='message' AND created>=16 ORDER BY created,seq LIMIT 100",
        ),
        (
            "migration_page",
            "SELECT key,version,body FROM documents WHERE module='migration' AND scope_kind='guild' AND scope_id='123' AND collection='cases' AND key>'row:0036' ORDER BY key LIMIT 37",
        ),
    ];
    let mut plans = serde_json::Map::new();
    let orphan_sql = "INSERT INTO document_indexes(module,scope_kind,scope_id,collection,key,index_name,eq_text,sort_int,sort_text) VALUES('orphan','guild','123','missing','missing','label','*',0,'x')";
    let orphan_error = match &store.db.backend {
        Backend::Sqlite(pool) => sqlx::query(orphan_sql).execute(pool).await.unwrap_err(),
        Backend::Postgres(pool) => sqlx::query(orphan_sql).execute(pool).await.unwrap_err(),
    };
    ensure!(
        orphan_error
            .as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "orphan index FK not enforced"
    );
    match &store.db.backend {
        Backend::Sqlite(pool) => {
            let version: (String, String) =
                sqlx::query_as("SELECT sqlite_version(),sqlite_source_id()")
                    .fetch_one(pool)
                    .await?;
            let journal: (String,) = sqlx::query_as("PRAGMA journal_mode")
                .fetch_one(pool)
                .await?;
            ensure!(journal.0 == "wal", "SQLite is not real WAL");
            let mut conns = Vec::new();
            for _ in 0..4 {
                let mut conn = pool.acquire().await?;
                let fk: (i64,) = sqlx::query_as("PRAGMA foreign_keys")
                    .fetch_one(&mut *conn)
                    .await?;
                ensure!(fk.0 == 1, "FK not enabled on every connection");
                conns.push(conn);
            }
            drop(conns);
            sqlx::query("ANALYZE").execute(pool).await?;
            for (name, query) in queries {
                let explain = format!("EXPLAIN QUERY PLAN {query}");
                let rows: Vec<(i64, i64, i64, String)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(explain.as_str()))
                        .fetch_all(pool)
                        .await?;
                let lines: Vec<_> = rows.into_iter().map(|r| r.3).collect();
                ensure!(
                    lines.iter().all(|s| !s.contains("SCAN ")),
                    "full scan in {name}: {lines:?}"
                );
                ensure!(
                    lines.iter().any(|s| s.contains("INDEX")),
                    "missing index {name}"
                );
                plans.insert(name.into(), json!(lines));
            }
            Ok((
                json!({"sqlite_version":version.0,"sqlite_source_id":version.1,"journal_mode":journal.0,"libsqlite3_sys":"0.37.0 bundled"}),
                Value::Object(plans),
                4,
            ))
        }
        Backend::Postgres(pool) => {
            let version: (String,) = sqlx::query_as("SELECT version()").fetch_one(pool).await?;
            let mut conns = Vec::new();
            let mut pids = Vec::new();
            for _ in 0..5 {
                let mut conn = pool.acquire().await?;
                let pid: (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
                    .fetch_one(&mut *conn)
                    .await?;
                pids.push(pid.0);
                conns.push(conn);
            }
            pids.sort();
            pids.dedup();
            ensure!(
                pids.len() == 5,
                "Postgres clients shared a single connection"
            );
            drop(conns);
            sqlx::query("ANALYZE").execute(pool).await?;
            for (name, query) in queries {
                let explain = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) {query}");
                let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(explain.as_str()))
                    .fetch_all(pool)
                    .await?;
                let lines: Vec<_> = rows.into_iter().map(|r| r.0).collect();
                ensure!(
                    lines.iter().all(|s| !s.contains("Seq Scan")),
                    "full scan in {name}: {lines:?}"
                );
                ensure!(
                    lines.iter().any(|s| s.contains("Index")),
                    "missing index {name}"
                );
                plans.insert(name.into(), json!(lines));
            }
            Ok((json!({"postgres":version.0}), Value::Object(plans), 5))
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let output = std::env::args()
        .nth(1)
        .context("usage: oracle-storage-fit OUTPUT_JSON SQLITE_FILE (P4_POSTGRES_URL required)")?;
    let sqlite = std::env::args().nth(2).context("SQLite file required")?;
    // A fresh file and disposable PG database are mandatory. Never overwrite data.
    ensure!(!Path::new(&sqlite).exists(), "SQLite file already exists");
    let url = std::env::var("P4_POSTGRES_URL")
        .context("P4_POSTGRES_URL required; no skipped backend passes")?;
    std::fs::write(
        &output,
        serde_json::to_vec_pretty(&json!({"status":"running"}))?,
    )?;
    let result:Result<Value>=async {
        let local=Store::connect_sqlite(Path::new(&sqlite)).await?;
        let a=contract(&local,"sqlite").await.context("SQLite contract")?;
        ensure!(Path::new(&format!("{sqlite}-wal")).exists(), "SQLite WAL file missing");
        let remote=Store::connect_postgres(&url).await?;
        let b=contract(&remote,"postgres").await.context("PostgreSQL contract")?;
        ensure!(a["semantic_sha256"]==b["semantic_sha256"],"backend contract outputs differ");
        Ok(json!({"gate":"P4","status":"passed","sqlx":"0.9.0","schema_sha256":format!("{:x}",Sha256::digest(DDL.as_bytes())),"cargo_lock_sha256":format!("{:x}",Sha256::digest(include_bytes!("../Cargo.lock"))),"backends":[a,b],"scope":"representative scoped storage fit; not full production persistence, effect recovery, backup or deployment migrations"}))
    }.await;
    let report = match &result {
        Ok(report) => report.clone(),
        Err(e) => json!({"gate":"P4","status":"failed","error":format!("{e:#}")}),
    };
    std::fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    result?;
    println!("P4 passed; both backend semantics match; report {output}");
    Ok(())
}
