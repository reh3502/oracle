//! P7 scenario harness: trusted native logger, durable desired config, observed
//! effective config and a deterministic delivery adapter. No Discord network I/O.
#![forbid(unsafe_code)]
use oracle_process_prototype::runtime::{ModuleHandle, ProcessRuntime};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

pub const IDENTITY: &str = "community.activity-log";
pub const SCOPE: &str = "guild:123";
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("module is unavailable or generation changed")]
    Generation,
    #[error("configuration revision changed")]
    Conflict,
    #[error("destination or actor authorization denied")]
    Denied,
    #[error("unknown preset")]
    Preset,
    #[error("missing required capability")]
    Capability,
    #[error("operation remains partial: {0}")]
    Partial(&'static str),
    #[error(transparent)]
    Runtime(#[from] oracle_process_prototype::runtime::RuntimeError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub preset: String,
    pub enabled: Vec<String>,
    pub excluded: Vec<String>,
    pub membership_summary_minutes: u64,
    pub retention_days: u64,
    pub retain_message_content: bool,
    pub retain_attachments: bool,
    pub self_origin_exclusion: bool,
    pub coalesce: bool,
    pub queue_limit: u64,
    pub dropped_event_summary: bool,
    pub destination: String,
    pub operator_note: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub guild_members: bool,
    pub guilds: bool,
    pub guild_moderation: bool,
    pub view_audit_log: bool,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            guild_members: true,
            guilds: true,
            guild_moderation: true,
            view_audit_log: true,
        }
    }
}
#[derive(Debug, Clone)]
pub struct Destination {
    pub id: String,
    pub scope: String,
    pub readers: BTreeSet<String>,
    pub actor_can_view: bool,
    pub bot_can_send: bool,
}
impl Destination {
    pub fn staff() -> Self {
        Self {
            id: "staff-logs".into(),
            scope: SCOPE.into(),
            readers: BTreeSet::from(["staff".into()]),
            actor_can_view: true,
            bot_can_send: true,
        }
    }
    fn permitted(&self) -> bool {
        self.scope == SCOPE
            && self.actor_can_view
            && self.bot_can_send
            && !self.readers.is_empty()
            && self.readers.is_subset(&BTreeSet::from(["staff".into()]))
    }
}
#[derive(Debug, Clone)]
pub struct Plan {
    owner_epoch: String,
    pub id: String,
    pub generation: u64,
    pub expected_revision: u64,
    pub desired: Config,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub id: String,
    pub stored_revision: u64,
    pub effective_revision: Option<u64>,
    pub delivery_id: Option<String>,
    pub complete: bool,
    pub problem: Option<String>,
}

/// Host-owned deterministic fixture for send + independent readback. The key is
/// stable across retries; no live server messages or real moderation events.
#[derive(Default)]
pub struct Delivery {
    pub fail_send: bool,
    pub fail_readback: bool,
    messages: BTreeMap<String, (String, Value)>,
}
impl Delivery {
    pub fn count(&self) -> usize {
        self.messages.len()
    }
    fn send(&mut self, id: &str, destination: &str) -> Result<String, Error> {
        if self.fail_send {
            return Err(Error::Partial("delivery_failed"));
        }
        self.messages.entry(id.into()).or_insert_with(||(destination.into(),json!({"origin":"oracle-synthetic-probe","text":"Synthetic logging delivery test","message_body":null,"attachments":[]})));
        Ok(format!("delivery:{id}"))
    }
    fn verified(&self, id: &str, destination: &str) -> bool {
        !self.fail_readback
            && self.messages.get(id).is_some_and(|(target, payload)| {
                target == destination
                    && payload["origin"] == "oracle-synthetic-probe"
                    && payload["message_body"].is_null()
                    && payload["attachments"] == json!([])
            })
    }
}

