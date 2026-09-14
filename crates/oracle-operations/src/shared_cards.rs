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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub desired: SharedIntent,
    pub phase: SharedPhase,
    pub identity: Option<SharedIdentity>,
    pub confirmed_revision: Option<u64>,
    pub effect: Option<SharedEffect>,
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
        self.repository
            .workflow_get(guild, WorkflowKind::SharedCard, &Self::key(module, run_id))
            .await?
            .map(decode)
            .transpose()
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
            || run_id.len() > 128
            || intent.desired_revision == 0
            || intent.key != run_id
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        for _ in 0..8 {
            let existing = self.get(guild, module, run_id).await?;
            let (revision, mut record) = match existing {
                Some((revision, record)) => (Some(revision), record),
                None => (
                    None,
                    SharedRecord {
                        module: module.clone(),
                        run_id: run_id.into(),
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
            if saved.effect_id != effect.effect_id || record.phase != SharedPhase::Unknown {
                return Err(Error::new(ErrorCode::Conflict));
            }
            match &observation {
                SharedObservation::Unknown => return Ok(record),
                SharedObservation::Confirmed { message_id } => {
                    if message_id.is_empty()
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
                    record.phase = SharedPhase::Missing
                }
                SharedObservation::Missing => return Err(Error::new(ErrorCode::Integrity)),
                SharedObservation::Rejected => record.phase = SharedPhase::Rejected,
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
    /// Used only after a fresh authorized explicit repost action.
    pub async fn repost(&self, guild: &GuildId, module: &ModuleId, run_id: &str) -> Result<()> {
        let (revision, mut record) = self
            .get(guild, module, run_id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if record.phase != SharedPhase::Missing || record.effect.is_some() {
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
