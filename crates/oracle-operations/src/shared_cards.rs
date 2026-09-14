//! Durable shared-message boundary. Module data cannot select remote identities.
use async_trait::async_trait;
use oracle_core::{GuildId, ModuleId, PrivateCardBody, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedIntent {
    pub key: String,
    pub desired_revision: u64,
    #[serde(default)]
    pub repost_generation: u32,
    /// Permanently remove this publication; never an implicit repost.
    #[serde(default)]
    pub delete: bool,
    pub destination: String,
    pub created_at: u64,
    pub card: PrivateCardBody,
    pub actions: Vec<SharedAction>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedAction {
    pub name: String,
    pub label: String,
    pub operation: String,
    pub input: Map<String, Value>,
}
/// Resolved exclusively by host configuration and current remote authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedTarget {
    pub channel_id: String,
    pub application_id: String,
    pub bot_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedIdentity {
    pub target: SharedTarget,
    pub message_id: String,
    pub control_version: String,
}
/// Frozen before a remote request; observations compare this exact payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedEffect {
    pub effect_id: String,
    pub guild: GuildId,
    pub module: ModuleId,
    pub run_id: String,
    pub target: SharedTarget,
    pub message_id: Option<String>,
    pub desired_revision: u64,
    pub marker: String,
    pub payload: Value,
}
#[derive(Clone, Debug)]
pub enum SharedObservation {
    /// Exact bot author, channel, marker and payload were observed.
    Confirmed { message_id: String },
    /// Only applicable to an existing message with a definite 404.
    Missing,
    /// The remote request was definitively rejected, with no side effect.
    Rejected,
    /// Transport proved that no HTTP request was submitted. Safe to retry.
    NotSent { reason: ErrorCode },
    /// Includes incomplete history, multiple matches and inaccessible channels.
    Unknown,
}
#[async_trait]
pub trait SharedCardTransport: Send + Sync {
    /// Must recheck current module/guild authority immediately before sending.
    /// Errors after dispatch are uncertain, never evidence that nothing happened.
    async fn execute(
        &self,
        effect: &SharedEffect,
        cancel: CancellationToken,
    ) -> Result<SharedObservation>;
    /// Read-only recovery. Missing history never proves an absent create.
    async fn observe(
        &self,
        effect: &SharedEffect,
        cancel: CancellationToken,
    ) -> Result<SharedObservation>;
}

use oracle_core::{Error, ErrorCode, WorkflowKind, WorkflowRecord, WorkflowRepository};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const MAX_JOURNAL_BYTES: usize = 16 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SharedPhase {
    Pending,
    Confirmed,
    Unknown,
    Missing,
    Rejected,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedRecord {
    pub module: ModuleId,
    pub run_id: String,
    secret: ControlSecret,
    pub desired: SharedIntent,
    pub phase: SharedPhase,
    pub identity: Option<SharedIdentity>,
    pub confirmed_revision: Option<u64>,
    pub effect: Option<SharedEffect>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ControlSecret([u8; 32]);
impl std::fmt::Debug for ControlSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
impl ControlSecret {
    fn new() -> Self {
        let mut bytes = [0; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }
}
impl SharedRecord {
    fn control_mac(
        &self,
        guild: &GuildId,
        version: &str,
        action: usize,
    ) -> Result<hmac::Hmac<Sha256>> {
        use hmac::Mac;
        let action = self
            .desired
            .actions
            .get(action)
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
        let input = serde_json::to_vec(&(guild, &self.module, &self.run_id, version, action))
            .map_err(|_| Error::new(ErrorCode::Integrity))?;
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&self.secret.0)
            .map_err(|_| Error::new(ErrorCode::Integrity))?;
        mac.update(&input);
        Ok(mac)
    }
    pub fn control_id(&self, guild: &GuildId, version: &str, action: usize) -> Result<String> {
        use hmac::Mac;
        if action > 4 {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let tag = self
            .control_mac(guild, version, action)?
            .finalize()
            .into_bytes();
        let tag: String = tag[..12].iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!(
            "os:{}:{action}:{tag}",
            SharedCardJournal::key(&self.module, &self.run_id)
        ))
    }
    pub fn verify_control(
        &self,
        guild: &GuildId,
        channel: &str,
        message: &str,
        application: &str,
        author: &str,
        custom_id: &str,
    ) -> Result<SharedAction> {
        use hmac::Mac;
        let forbidden = || Error::new(ErrorCode::ForbiddenScope);
        if self.desired.delete {
            return Err(forbidden());
        }
        let identity = self.identity.as_ref().ok_or_else(forbidden)?;
        if identity.target.channel_id != channel
            || identity.message_id != message
            || identity.target.application_id != application
            || identity.target.bot_id != author
        {
            return Err(forbidden());
        }
        let parts: Vec<_> = custom_id.split(':').collect();
        if parts.len() != 4
            || parts[0] != "os"
            || parts[1] != SharedCardJournal::key(&self.module, &self.run_id)
            || parts[2].len() != 1
            || parts[3].len() != 24
            || !parts[3].is_ascii()
        {
            return Err(forbidden());
        }
        let index: usize = parts[2].parse().map_err(|_| forbidden())?;
        if index > 4 {
            return Err(forbidden());
        }
        let mut tag = [0; 12];
        for (i, byte) in tag.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&parts[3][i * 2..i * 2 + 2], 16).map_err(|_| forbidden())?;
        }
        self.control_mac(guild, &identity.control_version, index)?
            .verify_truncated_left(&tag)
            .map_err(|_| forbidden())?;
        self.desired
            .actions
            .get(index)
            .cloned()
            .ok_or_else(forbidden)
    }
}
/// CAS is the serialization boundary, including across a host restart. A saved
/// unknown effect blocks replacement until a positive observation settles it.
pub struct SharedCardJournal {
    repository: Arc<dyn WorkflowRepository>,
}
impl SharedCardJournal {
    pub fn new(repository: Arc<dyn WorkflowRepository>) -> Self {
        Self { repository }
    }
    pub fn key(module: &ModuleId, run_id: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(module.as_str());
        hash.update([0]);
        hash.update(run_id);
        format!("{:x}", hash.finalize())
    }
    pub async fn get(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        run_id: &str,
    ) -> Result<Option<(u64, SharedRecord)>> {
        let found = self
            .repository
            .workflow_get(guild, WorkflowKind::SharedCard, &Self::key(module, run_id))
            .await?
            .map(decode)
            .transpose()?;
        if found
            .as_ref()
            .is_some_and(|(_, record)| &record.module != module || record.run_id != run_id)
        {
            return Err(Error::new(ErrorCode::Integrity));
        }
        Ok(found)
    }
    pub async fn resolve_control(&self, guild: &GuildId, custom_id: &str) -> Result<SharedRecord> {
        let parts: Vec<_> = custom_id.split(':').collect();
        if parts.len() != 4
            || parts[0] != "os"
            || parts[1].len() != 64
            || !parts[1].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let (_, record) = self
            .repository
            .workflow_get(guild, WorkflowKind::SharedCard, parts[1])
            .await?
            .map(decode)
            .transpose()?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if Self::key(&record.module, &record.run_id) != parts[1] {
            return Err(Error::new(ErrorCode::Integrity));
        }
        Ok(record)
    }
    async fn save(
        &self,
        guild: &GuildId,
        revision: Option<u64>,
        value: &SharedRecord,
    ) -> Result<u64> {
        let value_json =
            serde_json::to_value(value).map_err(|_| Error::new(ErrorCode::Integrity))?;
        if serde_json::to_vec(&value_json)
            .map_err(|_| Error::new(ErrorCode::Integrity))?
            .len()
            > MAX_JOURNAL_BYTES
        {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        Ok(self
            .repository
            .workflow_put(
                guild,
                WorkflowKind::SharedCard,
                &Self::key(&value.module, &value.run_id),
                revision,
                &value_json,
            )
            .await?
            .revision)
    }
    async fn reserve(&self, guild: &GuildId, module: &ModuleId, run_id: &str) -> Result<()> {
        // Reserve a bounded slot before creating its journal. A crash between
        // these writes retains the reservation and cannot exceed the quota.
        let key = format!("index:{}", Self::key(module, ""));
        for _ in 0..8 {
            let existing = self
                .repository
                .workflow_get(guild, WorkflowKind::SharedCard, &key)
                .await?;
            let revision = existing.as_ref().map(|row| row.revision);
            let mut runs: std::collections::BTreeSet<String> = existing
                .map(|row| {
                    serde_json::from_value(row.value).map_err(|_| Error::new(ErrorCode::Integrity))
                })
                .transpose()?
                .unwrap_or_default();
            if runs.contains(run_id) {
                return Ok(());
            }
            if runs.len() >= 700 {
                return Err(Error::new(ErrorCode::QuotaExceeded));
            }
            runs.insert(run_id.into());
            match self
                .repository
                .workflow_put(
                    guild,
                    WorkflowKind::SharedCard,
                    &key,
                    revision,
                    &serde_json::to_value(runs).map_err(|_| Error::new(ErrorCode::Integrity))?,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(ErrorCode::Conflict))
    }
    pub async fn run_ids(&self, guild: &GuildId, module: &ModuleId) -> Result<Vec<String>> {
        let key = format!("index:{}", Self::key(module, ""));
        self.repository
            .workflow_get(guild, WorkflowKind::SharedCard, &key)
            .await?
            .map(|row| {
                serde_json::from_value(row.value).map_err(|_| Error::new(ErrorCode::Integrity))
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }
    /// The caller has already loaded and validated the manifest-declared intent.
    /// Repeating a revision is idempotent only when its complete intent agrees.
    pub async fn enqueue(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        run_id: &str,
        intent: SharedIntent,
    ) -> Result<SharedRecord> {
        if run_id.is_empty()
            || run_id.len() > 64
            || intent.desired_revision == 0
            || intent.key != run_id
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        self.reserve(guild, module, run_id).await?;
        for _ in 0..8 {
            let existing = self.get(guild, module, run_id).await?;
            let (revision, mut record) = match existing {
                Some((revision, record)) => (Some(revision), record),
                None => (
                    None,
                    SharedRecord {
                        module: module.clone(),
                        run_id: run_id.into(),
                        secret: ControlSecret::new(),
                        desired: intent.clone(),
                        phase: SharedPhase::Pending,
                        identity: None,
                        confirmed_revision: None,
                        effect: None,
                    },
                ),
            };
            if revision.is_some() && intent.desired_revision <= record.desired.desired_revision {
                if intent.desired_revision == record.desired.desired_revision
                    && serde_json::to_value(&intent).ok()
                        != serde_json::to_value(&record.desired).ok()
                {
                    return Err(Error::new(ErrorCode::Conflict));
                }
                return Ok(record);
            }
            if revision.is_some() && record.desired.delete && !intent.delete {
                return Err(Error::new(ErrorCode::Conflict));
            }
            if revision.is_some() && intent.repost_generation != record.desired.repost_generation {
                if intent.delete || record.desired.delete {
                    return Err(Error::new(ErrorCode::Conflict));
                }
                if record.phase != SharedPhase::Missing
                    || record.effect.is_some()
                    || record.desired.repost_generation.checked_add(1)
                        != Some(intent.repost_generation)
                {
                    return Err(Error::new(ErrorCode::Conflict));
                }
                record.identity = None;
                record.confirmed_revision = None;
                record.phase = SharedPhase::Pending;
            } else if revision.is_none() && intent.repost_generation != 0 {
                return Err(Error::new(ErrorCode::Conflict));
            }
            record.desired = intent.clone();
            if !matches!(record.phase, SharedPhase::Unknown | SharedPhase::Missing) {
                record.phase = SharedPhase::Pending;
            }
            match self.save(guild, revision, &record).await {
                Ok(_) => return Ok(record),
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(ErrorCode::Conflict))
    }
    /// Freeze one exact effect before its socket write. A crash from this point
    /// requires observation, even if no bytes were actually sent.
    pub async fn prepare(&self, effect: SharedEffect) -> Result<()> {
        let (revision, mut record) = self
            .get(&effect.guild, &effect.module, &effect.run_id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if record.phase != SharedPhase::Pending
            || record.effect.is_some()
            || effect.desired_revision != record.desired.desired_revision
            || effect.effect_id.is_empty()
            || effect.marker.is_empty()
            || effect.payload.is_null() != record.desired.delete
            || (record.desired.delete && record.identity.is_none())
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        match &record.identity {
            Some(identity)
                if effect.message_id.as_deref() == Some(&identity.message_id)
                    && effect.target == identity.target => {}
            None if effect.message_id.is_none() => {}
            _ => return Err(Error::new(ErrorCode::ForbiddenScope)),
        }
        record.phase = SharedPhase::Unknown;
        record.effect = Some(effect.clone());
        self.save(&effect.guild, Some(revision), &record).await?;
        Ok(())
    }
    pub async fn settle(
        &self,
        effect: &SharedEffect,
        observation: SharedObservation,
    ) -> Result<SharedRecord> {
        for _ in 0..8 {
            let (revision, mut record) = self
                .get(&effect.guild, &effect.module, &effect.run_id)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            let saved = record
                .effect
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::Conflict))?;
            if saved != effect || record.phase != SharedPhase::Unknown {
                return Err(Error::new(ErrorCode::Conflict));
            }
            match &observation {
                SharedObservation::Unknown => return Ok(record),
                SharedObservation::Confirmed { message_id } => {
                    if effect.payload.is_null()
                        || message_id.is_empty()
                        || effect
                            .message_id
                            .as_ref()
                            .is_some_and(|id| id != message_id)
                    {
                        return Err(Error::new(ErrorCode::Integrity));
                    }
                    let version = record
                        .identity
                        .as_ref()
                        .map(|identity| identity.control_version.clone())
                        .unwrap_or_else(|| effect.effect_id.clone());
                    record.identity = Some(SharedIdentity {
                        target: effect.target.clone(),
                        message_id: message_id.clone(),
                        control_version: version,
                    });
                    record.confirmed_revision = Some(effect.desired_revision);
                    record.phase = if record.desired.desired_revision > effect.desired_revision {
                        SharedPhase::Pending
                    } else {
                        SharedPhase::Confirmed
                    };
                }
                SharedObservation::Missing if effect.message_id.is_some() => {
                    if effect.payload.is_null() {
                        record.confirmed_revision = Some(effect.desired_revision);
                        record.phase = if record.desired.desired_revision > effect.desired_revision
                        {
                            SharedPhase::Pending
                        } else {
                            SharedPhase::Confirmed
                        };
                    } else {
                        record.phase = SharedPhase::Missing;
                    }
                }
                SharedObservation::Missing => return Err(Error::new(ErrorCode::Integrity)),
                SharedObservation::Rejected => record.phase = SharedPhase::Rejected,
                SharedObservation::NotSent { .. } => record.phase = SharedPhase::Pending,
            }
            record.effect = None;
            match self.save(&effect.guild, Some(revision), &record).await {
                Ok(_) => return Ok(record),
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(ErrorCode::Conflict))
    }
    /// Complete cancellation without a remote write only if no create is in doubt,
    /// or an earlier authorized read already established that the message is absent.
    pub async fn complete_unpublished_delete(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        run_id: &str,
    ) -> Result<SharedRecord> {
        let (revision, mut record) = self
            .get(guild, module, run_id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if !record.desired.delete
            || record.effect.is_some()
            || record.phase == SharedPhase::Unknown
            || (record.identity.is_some() && record.phase != SharedPhase::Missing)
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        record.phase = SharedPhase::Confirmed;
        record.confirmed_revision = Some(record.desired.desired_revision);
        self.save(guild, Some(revision), &record).await?;
        Ok(record)
    }
    /// A read-only remote probe positively established that this exact message
    /// is absent while the bot still has access to its channel.
    pub async fn mark_missing(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        run_id: &str,
        message_id: &str,
    ) -> Result<()> {
        let (revision, mut record) = self
            .get(guild, module, run_id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if record.desired.delete
            || record.phase != SharedPhase::Confirmed
            || record.effect.is_some()
            || record
                .identity
                .as_ref()
                .is_none_or(|identity| identity.message_id != message_id)
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        record.phase = SharedPhase::Missing;
        self.save(guild, Some(revision), &record).await?;
        Ok(())
    }
    /// Used only after a fresh authorized explicit repost action.
    pub async fn repost(&self, guild: &GuildId, module: &ModuleId, run_id: &str) -> Result<()> {
        let (revision, mut record) = self
            .get(guild, module, run_id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if record.desired.delete || record.phase != SharedPhase::Missing || record.effect.is_some()
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        record.identity = None;
        record.confirmed_revision = None;
        record.phase = SharedPhase::Pending;
        self.save(guild, Some(revision), &record).await?;
        Ok(())
    }
}
fn decode(record: WorkflowRecord) -> Result<(u64, SharedRecord)> {
    let value =
        serde_json::from_value(record.value).map_err(|_| Error::new(ErrorCode::Integrity))?;
    Ok((record.revision, value))
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use oracle_storage::{DatabaseConfig, Storage};
    use serde_json::json;

    fn intent(revision: u64, delete: bool) -> SharedIntent {
        serde_json::from_value(json!({"key":"RUN1","desired_revision":revision,"delete":delete,
            "destination":"runs","created_at":1,"card":{"title":"Run","description":"Run","fields":[],"footer":""},"actions":[]})).unwrap()
    }
    fn effect(revision: u64, delete: bool, message: Option<&str>) -> SharedEffect {
        SharedEffect {
            effect_id: format!("effect-{revision}"),
            guild: "123".parse().unwrap(),
            module: "test.runs".parse().unwrap(),
            run_id: "RUN1".into(),
            target: SharedTarget {
                channel_id: "456".into(),
                application_id: "789".into(),
                bot_id: "789".into(),
            },
            message_id: message.map(str::to_owned),
            desired_revision: revision,
            marker: "marker".into(),
            payload: if delete {
                Value::Null
            } else {
                json!({"content":"Run"})
            },
        }
    }
    #[tokio::test]
    async fn cancelled_unknown_create_recovers_exact_message_then_deletes_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let config = DatabaseConfig::Sqlite {
            path: temp.path().join("cancel.sqlite"),
        };
        let store = Storage::open(config.clone()).await.unwrap();
        let create = effect(1, false, None);
        store
            .initialize_guilds(std::slice::from_ref(&create.guild))
            .await
            .unwrap();
        let journal = SharedCardJournal::new(Arc::new(store.clone()));
        journal
            .enqueue(&create.guild, &create.module, "RUN1", intent(1, false))
            .await
            .unwrap();
        journal.prepare(create.clone()).await.unwrap();
        journal
            .enqueue(&create.guild, &create.module, "RUN1", intent(2, true))
            .await
            .unwrap();
        assert!(
            journal
                .complete_unpublished_delete(&create.guild, &create.module, "RUN1")
                .await
                .is_err()
        );
        drop(journal);
        store.close().await.unwrap();
        drop(store);
        let store = Storage::open(config).await.unwrap();
        let journal = SharedCardJournal::new(Arc::new(store.clone()));
        let recovered = journal
            .settle(
                &create,
                SharedObservation::Confirmed {
                    message_id: "1000".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(recovered.phase, SharedPhase::Pending);
        assert_eq!(recovered.identity.unwrap().message_id, "1000");
        assert!(journal.prepare(effect(2, true, None)).await.is_err());
        assert!(
            journal
                .prepare(effect(2, true, Some("9999")))
                .await
                .is_err()
        );
        assert!(
            journal
                .prepare(effect(2, false, Some("1000")))
                .await
                .is_err()
        );
        let delete = effect(2, true, Some("1000"));
        let mut wrong_channel = delete.clone();
        wrong_channel.target.channel_id = "999".into();
        assert!(journal.prepare(wrong_channel).await.is_err());
        journal.prepare(delete.clone()).await.unwrap();
        assert!(
            journal
                .settle(
                    &delete,
                    SharedObservation::Confirmed {
                        message_id: "1000".into()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(
            journal
                .settle(&delete, SharedObservation::Unknown)
                .await
                .unwrap()
                .phase,
            SharedPhase::Unknown
        );
        let deleted = journal
            .settle(&delete, SharedObservation::Missing)
            .await
            .unwrap();
        assert_eq!(deleted.phase, SharedPhase::Confirmed);
        assert_eq!(deleted.confirmed_revision, Some(2));
        assert_eq!(deleted.identity.unwrap().message_id, "1000");
        assert!(
            journal
                .repost(&create.guild, &create.module, "RUN1")
                .await
                .is_err()
        );
        assert!(
            journal
                .enqueue(&create.guild, &create.module, "RUN1", intent(3, false))
                .await
                .is_err()
        );
        store.close().await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_before_send_completes_without_creating_message() {
        let temp = tempfile::tempdir().unwrap();
        let store = Storage::open(DatabaseConfig::Sqlite {
            path: temp.path().join("cancel.sqlite"),
        })
        .await
        .unwrap();
        let create = effect(1, false, None);
        store
            .initialize_guilds(std::slice::from_ref(&create.guild))
            .await
            .unwrap();
        let journal = SharedCardJournal::new(Arc::new(store.clone()));
        journal
            .enqueue(&create.guild, &create.module, "RUN1", intent(1, false))
            .await
            .unwrap();
        // A delete sentinel may never be prepared for a live publication or no target.
        assert!(journal.prepare(effect(1, true, None)).await.is_err());
        journal
            .enqueue(&create.guild, &create.module, "RUN1", intent(2, true))
            .await
            .unwrap();
        let cancelled = journal
            .complete_unpublished_delete(&create.guild, &create.module, "RUN1")
            .await
            .unwrap();
        assert_eq!(cancelled.phase, SharedPhase::Confirmed);
        assert_eq!(cancelled.confirmed_revision, Some(2));
        assert!(cancelled.identity.is_none());
        assert!(cancelled.effect.is_none());
        assert!(journal.prepare(effect(2, false, None)).await.is_err());
        store.close().await.unwrap();
    }
}
