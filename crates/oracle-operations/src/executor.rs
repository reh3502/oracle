//! Durable structure planning, exact approvals and readback over a typed host adapter.
use crate::structure::{
    Change, Channel, ChannelKind, Snapshot, Step, StructureRequest, build_steps,
};
use async_trait::async_trait;
use oracle_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn error(code: ErrorCode) -> Error {
    Error::new(code)
}
fn encode(value: &impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| error(ErrorCode::Integrity))
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| error(ErrorCode::Integrity))
}
fn actor(context: &PolicyContext) -> String {
    match context {
        PolicyContext::LocalOperator => "local_operator".into(),
        PolicyContext::Discord { user, .. } => format!("discord:{user}"),
    }
}

/// The adapter must use this gate at each actual request dispatch, after any wait.
/// Revocation and synchronous submission use one lock. It cannot recall a sent request.
#[derive(Clone)]
pub struct SendGuard {
    revoked: Arc<Mutex<bool>>,
    cancel: CancellationToken,
    expires_at: u64,
    fence: Option<Arc<dyn DispatchFence>>,
}
pub trait DispatchFence: Send + Sync {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()>;
}
impl SendGuard {
    pub fn new(cancel: CancellationToken, expires_at: u64) -> Self {
        Self {
            revoked: Arc::new(Mutex::new(false)),
            cancel,
            expires_at,
            fence: None,
        }
    }
    pub fn with_fence(
        cancel: CancellationToken,
        expires_at: u64,
        fence: Arc<dyn DispatchFence>,
    ) -> Self {
        Self {
            revoked: Arc::new(Mutex::new(false)),
            cancel,
            expires_at,
            fence: Some(fence),
        }
    }
    pub fn revoke(&self) {
        *self.revoked.lock().unwrap() = true;
    }
    pub fn dispatch<T>(&self, send: impl FnOnce() -> Result<T>) -> Result<T> {
        let revoked = self.revoked.lock().unwrap();
        if *revoked || self.cancel.is_cancelled() || now() >= self.expires_at {
            return Err(error(ErrorCode::Cancelled));
        }
        let mut send = Some(send);
        let mut output = None;
        let mut submit = || {
            output = Some(send.take().ok_or_else(|| error(ErrorCode::Integrity))?()?);
            Ok(())
        };
        if let Some(fence) = &self.fence {
            fence.dispatch(&mut submit)?;
        } else {
            submit()?;
        }
        output.ok_or_else(|| error(ErrorCode::Integrity))
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelMutation {
    pub before: Option<Channel>,
    pub desired: Channel,
    pub expected_fingerprint: String,
}
#[async_trait]
pub trait StructureBackend: Send + Sync {
    /// Refresh member, bot, role, overwrite and visibility facts; no cached authority.
    async fn inspect(&self, context: &PolicyContext, guild: &GuildId) -> Result<Snapshot>;
    /// Create/update a channel with explicit overwrites. No raw URL or SQL surface.
    /// Implementations must refresh authority after rate waits and call guard.dispatch
    /// at actual submission, including each definitely rejected 429 retry.
    async fn mutate(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        mutation: &ChannelMutation,
        guard: &SendGuard,
    ) -> Result<Channel>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Planned,
    Applying,
    Complete,
    Partial,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepReceipt {
    pub key: String,
    pub channel: Channel,
    pub change: Change,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructurePlan {
    pub id: String,
    pub guild: GuildId,
    pub principal: String,
    pub deployment: DeploymentId,
    pub expires_at: u64,
    pub policy_version: u32,
    pub hash: String,
    pub fingerprint: String,
    pub steps: Vec<Step>,
    pub approved: bool,
    pub receipts: Vec<StepReceipt>,
    pub state: PlanState,
    pub last_error: Option<ErrorCode>,
}
impl StructurePlan {
    fn hash(&self) -> Result<String> {
        Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&json!({
            "id":self.id,"guild":self.guild,"principal":self.principal,"deployment":self.deployment,
            "expires_at":self.expires_at,"policy_version":self.policy_version,"fingerprint":self.fingerprint,"steps":self.steps
        })).map_err(|_|error(ErrorCode::Integrity))?)))
    }
}
pub struct StructureExecutor {
    core: Arc<CoreService>,
    repository: Arc<dyn WorkflowRepository>,
    backend: Arc<dyn StructureBackend>,
    locks: Mutex<BTreeMap<GuildId, Arc<tokio::sync::Mutex<()>>>>,
}
impl StructureExecutor {
    pub fn new(
        core: Arc<CoreService>,
        repository: Arc<dyn WorkflowRepository>,
        backend: Arc<dyn StructureBackend>,
    ) -> Self {
        Self {
            core,
            repository,
            backend,
            locks: Mutex::new(BTreeMap::new()),
        }
    }
    fn guild_lock(&self, guild: &GuildId) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(guild.clone())
            .or_default()
            .clone()
    }
    pub async fn inspect(&self, context: &PolicyContext, guild: &GuildId) -> Result<Snapshot> {
        self.core.status(context, Some(guild)).await?;
        let snapshot = self.backend.inspect(context, guild).await?;
        if snapshot.guild != *guild {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        if let PolicyContext::Discord { user, .. } = context
            && snapshot.actor.id != user.as_str()
        {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        if snapshot.observed_at > now() || now().saturating_sub(snapshot.observed_at) > 30 {
            return Err(error(ErrorCode::Conflict));
        }
        snapshot.visible()
    }
    pub async fn plan(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: &StructureRequest,
    ) -> Result<StructurePlan> {
        self.core.authorize_module(context, guild).await?;
        let lock = self.guild_lock(guild);
        let _lock = lock.lock().await;
        let snapshot = self.inspect(context, guild).await?;
        let mut bindings = BTreeMap::new();
        for desired in &request.channels {
            if let Some(record) = self
                .repository
                .workflow_get(guild, WorkflowKind::ResourceBinding, &desired.key)
                .await?
            {
                let id = record
                    .value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
                bindings.insert(desired.key.clone(), id.to_owned());
            }
        }
        let steps = build_steps(&snapshot, request, &bindings)?;
        let deployment = self.core.status(context, Some(guild)).await?.deployment;
        let mut plan = StructurePlan {
            id: uuid::Uuid::new_v4().to_string(),
            guild: guild.clone(),
            principal: actor(context),
            deployment,
            expires_at: now() + 900,
            policy_version: 1,
            hash: String::new(),
            fingerprint: snapshot.fingerprint()?,
            steps,
            approved: false,
            receipts: vec![],
            state: PlanState::Planned,
            last_error: None,
        };
        plan.hash = plan.hash()?;
        self.repository
            .workflow_put(
                guild,
                WorkflowKind::StructurePlan,
                &plan.id,
                None,
                &encode(&plan)?,
            )
            .await?;
        Ok(plan)
    }
    async fn owned(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &str,
    ) -> Result<(WorkflowRecord, StructurePlan)> {
        self.core.authorize_module(context, guild).await?;
        let record = self
            .repository
            .workflow_get(guild, WorkflowKind::StructurePlan, id)
            .await?
            .ok_or_else(|| error(ErrorCode::NotFound))?;
        let plan: StructurePlan = decode(record.value.clone())?;
        if plan.guild != *guild || plan.principal != actor(context) {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        if plan.deployment != self.core.status(context, Some(guild)).await?.deployment
            || plan.policy_version != 1
            || plan.expires_at <= now()
        {
            return Err(error(ErrorCode::Conflict));
        }
        Ok((record, plan))
    }
    pub async fn read_plan(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &str,
    ) -> Result<StructurePlan> {
        Ok(self.owned(context, guild, id).await?.1)
    }
    pub async fn approve(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &str,
        hash: &str,
    ) -> Result<StructurePlan> {
        let lock = self.guild_lock(guild);
        let _lock = lock.lock().await;
        let (record, mut plan) = self.owned(context, guild, id).await?;
        if !matches!(plan.state, PlanState::Planned)
            || hash != plan.hash
            || plan.hash()? != plan.hash
        {
            return Err(error(ErrorCode::Conflict));
        }
        if self.inspect(context, guild).await?.fingerprint()? != plan.fingerprint {
            return Err(error(ErrorCode::Conflict));
        }
        plan.approved = true;
        self.save(&plan, record.revision).await?;
        Ok(plan)
    }
    async fn save(&self, plan: &StructurePlan, revision: u64) -> Result<u64> {
        Ok(self
            .repository
            .workflow_put(
                &plan.guild,
                WorkflowKind::StructurePlan,
                &plan.id,
                Some(revision),
                &encode(plan)?,
            )
            .await?
            .revision)
    }
    pub async fn apply(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<StructurePlan> {
        let lock = self.guild_lock(guild);
        let _lock = tokio::select! {biased;_=cancel.cancelled()=>return Err(error(ErrorCode::Cancelled)),v=lock.lock()=>v};
        let (record, mut plan) = self.owned(context, guild, id).await?;
        if plan.steps.iter().any(|s| s.approval_required) && !plan.approved {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        if matches!(plan.state, PlanState::Applying)
            || plan.last_error == Some(ErrorCode::UnknownOutcome)
        {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        let mut snapshot = self.inspect(context, guild).await?;
        if snapshot.fingerprint()? != plan.fingerprint {
            return Err(error(ErrorCode::Conflict));
        }
        if matches!(plan.state, PlanState::Complete) {
            return Ok(plan);
        }
        let mut revision = record.revision;
        for step in plan.steps.clone().into_iter().skip(plan.receipts.len()) {
            self.core.authorize_module(context, guild).await?;
            let guard = SendGuard::new(cancel.clone(), plan.expires_at);
            guard.dispatch(|| Ok(()))?;
            let parent = step
                .parent_key
                .as_ref()
                .map(|key| {
                    plan.receipts
                        .iter()
                        .find(|r| &r.key == key)
                        .map(|r| r.channel.id.clone())
                        .ok_or_else(|| error(ErrorCode::Integrity))
                })
                .transpose()?;
            let desired = Channel {
                id: step
                    .before
                    .as_ref()
                    .map(|c| c.id.clone())
                    .unwrap_or_default(),
                guild: guild.clone(),
                parent,
                kind: step.kind,
                name: step.name.clone(),
                overwrites: step.overwrites.clone(),
            };
            let actual = step
                .before
                .as_ref()
                .and_then(|c| snapshot.channels.iter().find(|v| v.id == c.id))
                .cloned();
            let reuse = actual.as_ref() == Some(&desired);
            // Reserve every logical identity before sending. A pending identity is never erased by timeout.
            let binding = self
                .repository
                .workflow_get(guild, WorkflowKind::ResourceBinding, &step.key)
                .await?;
            let binding = match binding {
                Some(existing) => {
                    if existing.value.get("id").and_then(Value::as_str)
                        != actual.as_ref().map(|c| c.id.as_str())
                    {
                        return Err(error(ErrorCode::RecoveryRequired));
                    }
                    existing
                }
                None => {
                    self.repository
                        .workflow_put(
                            guild,
                            WorkflowKind::ResourceBinding,
                            &step.key,
                            None,
                            &json!({"plan":plan.id,"id":actual.as_ref().map(|c|&c.id)}),
                        )
                        .await?
                }
            };
            plan.state = PlanState::Applying;
            revision = self.save(&plan, revision).await?;
            let mutation = ChannelMutation {
                before: actual.clone(),
                desired,
                expected_fingerprint: snapshot.fingerprint()?,
            };
            let outcome = if reuse {
                Ok(actual.unwrap())
            } else {
                let adapter = MutationAdapter {
                    backend: self.backend.clone(),
                    context: context.clone(),
                    guild: guild.clone(),
                    mutation: mutation.clone(),
                    guard: guard.clone(),
                };
                let purpose = if step.change == Change::Create {
                    format!("structure:create:{}", step.key)
                } else {
                    format!("structure:{}:{}", plan.id, step.key)
                };
                self.core
                    .execute(context, guild, &purpose, &adapter, cancel)
                    .await
                    .and_then(|effect| {
                        decode(effect.receipt.ok_or_else(|| error(ErrorCode::Integrity))?)
                    })
            };
            guard.revoke();
            let outcome = match outcome {
                Ok(channel) => self
                    .verify_transition(context, guild, &snapshot, &mutation, &channel)
                    .await
                    .map(|next| (channel, next)),
                Err(e) => Err(e),
            };
            match outcome {
                Ok((channel, next)) => {
                    self.repository
                        .workflow_put(
                            guild,
                            WorkflowKind::ResourceBinding,
                            &step.key,
                            Some(binding.revision),
                            &json!({"plan":plan.id,"id":channel.id}),
                        )
                        .await?;
                    plan.receipts.push(StepReceipt {
                        key: step.key,
                        channel,
                        change: if reuse { Change::Reuse } else { step.change },
                    });
                    snapshot = next;
                    plan.fingerprint = snapshot.fingerprint()?;
                    plan.state = PlanState::Partial;
                    plan.last_error = None;
                    revision = self.save(&plan, revision).await?;
                }
                Err(e) => {
                    plan.last_error = Some(e.code);
                    plan.state = PlanState::Partial;
                    self.save(&plan, revision).await?;
                    return Ok(plan);
                }
            }
        }
        plan.state = PlanState::Complete;
        self.save(&plan, revision).await?;
        Ok(plan)
    }
    async fn verify_transition(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        before: &Snapshot,
        mutation: &ChannelMutation,
        returned: &Channel,
    ) -> Result<Snapshot> {
        let mut desired = mutation.desired.clone();
        desired.id = returned.id.clone();
        if *returned != desired || UserId::new(&returned.id).is_err() {
            return Err(error(ErrorCode::Integrity));
        }
        let mut expected = before.clone();
        if let Some(old) = &mutation.before {
            expected.channels.retain(|c| c.id != old.id);
            if old.kind == ChannelKind::Category && old.overwrites != returned.overwrites {
                for child in &mut expected.channels {
                    if child.parent.as_deref() == Some(old.id.as_str())
                        && child.overwrites == old.overwrites
                    {
                        child.overwrites = returned.overwrites.clone();
                    }
                }
            }
        } else if expected.channels.iter().any(|c| c.id == returned.id) {
            return Err(error(ErrorCode::Conflict));
        }
        expected.channels.push(returned.clone());
        let observed = self.inspect(context, guild).await?;
        if observed.fingerprint()? != expected.fingerprint()? {
            return Err(error(ErrorCode::Conflict));
        }
        Ok(observed)
    }
}
struct MutationAdapter {
    backend: Arc<dyn StructureBackend>,
    context: PolicyContext,
    guild: GuildId,
    mutation: ChannelMutation,
    guard: SendGuard,
}
#[async_trait]
impl EffectAdapter for MutationAdapter {
    async fn apply(&self, _effect: &Effect) -> Result<Value> {
        encode(
            &self
                .backend
                .mutate(&self.context, &self.guild, &self.mutation, &self.guard)
                .await?,
        )
    }
}
