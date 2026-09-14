//! Trusted shared-card journal service and generation-bound worker leases.
use super::*;
use crate::admission::{Authority, Lease};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// Shared-card enqueue/status only touch host-owned records. Reminder delivery
/// additionally requires a separately authorized maintenance worker lease.
#[async_trait]
pub trait SharedCardService: Send + Sync {
    async fn enqueue(&self, module: &ModuleId, guild: &GuildId, intent: Value) -> Result<Value>;
    async fn status(&self, module: &ModuleId, guild: &GuildId, intent_key: &str) -> Result<Value>;
    /// Process a persisted run reminder under authenticated maintenance authority.
    async fn run_reminder(
        &self,
        _module: &ModuleId,
        _guild: &GuildId,
        _key: &str,
        _document: Value,
    ) -> Result<Value> {
        Err(Error::new(ErrorCode::ModuleUnavailable))
    }
}
#[derive(Clone)]
pub struct SharedCardDispatch {
    lease: Arc<Lease>,
    permit: crate::DispatchPermit,
}
impl SharedCardDispatch {
    pub fn dispatch<T>(&self, send: impl FnOnce() -> T) -> Result<T> {
        self.permit.dispatch(send)
    }
    pub fn permit(&self) -> crate::DispatchPermit {
        self.permit.clone()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.lease.authority.cancel.child_token()
    }
}
impl ModuleManager {
    pub fn set_shared_card_service(&self, service: Arc<dyn SharedCardService>) -> Result<()> {
        let mut current = self.shared_card_service.write().unwrap();
        if current.is_some() {
            return Err(Error::new(ErrorCode::Conflict));
        }
        *current = Some(service);
        Ok(())
    }
    pub(super) fn shared_card_service(
        &self,
        module: &ModuleId,
        session: &str,
        number: u64,
        authority: &Authority,
    ) -> Result<Arc<dyn SharedCardService>> {
        let generation = self.get(module)?;
        if generation.session != session
            || generation.number != number
            || !generation.gate.is_active(&authority.guild, authority.epoch)
            || !authority.capabilities.contains("shared_cards.publish")
            || authority.cancel.is_cancelled()
            || tokio::time::Instant::now() >= authority.deadline
        {
            return Err(unavailable());
        }
        if let Some(member) = &authority.member {
            member.check()?;
        }
        self.validate_dependencies(&generation, &authority.guild)?;
        self.shared_card_service
            .read()
            .unwrap()
            .clone()
            .ok_or_else(unavailable)
    }
    /// No member identity is fabricated. The returned RAII value retains the
    /// opaque lease and checks current worker policy at actual socket admission.
    pub async fn shared_card_dispatch(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        session: &str,
        number: u64,
        epoch: u64,
    ) -> Result<SharedCardDispatch> {
        self.core
            .authorize_module(&PolicyContext::LocalOperator, guild)
            .await?;
        let generation = self.get(module)?;
        if generation.session != session
            || generation.number != number
            || generation.installed.package.manifest.shared_cards.is_none()
            || !generation.gate.is_active(guild, epoch)
        {
            return Err(unavailable());
        }
        self.validate_dependencies(&generation, guild)?;
        let active = generation
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .cloned()
            .ok_or_else(unavailable)?;
        if !active.grants.contains("shared_cards.publish") || !active.grants.contains("config.own")
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let config = self
            .configuration_ready(&PolicyContext::LocalOperator, guild, &generation)
            .await?;
        let policy = self.core.member_mutation_gate().worker(guild, module)?;
        let deadline = tokio::time::Instant::from_std(policy.expires_at());
        let authority = Authority {
            member: Some(policy.clone()),
            callback_methods: Some(BTreeSet::new()),
            callback_collections: Some(BTreeSet::new()),
            audience: ModuleAudience::Operator,
            guild: guild.clone(),
            epoch,
            actor: PolicyContext::LocalOperator,
            capabilities: BTreeSet::from(["shared_cards.publish".into()]),
            deadline,
            depth: 0,
            configuration_revision: Some(config.revision),
            cancel: policy.cancellation(),
        };
        let lease = Arc::new(generation.gate.admit(authority)?);
        let permit = crate::DispatchPermit::new(generation.gate.clone(), lease.handle.clone());
        Ok(SharedCardDispatch { lease, permit })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    state: String,
    desired_revision: u64,
    confirmed_revision: Option<u64>,
}
pub(super) fn validate_status(value: &Value) -> Result<()> {
    let status: Status =
        serde_json::from_value(value.clone()).map_err(|_| Error::new(ErrorCode::Integrity))?;
    if !matches!(
        status.state.as_str(),
        "pending" | "confirmed" | "recovery_required" | "missing" | "rejected"
    ) || status
        .confirmed_revision
        .is_some_and(|revision| revision > status.desired_revision)
    {
        return Err(Error::new(ErrorCode::Integrity));
    }
    Ok(())
}

pub struct SharedCardSourceSnapshot {
    pub session: String,
    pub generation: u64,
    pub epoch: u64,
    pub intent: Value,
    pub configuration: Value,
}
impl ModuleManager {
    pub async fn shared_card_source(
        &self,
        guild: &GuildId,
        module: &ModuleId,
        intent_key: &str,
    ) -> Result<SharedCardSourceSnapshot> {
        let generation = self.get(module)?;
        let source = generation
            .installed
            .package
            .manifest
            .shared_cards
            .as_ref()
            .ok_or_else(unavailable)?;
        let active = generation
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .cloned()
            .ok_or_else(unavailable)?;
        let lease = self
            .shared_card_dispatch(
                guild,
                module,
                &generation.session,
                generation.number,
                active.epoch,
            )
            .await?;
        let document = self
            .repository
            .document_get(module, guild, &source.collection, intent_key)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let intent = document
            .value
            .pointer(&source.pointer)
            .cloned()
            .filter(|value| !value.is_null())
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let configuration = self
            .configuration_ready(&PolicyContext::LocalOperator, guild, &generation)
            .await?
            .values;
        lease.dispatch(|| ())?;
        Ok(SharedCardSourceSnapshot {
            session: generation.session.clone(),
            generation: generation.number,
            epoch: active.epoch,
            intent,
            configuration,
        })
    }
}