pub struct Host {
    pub runtime: ProcessRuntime,
    pub module: Option<ModuleHandle>,
    pub destination: Destination,
    pub capabilities: Capabilities,
    pub delivery: Delivery,
    db: Connection,
    pub host_tracing: String,
    next_plan: u64,
    owner_epoch: String,
}
impl Host {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS config(scope TEXT PRIMARY KEY,revision INTEGER NOT NULL,body TEXT); CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY,body TEXT NOT NULL); INSERT OR IGNORE INTO config VALUES('guild:123',0,NULL);")?;
        Ok(Self {
            runtime: ProcessRuntime::new()?,
            module: None,
            destination: Destination::staff(),
            capabilities: Capabilities::default(),
            delivery: Delivery::default(),
            db,
            host_tracing: "info,oracle=debug".into(),
            next_plan: 0,
            owner_epoch: uuid::Uuid::new_v4().to_string(),
        })
    }
    pub async fn load(&mut self, path: &Path) -> Result<(), Error> {
        if self.module.is_some() {
            return Err(Error::Generation);
        }
        self.module = Some(self.runtime.load(path, IDENTITY).await?);
        Ok(())
    }
    pub async fn unload(&mut self) -> Result<(), Error> {
        if let Some(module) = self.module.take() {
            module.unload(Duration::from_millis(100)).await?;
        }
        Ok(())
    }
    pub async fn discover(&self) -> Result<Value, Error> {
        let module = self.module.as_ref().ok_or(Error::Generation)?;
        Ok(module
            .invoke("describe", json!({}), SCOPE, Duration::from_secs(2))
            .await?)
    }
    pub fn stored(&self) -> Result<(u64, Option<Config>), Error> {
        let (revision, body): (u64, Option<String>) = self.db.query_row(
            "SELECT revision,body FROM config WHERE scope=?1",
            [SCOPE],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            revision,
            body.map(|body| serde_json::from_str(&body)).transpose()?,
        ))
    }
    pub fn receipt(&self, id: &str) -> Result<Receipt, Error> {
        let body: String =
            self.db
                .query_row("SELECT body FROM receipts WHERE id=?1", [id], |r| r.get(0))?;
        Ok(serde_json::from_str(&body)?)
    }
    fn save_receipt(&self, receipt: &Receipt) -> Result<(), Error> {
        self.db.execute("INSERT INTO receipts(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![receipt.id,serde_json::to_string(receipt)?])?;
        Ok(())
    }
    pub async fn plan(&mut self, preset: &str) -> Result<Plan, Error> {
        if !self.destination.permitted() {
            return Err(Error::Denied);
        }
        let descriptor = self.discover().await?;
        if descriptor["preset"]["preset"] != preset {
            return Err(Error::Preset);
        }
        let (revision, current) = self.stored()?;
        let mut desired: Config = serde_json::from_value(descriptor["preset"].clone())?;
        desired.destination = self.destination.id.clone();
        if let Some(current) = current {
            desired.operator_note = current.operator_note;
        }
        let module = self.module.as_ref().ok_or(Error::Generation)?;
        module
            .invoke(
                "prepare",
                json!({"config":desired,"capabilities":self.capabilities}),
                SCOPE,
                Duration::from_secs(2),
            )
            .await
            .map_err(|_| Error::Capability)?;
        self.next_plan += 1;
        Ok(Plan {
            owner_epoch: self.owner_epoch.clone(),
            id: format!("{}:{}:{}", module.generation(), revision, self.next_plan),
            generation: module.generation(),
            expected_revision: revision,
            desired,
        })
    }
    pub async fn apply(
        &mut self,
        plan: &Plan,
        crash_before_ack: bool,
        unhealthy_subscriptions: bool,
    ) -> Result<Receipt, Error> {
        let module = self.module.as_ref().ok_or(Error::Generation)?.clone();
        if module.generation() != plan.generation || plan.owner_epoch != self.owner_epoch {
            return Err(Error::Generation);
        }
        if !self.destination.permitted() || self.destination.id != plan.desired.destination {
            return Err(Error::Denied);
        }
        // Revalidate capabilities at apply time; inspection is not authority.
        module
            .invoke(
                "prepare",
                json!({"config":plan.desired,"capabilities":self.capabilities}),
                SCOPE,
                Duration::from_secs(2),
            )
            .await
            .map_err(|_| Error::Capability)?;
        match self.receipt(&plan.id) {
            Ok(_) => return self.recover(&plan.id).await,
            Err(Error::Sql(rusqlite::Error::QueryReturnedNoRows)) => (),
            Err(error) => return Err(error),
        }
        let revision = plan.expected_revision + 1;
        let receipt = Receipt {
            id: plan.id.clone(),
            stored_revision: revision,
            effective_revision: None,
            delivery_id: None,
            complete: false,
            problem: Some("activation_pending".into()),
        };
        let tx = self.db.transaction()?;
        let changed = tx.execute(
            "UPDATE config SET revision=?1,body=?2 WHERE scope=?3 AND revision=?4",
            params![
                revision,
                serde_json::to_string(&plan.desired)?,
                SCOPE,
                plan.expected_revision
            ],
        )?;
        if changed != 1 {
            return Err(Error::Conflict);
        }
        tx.execute(
            "INSERT INTO receipts VALUES(?1,?2)",
            params![plan.id, serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        self.activate_and_verify(
            receipt,
            plan.desired.clone(),
            crash_before_ack,
            unhealthy_subscriptions,
        )
        .await
    }
    async fn activate_and_verify(
        &mut self,
        mut receipt: Receipt,
        config: Config,
        crash: bool,
        unhealthy: bool,
    ) -> Result<Receipt, Error> {
        receipt.complete = false;
        receipt.effective_revision = None;
        let module = self.module.as_ref().ok_or(Error::Generation)?.clone();
        let activate=module.invoke("activate",json!({"revision":receipt.stored_revision,"config":config,"crash_before_ack":crash,"unhealthy_subscriptions":unhealthy}),SCOPE,Duration::from_secs(2)).await;
        if activate.is_err() {
            receipt.problem = Some("activation_unknown".into());
            self.save_receipt(&receipt)?;
            return Ok(receipt);
        }
        let health = module
            .invoke("health", json!({}), SCOPE, Duration::from_secs(2))
            .await;
        let Ok(health) = health else {
            receipt.problem = Some("health_unavailable".into());
            self.save_receipt(&receipt)?;
            return Ok(receipt);
        };
        if health["effective_revision"].as_u64() != Some(receipt.stored_revision)
            || health["config"] != serde_json::to_value(&config)?
        {
            receipt.problem = Some("effective_config_mismatch".into());
            self.save_receipt(&receipt)?;
            return Ok(receipt);
        }
        receipt.effective_revision = Some(receipt.stored_revision);
        if health["subscriptions_healthy"] != true {
            receipt.problem = Some("subscriptions_unhealthy".into());
            self.save_receipt(&receipt)?;
            return Ok(receipt);
        }
        if !self.destination.permitted() || config.destination != self.destination.id {
            receipt.problem = Some("destination_denied".into());
            self.save_receipt(&receipt)?;
            return Ok(receipt);
        }
        match self.delivery.send(&receipt.id, &config.destination) {
            Ok(id) => {
                receipt.delivery_id = Some(id);
                if self.delivery.verified(&receipt.id, &config.destination) {
                    receipt.complete = true;
                    receipt.problem = None;
                } else {
                    receipt.problem = Some("delivery_unverified".into());
                }
            }
            Err(_) => receipt.problem = Some("delivery_failed".into()),
        }
        self.save_receipt(&receipt)?;
        Ok(receipt)
    }
    pub async fn recover(&mut self, id: &str) -> Result<Receipt, Error> {
        let receipt = self.receipt(id)?;
        // Previously complete is historical evidence; always observe the current process again.
        let (revision, config) = self.stored()?;
        if revision != receipt.stored_revision {
            return Err(Error::Conflict);
        }
        let config = config.ok_or(Error::Conflict)?;
        if !self.destination.permitted() || config.destination != self.destination.id {
            return Err(Error::Denied);
        }
        self.module
            .as_ref()
            .ok_or(Error::Generation)?
            .invoke(
                "prepare",
                json!({"config":config,"capabilities":self.capabilities}),
                SCOPE,
                Duration::from_secs(2),
            )
            .await
            .map_err(|_| Error::Capability)?;
        self.activate_and_verify(receipt, config, false, false)
            .await
    }
    /// Simulates a concurrent authorized human changing an unrelated field.
    pub fn set_operator_note(&mut self, note: &str) -> Result<(), Error> {
        let (revision, config) = self.stored()?;
        let mut config = config.ok_or(Error::Conflict)?;
        config.operator_note = note.into();
        let count = self.db.execute(
            "UPDATE config SET revision=?1,body=?2 WHERE scope=?3 AND revision=?4",
            params![
                revision + 1,
                serde_json::to_string(&config)?,
                SCOPE,
                revision
            ],
        )?;
        if count != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}
