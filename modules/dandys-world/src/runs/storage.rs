//! Atomic run aggregates and bounded indexes over the host's scoped documents.
use super::domain::{
    self, Actor, Command, Confirmation, EligibilitySnapshot, Outcome, Run, RunMode, RunState,
};
use async_trait::async_trait;
use oracle_contracts::{DocumentWrite, ModuleDocument};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

pub const DATA_VERSION: u32 = 4;
pub const MAX_AGGREGATE_BYTES: usize = 40 * 1024;
pub const DAY: u64 = 86_400_000;
const MAX_BYTES: usize = 32 * 1024;
const MAX_RECEIPTS: usize = 1_000;
const DISCORD_EPOCH: u64 = 1_420_070_400_000;
pub const COLLECTIONS: [&str; 3] = ["runs", "run_index", "run_receipts"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Rule(#[from] domain::Error),
    #[error("run not found in this guild")]
    NotFound,
    #[error("run storage changed; try again")]
    Conflict,
    #[error("run storage is busy; try again")]
    Busy,
    #[error("run storage is unavailable")]
    Unavailable,
    #[error("stored run data is invalid")]
    Corrupt,
    #[error("this guild has reached a run or receipt limit")]
    Limit,
    #[error("this host already has {limit} active runs")]
    OwnerPublishedLimit { limit: usize },
    #[error("interaction identity is expired or inconsistent")]
    Interaction,
}
pub type Result<T> = std::result::Result<T, Error>;

/// The host adapter supplies the scope; operation JSON cannot select a namespace.
#[async_trait]
pub trait Documents: Send + Sync {
    fn guild(&self) -> &str;
    async fn get(&self, collection: &str, key: &str) -> Result<Option<ModuleDocument>>;
    async fn batch(&self, writes: Vec<DocumentWrite>) -> Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationIntent {
    pub key: String,
    pub desired_revision: u64,
    #[serde(default)]
    pub repost_generation: u32,
    pub destination: String,
    pub created_at: u64,
    pub card: Value,
    pub actions: Vec<super::ui::PublicAction>,
}

/// Bounded management history, removed with the aggregate at terminal expiry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    pub actor_id: String,
    pub action: String,
    pub at: u64,
    pub affected_member: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredRun {
    pub moderator_audit: Vec<AuditEntry>,
    pub schema_version: u32,
    pub run: Run,
    /// Desired output only. The module never writes a host delivery receipt.
    pub publication: Option<PublicationIntent>,
}
impl StoredRun {
    fn validate(&self, guild: &str, id: &str) -> Result<()> {
        self.validate_version(guild, id, DATA_VERSION)
    }
    fn validate_version(&self, guild: &str, id: &str, version: u32) -> Result<()> {
        self.run.validate().map_err(|_| Error::Corrupt)?;
        if self.schema_version != version || self.run.guild_id != guild || self.run.id != id {
            return Err(Error::Corrupt);
        }
        if let Some(intent) = &self.publication
            && (intent.key != id
                || intent.destination != "runs"
                || intent.desired_revision != self.run.desired_card_revision
                || intent.created_at < self.run.created_at)
        {
            return Err(Error::Corrupt);
        }
        if self.moderator_audit.len() > 128
            || self.moderator_audit.iter().any(|e| {
                !domain::valid_member_id(&e.actor_id)
                    || e.action.len() > 32
                    || e.at < self.run.created_at
                    || e.at > self.run.updated_at
                    || e.affected_member
                        .as_ref()
                        .is_some_and(|id| !domain::valid_member_id(id))
            })
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MutationResult {
    pub run_id: String,
    pub outcome: Outcome,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationToken {
    pub interaction_id: String,
    pub token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Applied {
        result: MutationResult,
    },
    Confirm {
        run_id: String,
        revision: u64,
        confirmation: ConfirmationToken,
    },
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Create {
        mode: RunMode,
        name: Option<String>,
    },
    Change {
        run_id: String,
        command: Command,
        confirmation: Option<ConfirmationToken>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Internal adapter only: a fresh host status must establish Missing first.
    Repost {
        run_id: String,
        expected_revision: u64,
    },
    Prepare {
        run_id: String,
        command: Command,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveEntry {
    owner: String,
    state: RunState,
    last_owner_edit_at: u64,
}
type Live = BTreeMap<String, LiveEntry>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Maintenance {
    version: u32,
    terminal_buckets: BTreeSet<u64>,
    terminal_count: usize,
    receipt_buckets: BTreeSet<String>,
    receipt_count: usize,
    tombstone_count: usize,
    /// Includes terminal records with unresolved publication intent.
    pending: BTreeSet<String>,
    published_per_day: BTreeMap<u64, u16>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    user: String,
    hash: String,
    expires_at: u64,
    payload: ReceiptPayload,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReceiptPayload {
    Outcome {
        result: std::result::Result<MutationResult, domain::Error>,
    },
    Challenge {
        run_id: String,
        revision: u64,
        command_hash: String,
        token: String,
        valid_until: u64,
        used: bool,
    },
}

/// Tracks exact read revisions and commits all dirty keys in deterministic order.
struct Transaction<'a, D> {
    docs: &'a D,
    reads: BTreeMap<(String, String), Option<ModuleDocument>>,
    dirty: BTreeMap<(String, String), Option<Value>>,
}
impl<'a, D: Documents> Transaction<'a, D> {
    fn new(docs: &'a D) -> Self {
        Self {
            docs,
            reads: BTreeMap::new(),
            dirty: BTreeMap::new(),
        }
    }
    async fn get<T: DeserializeOwned>(&mut self, collection: &str, key: &str) -> Result<Option<T>> {
        let k = (collection.to_owned(), key.to_owned());
        if !self.reads.contains_key(&k) {
            self.reads
                .insert(k.clone(), self.docs.get(collection, key).await?);
        }
        let value = self
            .dirty
            .get(&k)
            .cloned()
            .unwrap_or_else(|| self.reads[&k].as_ref().map(|d| d.value.clone()));
        value
            .map(|value| serde_json::from_value(value).map_err(|_| Error::Corrupt))
            .transpose()
    }
    async fn put<T: Serialize>(&mut self, collection: &str, key: &str, value: &T) -> Result<()> {
        let _: Option<Value> = self.get(collection, key).await?;
        let value = serde_json::to_value(value).map_err(|_| Error::Corrupt)?;
        let limit = if collection == "run_receipts" {
            512
        } else if collection == "runs"
            && !matches!(
                value["run"]["state"].as_str(),
                Some("completed" | "cancelled")
            )
        {
            // Reserve a final moderator audit entry, terminal timestamp and
            // publication metadata so even a full live aggregate can close.
            MAX_AGGREGATE_BYTES - 512
        } else if collection == "runs" {
            MAX_AGGREGATE_BYTES
        } else {
            MAX_BYTES
        };
        if serde_json::to_vec(&value)
            .map_err(|_| Error::Corrupt)?
            .len()
            > limit
        {
            return Err(Error::Limit);
        }
        self.dirty
            .insert((collection.into(), key.into()), Some(value));
        Ok(())
    }
    async fn delete(&mut self, collection: &str, key: &str) -> Result<()> {
        let _: Option<Value> = self.get(collection, key).await?;
        let k = (collection.into(), key.into());
        if self.reads[&k].is_some() {
            self.dirty.insert(k, None);
        }
        Ok(())
    }
    async fn commit(self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        if self.dirty.len() > 8 {
            return Err(Error::Limit);
        }
        let writes: Vec<_> = self
            .dirty
            .into_iter()
            .map(|((collection, key), value)| {
                let revision = self.reads[&(collection.clone(), key.clone())]
                    .as_ref()
                    .map(|d| d.revision);
                DocumentWrite {
                    collection,
                    key,
                    expected_revision: revision,
                    value,
                }
            })
            .collect();
        if serde_json::to_vec(&writes)
            .map_err(|_| Error::Corrupt)?
            .len()
            > 128 * 1024
        {
            return Err(Error::Limit);
        }
        self.docs.batch(writes).await
    }
}

fn hash(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|_| Error::Corrupt)?)
    ))
}
fn random(bytes: usize) -> Result<Vec<u8>> {
    let mut result = vec![0; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut result))
        .map_err(|_| Error::Unavailable)?;
    Ok(result)
}
fn new_id() -> Result<String> {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut id = String::new();
    while id.len() < 8 {
        for byte in random(16)? {
            let cutoff = 256 - (256 % ALPHABET.len());
            if usize::from(byte) < cutoff {
                id.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
            }
            if id.len() == 8 {
                break;
            }
        }
    }
    Ok(id)
}
fn interaction_timestamp(id: &str, now: u64) -> Result<u64> {
    if !domain::valid_member_id(id) {
        return Err(Error::Interaction);
    }
    let created = (id.parse::<u64>().map_err(|_| Error::Interaction)? >> 22)
        .checked_add(DISCORD_EPOCH)
        .ok_or(Error::Interaction)?;
    if created > now || now - created > 15 * 60_000 {
        return Err(Error::Interaction);
    }
    Ok(created)
}
fn receipt_bucket(id: &str, expires_at: u64) -> String {
    // A fixed shard plus expiry hour keeps indexes bounded even under skew.
    let shard = Sha256::digest(id.as_bytes())[0] % 16;
    format!("receipts/{}/{shard}", expires_at / 3_600_000)
}
fn tombstone_bucket(id: &str) -> String {
    format!("tombstones/{}", id.as_bytes()[0] % 16)
}
fn normalize_command(command: &Command) -> Command {
    let mut command = command.clone();
    match &mut command {
        Command::Cancel { confirmation } | Command::Remove { confirmation, .. } => {
            *confirmation = Confirmation {
                actor_id: String::new(),
                revision: 0,
            }
        }
        Command::SetMode { confirmation, .. } => *confirmation = None,
        _ => {}
    }
    command
}
fn requires_confirmation(run: &Run, command: &Command) -> bool {
    matches!(command, Command::Cancel { .. } | Command::Remove { .. })
        || matches!(command,Command::SetMode{mode,..} if *mode==RunMode::Casual && run.mode==RunMode::Organized)
}

/// Operator admission settings may lower, never raise, the storage ceilings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub drafts_per_owner: usize,
    pub published_per_owner: usize,
    pub drafts_per_guild: usize,
    pub published_per_guild: usize,
    pub published_per_day: u16,
    pub retained_terminal: usize,
    pub tombstones: usize,
    pub receipts: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            drafts_per_owner: 1,
            published_per_owner: 2,
            drafts_per_guild: 20,
            published_per_guild: 50,
            published_per_day: 20,
            retained_terminal: 700,
            tombstones: 1024,
            receipts: MAX_RECEIPTS,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        let d = Self::default();
        if self.drafts_per_owner > d.drafts_per_owner
            || self.published_per_owner > d.published_per_owner
            || self.drafts_per_guild > d.drafts_per_guild
            || self.published_per_guild > d.published_per_guild
            || self.published_per_day > d.published_per_day
            || self.retained_terminal > d.retained_terminal
            || self.tombstones > d.tombstones
            || self.receipts > d.receipts
        {
            return Err(domain::Error::InvalidInput.into());
        }
        Ok(())
    }
}
pub struct RunService<D> {
    docs: D,
    limits: Limits,
}
impl<D: Documents> RunService<D> {
    pub fn new(docs: D) -> Self {
        Self {
            docs,
            limits: Limits::default(),
        }
    }
    pub fn with_limits(docs: D, limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self { docs, limits })
    }
    fn authorize(&self, actor: &Actor) -> Result<()> {
        if actor.guild_id != self.docs.guild() || !domain::valid_member_id(&actor.user_id) {
            return Err(domain::Error::Forbidden.into());
        }
        Ok(())
    }
    pub async fn view(&self, actor: &Actor, id: &str) -> Result<StoredRun> {
        self.authorize(actor)?;
        let id = domain::normalize_run_id(id)?;
        let document = self.docs.get("runs", &id).await?.ok_or(Error::NotFound)?;
        let run: StoredRun = serde_json::from_value(document.value).map_err(|_| Error::Corrupt)?;
        run.validate(self.docs.guild(), &id)?;
        if run.run.state == RunState::Draft
            && run.run.owner_id != actor.user_id
            && !actor.manage_all_runs
        {
            return Err(domain::Error::Forbidden.into());
        }
        Ok(run)
    }
    pub async fn list(&self, actor: &Actor, after: Option<&str>) -> Result<Vec<StoredRun>> {
        self.authorize(actor)?;
        let after = after.map(domain::normalize_run_id).transpose()?;
        let mut tx = Transaction::new(&self.docs);
        let live: Live = tx.get("run_index", "live").await?.unwrap_or_default();
        if live.len() > 70 {
            return Err(Error::Corrupt);
        }
        let mut result = Vec::new();
        for (id, entry) in live {
            if after.as_ref().is_some_and(|a| &id <= a) || entry.state != RunState::Open {
                continue;
            }
            let run = self.view(actor, &id).await?;
            if run.run.state == RunState::Open {
                result.push(run);
            }
            if result.len() == 25 {
                break;
            }
        }
        Ok(result)
    }
    pub async fn execute(
        &self,
        actor: &Actor,
        interaction: &str,
        request: Request,
        current: &EligibilitySnapshot,
        now: u64,
    ) -> Result<Response> {
        self.authorize(actor)?;
        interaction_timestamp(interaction, now)?;
        // Canonicalize inputs before fingerprinting and never trust nested actor confirmation.
        let request = match request {
            Request::Change {
                run_id,
                command,
                confirmation,
                expected_revision,
            } => Request::Change {
                run_id: domain::normalize_run_id(&run_id)?,
                command: normalize_command(&command),
                confirmation,
                expected_revision,
            },
            Request::Repost {
                run_id,
                expected_revision,
            } => Request::Repost {
                run_id: domain::normalize_run_id(&run_id)?,
                expected_revision,
            },
            Request::Prepare { run_id, command } => Request::Prepare {
                run_id: domain::normalize_run_id(&run_id)?,
                command: normalize_command(&command),
            },
            other => other,
        };
        let fingerprint = hash(&request)?;
        for _ in 0..3 {
            match self
                .attempt(actor, interaction, &request, current, now, &fingerprint)
                .await
            {
                Err(Error::Conflict) => continue,
                result => return result,
            }
        }
        Err(Error::Busy)
    }
    async fn load_run(&self, tx: &mut Transaction<'_, D>, id: &str) -> Result<StoredRun> {
        let run: StoredRun = tx.get("runs", id).await?.ok_or(Error::NotFound)?;
        run.validate(self.docs.guild(), id)?;
        Ok(run)
    }
    fn replay(
        receipt: Receipt,
        actor: &Actor,
        interaction: &str,
        fingerprint: &str,
        now: u64,
    ) -> Result<Response> {
        if receipt.user != actor.user_id || receipt.hash != fingerprint || receipt.expires_at <= now
        {
            return Err(Error::Interaction);
        }
        match receipt.payload {
            ReceiptPayload::Outcome { result } => result
                .map(|result| Response::Applied { result })
                .map_err(Error::Rule),
            ReceiptPayload::Challenge {
                run_id,
                revision,
                token,
                valid_until,
                used,
                ..
            } => {
                if valid_until <= now || used {
                    return Err(domain::Error::StaleConfirmation.into());
                }
                Ok(Response::Confirm {
                    run_id,
                    revision,
                    confirmation: ConfirmationToken {
                        interaction_id: interaction.into(),
                        token,
                    },
                })
            }
        }
    }
    async fn receipt(
        &self,
        tx: &mut Transaction<'_, D>,
        meta: &mut Maintenance,
        interaction: &str,
        receipt: &Receipt,
    ) -> Result<()> {
        if meta.receipt_count >= self.limits.receipts {
            return Err(Error::Limit);
        }
        let key = receipt_bucket(interaction, receipt.expires_at);
        let mut bucket: BTreeMap<String, u64> =
            tx.get("run_index", &key).await?.unwrap_or_default();
        if bucket.len() >= 256
            || bucket
                .insert(interaction.into(), receipt.expires_at)
                .is_some()
        {
            return Err(Error::Limit);
        }
        meta.receipt_count += 1;
        meta.receipt_buckets.insert(key.clone());
        tx.put("run_receipts", interaction, receipt).await?;
        tx.put("run_index", &key, &bucket).await?;
        tx.put("run_index", "maintenance", meta).await
    }
    async fn attempt(
        &self,
        actor: &Actor,
        interaction: &str,
        request: &Request,
        current: &EligibilitySnapshot,
        now: u64,
        fingerprint: &str,
    ) -> Result<Response> {
        let mut tx = Transaction::new(&self.docs);
        if let Some(receipt) = tx.get("run_receipts", interaction).await? {
            return Self::replay(receipt, actor, interaction, fingerprint, now);
        }
        let mut meta: Maintenance = tx
            .get("run_index", "maintenance")
            .await?
            .unwrap_or_default();
        if meta.version != 0 && meta.version != DATA_VERSION {
            return Err(Error::Corrupt);
        }
        meta.version = DATA_VERSION;
        let expires_at = now.checked_add(DAY).ok_or(Error::Interaction)?;
        let payload = match request {
            Request::Repost {
                run_id,
                expected_revision,
            } => {
                let mut stored = self.load_run(&mut tx, run_id).await?;
                if actor.user_id != stored.run.owner_id && !actor.manage_all_runs {
                    return Err(domain::Error::Forbidden.into());
                }
                if stored.run.state.is_terminal() || stored.run.state == RunState::Draft {
                    return Err(domain::Error::Closed.into());
                }
                if stored.run.desired_card_revision != *expected_revision {
                    return Err(domain::Error::StaleRevision.into());
                }
                if now < stored.run.updated_at {
                    return Err(domain::Error::InvalidInput.into());
                }
                let intent = stored.publication.as_mut().ok_or(Error::Corrupt)?;
                intent.repost_generation = intent
                    .repost_generation
                    .checked_add(1)
                    .ok_or(Error::Limit)?;
                stored.run.desired_card_revision = stored
                    .run
                    .desired_card_revision
                    .checked_add(1)
                    .ok_or(Error::Limit)?;
                stored.run.updated_at = now;
                if actor.user_id == stored.run.owner_id {
                    stored.run.last_owner_edit_at = now;
                } else {
                    if stored.moderator_audit.len() >= 127 {
                        return Err(Error::Limit);
                    }
                    stored.moderator_audit.push(AuditEntry {
                        actor_id: actor.user_id.clone(),
                        action: "repost".into(),
                        at: now,
                        affected_member: None,
                    });
                }
                intent.desired_revision = stored.run.desired_card_revision;
                let (card, actions) = super::ui::public_projection(&stored.run);
                intent.card = card;
                intent.actions = actions;
                meta.pending.insert(run_id.clone());
                tx.put("runs", run_id, &stored).await?;
                ReceiptPayload::Outcome {
                    result: Ok(MutationResult {
                        run_id: run_id.clone(),
                        outcome: Outcome {
                            changed: true,
                            state: stored.run.state,
                            card_revision: stored.run.desired_card_revision,
                        },
                    }),
                }
            }
            Request::Create { mode, name } => {
                let mut live: Live = tx.get("run_index", "live").await?.unwrap_or_default();
                if let Some((id, _)) = live
                    .iter()
                    .find(|(_, e)| e.state == RunState::Draft && e.owner == actor.user_id)
                {
                    let stored = self.load_run(&mut tx, id).await?;
                    if stored.run.state != RunState::Draft || stored.run.owner_id != actor.user_id {
                        return Err(Error::Corrupt);
                    }
                    ReceiptPayload::Outcome {
                        result: Ok(MutationResult {
                            run_id: id.clone(),
                            outcome: Outcome {
                                changed: false,
                                state: RunState::Draft,
                                card_revision: stored.run.desired_card_revision,
                            },
                        }),
                    }
                } else {
                    if live.values().filter(|e| e.state == RunState::Draft).count()
                        >= self.limits.drafts_per_guild
                        || live
                            .values()
                            .filter(|e| e.state == RunState::Draft && e.owner == actor.user_id)
                            .count()
                            >= self.limits.drafts_per_owner
                        || meta.terminal_count + live.len() >= self.limits.retained_terminal
                        || meta.tombstone_count >= self.limits.tombstones
                    {
                        return Err(Error::Limit);
                    }
                    let mut choice = None;
                    for _ in 0..5 {
                        let id = new_id()?;
                        let existing: Option<StoredRun> = tx.get("runs", &id).await?;
                        let tomb_key = tombstone_bucket(&id);
                        let tombstones: BTreeMap<String, u64> =
                            tx.get("run_index", &tomb_key).await?.unwrap_or_default();
                        if existing.is_none() && !tombstones.contains_key(&id) {
                            // CAS even an unchanged shard prevents concurrent GC/create ABA.
                            tx.put("run_index", &tomb_key, &tombstones).await?;
                            choice = Some(id);
                            break;
                        }
                    }
                    let id = choice.ok_or(Error::Busy)?;
                    match Run::new(id.clone(), actor, *mode, name.clone(), current.clone(), now) {
                        Ok(run) => {
                            live.insert(
                                id.clone(),
                                LiveEntry {
                                    owner: run.owner_id.clone(),
                                    state: run.state,
                                    last_owner_edit_at: run.last_owner_edit_at,
                                },
                            );
                            let result = MutationResult {
                                run_id: id.clone(),
                                outcome: Outcome {
                                    changed: true,
                                    state: run.state,
                                    card_revision: run.desired_card_revision,
                                },
                            };
                            tx.put(
                                "runs",
                                &id,
                                &StoredRun {
                                    schema_version: DATA_VERSION,
                                    run,
                                    publication: None,
                                    moderator_audit: Vec::new(),
                                },
                            )
                            .await?;
                            tx.put("run_index", "live", &live).await?;
                            ReceiptPayload::Outcome { result: Ok(result) }
                        }
                        Err(error) => ReceiptPayload::Outcome { result: Err(error) },
                    }
                }
            }
            Request::Prepare { run_id, command } => {
                let stored = self.load_run(&mut tx, run_id).await?;
                if stored.run.owner_id != actor.user_id && !actor.manage_all_runs {
                    return Err(domain::Error::Forbidden.into());
                }
                if !requires_confirmation(&stored.run, command) {
                    return Err(domain::Error::InvalidInput.into());
                }
                let token = random(16)?.iter().map(|b| format!("{b:02x}")).collect();
                ReceiptPayload::Challenge {
                    run_id: run_id.clone(),
                    revision: stored.run.desired_card_revision,
                    command_hash: hash(command)?,
                    token,
                    valid_until: now + 5 * 60_000,
                    used: false,
                }
            }
            Request::Change {
                run_id,
                command,
                confirmation,
                expected_revision,
            } => {
                let mut stored = self.load_run(&mut tx, run_id).await?;
                if expected_revision
                    .is_some_and(|revision| revision != stored.run.desired_card_revision)
                {
                    return Err(domain::Error::StaleRevision.into());
                }
                let mut command = command.clone();
                if requires_confirmation(&stored.run, &command) {
                    let token = confirmation
                        .as_ref()
                        .ok_or(domain::Error::ConfirmationRequired)?;
                    let mut challenge: Receipt = tx
                        .get("run_receipts", &token.interaction_id)
                        .await?
                        .ok_or(domain::Error::StaleConfirmation)?;
                    if challenge.user != actor.user_id {
                        return Err(domain::Error::StaleConfirmation.into());
                    }
                    match &mut challenge.payload {
                        ReceiptPayload::Challenge {
                            run_id: id,
                            revision,
                            command_hash,
                            token: expected,
                            valid_until,
                            used,
                        } if id == run_id
                            && *revision == stored.run.desired_card_revision
                            && *command_hash == hash(&command)?
                            && expected == &token.token
                            && *valid_until > now
                            && !*used =>
                        {
                            let checked = Confirmation {
                                actor_id: actor.user_id.clone(),
                                revision: *revision,
                            };
                            match &mut command {
                                Command::Cancel { confirmation }
                                | Command::Remove { confirmation, .. } => *confirmation = checked,
                                Command::SetMode { confirmation, .. } => {
                                    *confirmation = Some(checked)
                                }
                                _ => return Err(domain::Error::InvalidInput.into()),
                            }
                            *used = true;
                        }
                        _ => return Err(domain::Error::StaleConfirmation.into()),
                    }
                    tx.put("run_receipts", &token.interaction_id, &challenge)
                        .await?;
                }
                match domain::apply(&stored.run, actor, &command, current, now) {
                    Ok((run, outcome)) => {
                        if outcome.changed {
                            if actor.manage_all_runs
                                && actor.user_id != run.owner_id
                                && !matches!(
                                    command,
                                    Command::Join { .. } | Command::Switch { .. } | Command::Leave
                                )
                            {
                                let audit_limit = if run.state.is_terminal() { 128 } else { 127 };
                                if stored.moderator_audit.len() >= audit_limit {
                                    return Err(Error::Limit);
                                }
                                let encoded =
                                    serde_json::to_value(&command).map_err(|_| Error::Corrupt)?;
                                stored.moderator_audit.push(AuditEntry {
                                    actor_id: actor.user_id.clone(),
                                    action: encoded["action"]
                                        .as_str()
                                        .ok_or(Error::Corrupt)?
                                        .into(),
                                    at: now,
                                    affected_member: match &command {
                                        Command::Remove { member_id, .. } => {
                                            Some(member_id.clone())
                                        }
                                        _ => None,
                                    },
                                });
                            }
                            self.update_indexes(&mut tx, &mut meta, &stored.run, &run, now)
                                .await?;
                            if stored.publication.is_some()
                                || matches!(
                                    run.state,
                                    RunState::Open | RunState::Locked | RunState::Completed
                                )
                            {
                                let created_at =
                                    stored.publication.as_ref().map_or(now, |p| p.created_at);
                                let (card, actions) = super::ui::public_projection(&run);
                                stored.publication = Some(PublicationIntent {
                                    key: run.id.clone(),
                                    desired_revision: run.desired_card_revision,
                                    repost_generation: stored
                                        .publication
                                        .as_ref()
                                        .map_or(0, |p| p.repost_generation),
                                    destination: "runs".into(),
                                    created_at,
                                    card,
                                    actions,
                                });
                                meta.pending.insert(run.id.clone());
                            }
                            stored.run = run;
                            tx.put("runs", run_id, &stored).await?;
                        }
                        ReceiptPayload::Outcome {
                            result: Ok(MutationResult {
                                run_id: run_id.clone(),
                                outcome,
                            }),
                        }
                    }
                    Err(error) => ReceiptPayload::Outcome { result: Err(error) },
                }
            }
        };
        let receipt = Receipt {
            user: actor.user_id.clone(),
            hash: fingerprint.into(),
            expires_at,
            payload,
        };
        self.receipt(&mut tx, &mut meta, interaction, &receipt)
            .await?;
        tx.commit().await?;
        Self::replay(receipt, actor, interaction, fingerprint, now)
    }
    async fn update_indexes(
        &self,
        tx: &mut Transaction<'_, D>,
        meta: &mut Maintenance,
        old: &Run,
        new: &Run,
        now: u64,
    ) -> Result<()> {
        if old.state == new.state && old.last_owner_edit_at == new.last_owner_edit_at {
            return Ok(());
        }
        let mut live: Live = tx.get("run_index", "live").await?.unwrap_or_default();
        if !live.contains_key(&old.id) && !old.state.is_terminal() {
            return Err(Error::Corrupt);
        }
        if old.state == RunState::Draft && new.state == RunState::Open {
            if live
                .values()
                .filter(|e| e.state != RunState::Draft && e.owner == new.owner_id)
                .count()
                >= self.limits.published_per_owner
            {
                return Err(Error::OwnerPublishedLimit {
                    limit: self.limits.published_per_owner,
                });
            }
            if live.values().filter(|e| e.state != RunState::Draft).count()
                >= self.limits.published_per_guild
            {
                return Err(Error::Limit);
            }
            let day = now / DAY;
            meta.published_per_day
                .retain(|d, _| *d >= day.saturating_sub(1));
            let published = meta.published_per_day.entry(day).or_default();
            if *published >= self.limits.published_per_day {
                return Err(Error::Limit);
            }
            *published += 1;
        }
        if !old.state.is_terminal() && new.state.is_terminal() {
            if meta.terminal_count >= 700 {
                return Err(Error::Limit);
            }
            live.remove(&new.id);
            let day = new.terminal_at.ok_or(Error::Corrupt)? / DAY;
            let key = format!("terminal/{day}");
            let mut bucket: BTreeSet<String> = tx.get("run_index", &key).await?.unwrap_or_default();
            if !bucket.insert(new.id.clone()) {
                return Err(Error::Corrupt);
            }
            meta.terminal_count += 1;
            meta.terminal_buckets.insert(day);
            tx.put("run_index", &key, &bucket).await?;
        } else if !new.state.is_terminal() {
            live.insert(
                new.id.clone(),
                LiveEntry {
                    owner: new.owner_id.clone(),
                    state: new.state,
                    last_owner_edit_at: new.last_owner_edit_at,
                },
            );
        }
        tx.put("run_index", "live", &live).await
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupReport {
    pub receipts: usize,
    pub drafts: usize,
    pub terminal: usize,
    pub tombstones: usize,
}
impl<D: Documents> RunService<D> {
    /// Called only with a fresh host maintenance lease. Unresolved publication
    /// intents are retained; elapsed time alone is never evidence of delivery.
    pub async fn cleanup(&self, now: u64, budget: usize) -> Result<CleanupReport> {
        if budget == 0 || budget > 32 {
            return Err(domain::Error::InvalidInput.into());
        }
        let mut report = CleanupReport::default();
        for _ in 0..budget {
            match self.prune_receipt(now).await {
                Ok(true) => report.receipts += 1,
                Ok(false) => break,
                Err(Error::Conflict) => continue,
                Err(error) => return Err(error),
            }
        }
        for _ in 0..budget {
            match self.prune_run(now).await {
                Ok(Some(true)) => report.drafts += 1,
                Ok(Some(false)) => report.terminal += 1,
                Ok(None) => break,
                Err(Error::Conflict) => continue,
                Err(error) => return Err(error),
            }
        }
        for shard in 0..16 {
            let mut tx = Transaction::new(&self.docs);
            let key = format!("tombstones/{shard}");
            let mut bucket: BTreeMap<String, u64> =
                tx.get("run_index", &key).await?.unwrap_or_default();
            let before = bucket.len();
            bucket.retain(|_, expiry| *expiry > now);
            if before == bucket.len() {
                continue;
            }
            let mut meta: Maintenance = tx
                .get("run_index", "maintenance")
                .await?
                .ok_or(Error::Corrupt)?;
            meta.tombstone_count = meta
                .tombstone_count
                .checked_sub(before - bucket.len())
                .ok_or(Error::Corrupt)?;
            tx.put("run_index", &key, &bucket).await?;
            tx.put("run_index", "maintenance", &meta).await?;
            match tx.commit().await {
                Ok(()) => report.tombstones += before - bucket.len(),
                Err(Error::Conflict) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }
    async fn prune_receipt(&self, now: u64) -> Result<bool> {
        let mut tx = Transaction::new(&self.docs);
        let mut meta: Maintenance = tx
            .get("run_index", "maintenance")
            .await?
            .unwrap_or_default();
        // Old expiry-hour buckets come first numerically, not lexicographically.
        let mut keys: Vec<_> = meta
            .receipt_buckets
            .iter()
            .filter_map(|key| {
                key.split('/')
                    .nth(1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|hour| (hour, key.clone()))
            })
            .collect();
        keys.sort();
        for (hour, key) in keys {
            if hour > now / 3_600_000 {
                break;
            }
            let mut bucket: BTreeMap<String, u64> =
                tx.get("run_index", &key).await?.ok_or(Error::Corrupt)?;
            if let Some((id, expiry)) = bucket
                .iter()
                .find(|(_, expiry)| **expiry <= now)
                .map(|(id, expiry)| (id.clone(), *expiry))
            {
                let receipt: Receipt = tx.get("run_receipts", &id).await?.ok_or(Error::Corrupt)?;
                if receipt.expires_at != expiry {
                    return Err(Error::Corrupt);
                }
                bucket.remove(&id);
                meta.receipt_count = meta.receipt_count.checked_sub(1).ok_or(Error::Corrupt)?;
                tx.delete("run_receipts", &id).await?;
                if bucket.is_empty() {
                    meta.receipt_buckets.remove(&key);
                    tx.delete("run_index", &key).await?;
                } else {
                    tx.put("run_index", &key, &bucket).await?;
                }
                tx.put("run_index", "maintenance", &meta).await?;
                tx.commit().await?;
                return Ok(true);
            }
        }
        Ok(false)
    }
    async fn prune_run(&self, now: u64) -> Result<Option<bool>> {
        let mut tx = Transaction::new(&self.docs);
        let mut meta: Maintenance = tx
            .get("run_index", "maintenance")
            .await?
            .unwrap_or_default();
        if meta.tombstone_count >= 1024 {
            return Ok(None);
        }
        let mut live: Live = tx.get("run_index", "live").await?.unwrap_or_default();
        if let Some(id) = live
            .iter()
            .find(|(_, e)| {
                e.state == RunState::Draft && now.saturating_sub(e.last_owner_edit_at) >= DAY
            })
            .map(|(id, _)| id.clone())
        {
            let run = self.load_run(&mut tx, &id).await?;
            if run.run.state != RunState::Draft
                || now.saturating_sub(run.run.last_owner_edit_at) < DAY
                || run.publication.is_some()
            {
                return Err(Error::Corrupt);
            }
            live.remove(&id);
            tx.put("run_index", "live", &live).await?;
            self.retire(&mut tx, &mut meta, &id, now).await?;
            tx.commit().await?;
            return Ok(Some(true));
        }
        // Visit the boundary bucket too; retention is exactly thirty days.
        for day in meta.terminal_buckets.clone() {
            if day > now.saturating_sub(30 * DAY) / DAY {
                break;
            }
            let key = format!("terminal/{day}");
            let mut bucket: BTreeSet<String> =
                tx.get("run_index", &key).await?.ok_or(Error::Corrupt)?;
            for id in bucket.clone() {
                if meta.pending.contains(&id) {
                    continue;
                }
                let run = self.load_run(&mut tx, &id).await?;
                if !run.run.state.is_terminal() || run.run.terminal_at.is_none() {
                    return Err(Error::Corrupt);
                }
                if now.saturating_sub(run.run.terminal_at.ok_or(Error::Corrupt)?) < 30 * DAY {
                    continue;
                }
                bucket.remove(&id);
                meta.terminal_count = meta.terminal_count.checked_sub(1).ok_or(Error::Corrupt)?;
                if bucket.is_empty() {
                    meta.terminal_buckets.remove(&day);
                    tx.delete("run_index", &key).await?;
                } else {
                    tx.put("run_index", &key, &bucket).await?;
                }
                self.retire(&mut tx, &mut meta, &id, now).await?;
                tx.commit().await?;
                return Ok(Some(false));
            }
        }
        Ok(None)
    }
    async fn retire(
        &self,
        tx: &mut Transaction<'_, D>,
        meta: &mut Maintenance,
        id: &str,
        now: u64,
    ) -> Result<()> {
        let key = tombstone_bucket(id);
        let mut tombstones: BTreeMap<String, u64> =
            tx.get("run_index", &key).await?.unwrap_or_default();
        if tombstones
            .insert(id.into(), now.checked_add(30 * DAY).ok_or(Error::Corrupt)?)
            .is_some()
        {
            return Err(Error::Corrupt);
        }
        meta.tombstone_count += 1;
        tx.delete("runs", id).await?;
        tx.put("run_index", &key, &tombstones).await?;
        tx.put("run_index", "maintenance", meta).await
    }
}

/// SDK callbacks retain the invocation's host-owned guild and cancellation.
#[async_trait]
impl Documents for oracle_module_sdk::CallContext {
    fn guild(&self) -> &str {
        self.guild().as_str()
    }
    async fn get(&self, collection: &str, key: &str) -> Result<Option<ModuleDocument>> {
        self.document_get(collection, key)
            .await
            .map_err(callback_error)
    }
    async fn batch(&self, writes: Vec<DocumentWrite>) -> Result<()> {
        self.document_batch(writes)
            .await
            .map(|_| ())
            .map_err(callback_error)
    }
}
fn callback_error(error: oracle_module_sdk::RpcError) -> Error {
    match error {
        oracle_module_sdk::RpcError::Remote(code) if code == "Conflict" => Error::Conflict,
        _ => Error::Unavailable,
    }
}

/// Upgrade immutable v2 run documents to include their exact public projection.
/// Other stored indexes/receipts/tombstones retain their data and identity.
pub fn migrate_v2_document(document: ModuleDocument) -> Result<DocumentWrite> {
    let mut value = document.value;
    match document.collection.as_str() {
        "runs" => {
            if value["schema_version"] != 2 {
                return Err(Error::Corrupt);
            }
            let run: Run =
                serde_json::from_value(value["run"].clone()).map_err(|_| Error::Corrupt)?;
            run.validate().map_err(|_| Error::Corrupt)?;
            if !value["publication"].is_null() {
                let (card, actions) = super::ui::public_projection(&run);
                value["publication"]["repost_generation"] = serde_json::json!(0);
                value["publication"]["card"] = card;
                value["publication"]["actions"] =
                    serde_json::to_value(actions).map_err(|_| Error::Corrupt)?;
            }
            value["schema_version"] = serde_json::json!(3);
            let stored: StoredRun =
                serde_json::from_value(value.clone()).map_err(|_| Error::Corrupt)?;
            stored.validate_version(&run.guild_id, &document.key, 3)?;
            if serde_json::to_vec(&value)
                .map_err(|_| Error::Corrupt)?
                .len()
                > MAX_AGGREGATE_BYTES
            {
                return Err(Error::Limit);
            }
        }
        "run_index" if document.key == "maintenance" => {
            if value["version"] != 2 {
                return Err(Error::Corrupt);
            }
            value["version"] = serde_json::json!(3);
        }
        "run_index" | "run_receipts" => (),
        _ => return Err(Error::Corrupt),
    }
    Ok(DocumentWrite {
        collection: document.collection,
        key: document.key,
        expected_revision: Some(document.revision),
        value: Some(value),
    })
}
impl<D: Documents> RunService<D> {
    pub async fn pending_projections(&self) -> Result<Vec<String>> {
        let mut tx = Transaction::new(&self.docs);
        let meta: Maintenance = tx
            .get("run_index", "maintenance")
            .await?
            .unwrap_or_default();
        if meta.pending.len() > 700 {
            return Err(Error::Corrupt);
        }
        Ok(meta.pending.into_iter().collect())
    }
    pub async fn projection(&self, id: &str) -> Result<Value> {
        let id = domain::normalize_run_id(id)?;
        let document = self.docs.get("runs", &id).await?.ok_or(Error::NotFound)?;
        let stored: StoredRun =
            serde_json::from_value(document.value).map_err(|_| Error::Corrupt)?;
        stored.validate(self.docs.guild(), &id)?;
        Ok(
            serde_json::json!({"id":id,"expected_revision":document.revision,"terminal":stored.run.state.is_terminal(),"intent":stored.publication}),
        )
    }
}

impl<D: Documents> RunService<D> {
    /// Called only after a scoped host status callback confirms this desired revision.
    /// Retains the intent; later mutations requeue it. No member route exposes this.
    pub async fn release_confirmed(&self, id: &str, confirmed_revision: u64) -> Result<bool> {
        for _ in 0..3 {
            let mut tx = Transaction::new(&self.docs);
            let stored = self.load_run(&mut tx, id).await?;
            let Some(intent) = &stored.publication else {
                return Ok(false);
            };
            if intent.desired_revision > confirmed_revision {
                return Ok(false);
            }
            let mut meta: Maintenance = tx
                .get("run_index", "maintenance")
                .await?
                .ok_or(Error::Corrupt)?;
            if !meta.pending.remove(id) {
                return Ok(false);
            }
            // Include the run's exact read in the CAS so an edit cannot lose its pending flag.
            tx.put("runs", id, &stored).await?;
            tx.put("run_index", "maintenance", &meta).await?;
            match tx.commit().await {
                Ok(()) => return Ok(true),
                Err(Error::Conflict) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Busy)
    }
}

/// Preserve existing runs without inventing a date, and refresh their public schedule fields.
pub fn migrate_v3_document(document: ModuleDocument) -> Result<DocumentWrite> {
    let mut value = document.value;
    match document.collection.as_str() {
        "runs" => {
            let mut stored: StoredRun =
                serde_json::from_value(value).map_err(|_| Error::Corrupt)?;
            stored.validate_version(&stored.run.guild_id, &document.key, 3)?;
            stored.schema_version = DATA_VERSION;
            if let Some(intent) = &mut stored.publication {
                stored.run.desired_card_revision = stored
                    .run
                    .desired_card_revision
                    .checked_add(1)
                    .ok_or(Error::Corrupt)?;
                let (card, actions) = super::ui::public_projection(&stored.run);
                intent.desired_revision = stored.run.desired_card_revision;
                intent.card = card;
                intent.actions = actions;
            }
            stored.validate(&stored.run.guild_id, &document.key)?;
            value = serde_json::to_value(stored).map_err(|_| Error::Corrupt)?;
        }
        "run_index" if document.key == "maintenance" => {
            if value["version"] != 3 {
                return Err(Error::Corrupt);
            }
            value["version"] = serde_json::json!(DATA_VERSION);
        }
        "run_index" | "run_receipts" => (),
        _ => return Err(Error::Corrupt),
    }
    Ok(DocumentWrite {
        collection: document.collection,
        key: document.key,
        expected_revision: Some(document.revision),
        value: Some(value),
    })
}
