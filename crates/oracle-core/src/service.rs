use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildPolicy {
    pub guild: GuildId,
    pub operators: Vec<UserId>,
}

/// Created only by a trusted host ingress after authenticating its transport.
#[derive(Clone, Debug)]
pub enum PolicyContext {
    LocalOperator,
    Discord {
        guild: GuildId,
        user: UserId,
        manage_guild: bool,
    },
}
impl PolicyContext {
    fn actor(&self) -> String {
        match self {
            Self::LocalOperator => "local_operator".into(),
            Self::Discord { user, .. } => format!("discord:{user}"),
        }
    }
}

pub struct CoreService {
    repository: Arc<dyn Repository>,
    policies: BTreeMap<GuildId, GuildPolicy>,
    modules: RwLock<BTreeMap<ModuleId, BTreeSet<GuildId>>>,
    // Serializes pause with external send admission. A pause cannot overtake an
    // admitted adapter call; shutdown cancellation bounds the in-flight call.
    mutation: tokio::sync::Mutex<()>,
}

/// Stage 1's effect boundary. Only the host supplies adapters; no raw endpoint is
/// exposed to Discord or a provider. Stage 3 will add typed domain operations.
#[async_trait]
pub trait EffectAdapter: Send + Sync {
    async fn apply(&self, effect: &Effect) -> Result<serde_json::Value>;
}

impl CoreService {
    pub fn new(repository: Arc<dyn Repository>, policies: Vec<GuildPolicy>) -> Self {
        Self {
            repository,
            modules: RwLock::new(BTreeMap::new()),
            policies: policies.into_iter().map(|p| (p.guild.clone(), p)).collect(),
            mutation: tokio::sync::Mutex::new(()),
        }
    }
    fn authorize(
        &self,
        context: &PolicyContext,
        guild: Option<&GuildId>,
        write: bool,
    ) -> Result<()> {
        if let Some(guild) = guild
            && !self.policies.contains_key(guild)
        {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        match context {
            PolicyContext::LocalOperator => Ok(()),
            PolicyContext::Discord {
                guild: authenticated,
                user,
                manage_guild,
            } => {
                if guild != Some(authenticated) {
                    return Err(Error::new(ErrorCode::ForbiddenScope));
                }
                let policy = self
                    .policies
                    .get(authenticated)
                    .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
                if write && (!manage_guild || !policy.operators.contains(user)) {
                    return Err(Error::new(ErrorCode::ForbiddenPermission));
                }
                Ok(())
            }
        }
    }
    /// Module ingress shares the same authenticated operator and guild policy.
    pub async fn authorize_module(&self, context: &PolicyContext, guild: &GuildId) -> Result<()> {
        self.authorize(context, Some(guild), true)?;
        let status = self.repository.status(Some(guild)).await?;
        if status
            .guilds
            .iter()
            .any(|state| &state.guild == guild && !state.paused)
        {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
    /// Updated by the runtime registry after publication and fencing.
    pub fn set_module_inventory(&self, modules: BTreeMap<ModuleId, BTreeSet<GuildId>>) {
        *self.modules.write().unwrap() = modules;
    }
    pub async fn status(&self, context: &PolicyContext, guild: Option<&GuildId>) -> Result<Status> {
        self.authorize(context, guild, false)?;
        let mut status = self.repository.status(guild).await?;
        status.modules_loaded = self
            .modules
            .read()
            .unwrap()
            .values()
            .filter(|scopes| guild.is_none_or(|guild| scopes.contains(guild)))
            .count();
        Ok(status)
    }
    pub async fn control(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        paused: bool,
        expected_revision: u64,
    ) -> Result<ControlReceipt> {
        self.authorize(context, Some(guild), true)?;
        let _admission = self.mutation.lock().await;
        self.repository
            .set_paused(
                guild,
                paused,
                expected_revision,
                &context.actor(),
                &OperationId::generate(),
            )
            .await
    }
    pub async fn recovery(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        limit: u32,
    ) -> Result<Vec<Effect>> {
        self.authorize(context, Some(guild), true)?;
        if limit == 0 || limit > 100 {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        self.repository.recovery(guild, limit).await
    }
    /// Journal the send before calling an adapter. Unknown outcomes are never
    /// retried here: they require a later domain-specific readback/reconciliation.
    pub async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        purpose: &str,
        adapter: &dyn EffectAdapter,
        cancel: &CancellationToken,
    ) -> Result<Effect> {
        self.authorize(context, Some(guild), true)?;
        if purpose.is_empty() || purpose.len() > 160 || purpose.chars().any(char::is_control) {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let _admission = tokio::select! { biased; _ = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)), guard = self.mutation.lock() => guard };
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let state = self.repository.status(Some(guild)).await?;
        if state
            .guilds
            .iter()
            .find(|s| &s.guild == guild)
            .is_none_or(|s| s.paused)
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let operation = Operation {
            id: OperationId::generate(),
            guild: guild.clone(),
            actor: context.actor(),
            state: OperationState::Running,
            revision: 0,
        };
        self.repository.begin_operation(&operation).await?;
        let prepared = Effect {
            id: EffectId::generate(),
            operation: operation.id.clone(),
            guild: guild.clone(),
            purpose: purpose.into(),
            state: EffectState::Prepared,
            revision: 0,
            receipt: None,
        };
        let effect = self.repository.reserve_effect(&prepared).await?;
        if effect.id != prepared.id {
            self.repository
                .finish_operation(
                    guild,
                    &operation.id,
                    if effect.state == EffectState::Verified {
                        OperationState::Succeeded
                    } else {
                        OperationState::Failed
                    },
                )
                .await?;
            return if effect.state == EffectState::Verified {
                Ok(effect)
            } else {
                Err(Error::new(ErrorCode::RecoveryRequired))
            };
        }
        let sent = self
            .repository
            .transition_effect(guild, &effect.id, effect.revision, EffectState::Sent, None)
            .await?;
        let outcome = tokio::select! { biased; _ = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)), result = adapter.apply(&sent) => result };
        match outcome {
            Ok(receipt) => {
                let verified = self
                    .repository
                    .transition_effect(
                        guild,
                        &sent.id,
                        sent.revision,
                        EffectState::Verified,
                        Some(receipt),
                    )
                    .await?;
                self.repository
                    .finish_operation(guild, &operation.id, OperationState::Succeeded)
                    .await?;
                Ok(verified)
            }
            Err(_) => {
                self.repository
                    .transition_effect(guild, &sent.id, sent.revision, EffectState::Unknown, None)
                    .await?;
                self.repository
                    .finish_operation(guild, &operation.id, OperationState::RecoveryRequired)
                    .await?;
                Err(Error::new(ErrorCode::UnknownOutcome))
            }
        }
    }
}
