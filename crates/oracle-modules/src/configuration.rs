//! Host-owned configuration plans and durable desired/effective state.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

#[async_trait]
pub trait ConfigurationPolicy: Send + Sync {
    /// Recheck real host prerequisites and any destination. Returning Ok only
    /// authorizes these exact values; it does not assert successful delivery.
    async fn validate(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        values: &Value,
    ) -> Result<()>;
}
#[derive(Clone)]
pub(super) struct ConfigurationServices {
    repository: Arc<dyn WorkflowRepository>,
    policy: Arc<dyn ConfigurationPolicy>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigurationPlan {
    pub id: String,
    pub expected_revision: u64,
    pub schema_version: u32,
    pub values: Value,
    pub expires_at: u64,
}
#[derive(Clone)]
pub(super) struct BoundPlan {
    plan: ConfigurationPlan,
    guild: GuildId,
    module: ModuleId,
    actor: String,
    deployment: DeploymentId,
    generation: u64,
    session: String,
    epoch: u64,
    record_revision: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigurationReceipt {
    pub plan: String,
    pub stored_revision: u64,
    pub effective_revision: Option<u64>,
    /// pending, rejected, committed, unknown, or effective. Effective describes
    /// configuration only; event subscriptions and delivery need separate proof.
    pub state: String,
    pub problem: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigurationStatus {
    pub schema_version: Option<u32>,
    pub stored_revision: u64,
    pub values: Option<Value>,
    pub effective: Option<EffectiveConfiguration>,
    pub receipt: Option<ConfigurationReceipt>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Candidate {
    expires_at: u64,
    generation: u64,
    session: String,
    epoch: u64,
    plan: String,
    actor: String,
    deployment: DeploymentId,
    schema_version: u32,
    revision: u64,
    values: Value,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Saved {
    effective_session: Option<String>,
    schema_version: Option<u32>,
    revision: u64,
    values: Option<Value>,
    candidate: Option<Candidate>,
    receipt: Option<ConfigurationReceipt>,
}
fn err(code: ErrorCode) -> Error {
    Error::new(code)
}
fn principal(actor: &PolicyContext) -> String {
    match actor {
        PolicyContext::LocalOperator => "local_operator".into(),
        PolicyContext::Discord { user, .. } => format!("discord:{user}"),
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn encode(value: &impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| err(ErrorCode::InvalidInput))
}
fn decode(record: Option<&WorkflowRecord>) -> Result<Saved> {
    record
        .map(|r| {
            serde_json::from_value(r.value.clone()).map_err(|_| err(ErrorCode::StorageUnavailable))
        })
        .transpose()
        .map(|r| r.unwrap_or_default())
}
fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if value.is_object() {
                let entry = target.entry(key.clone()).or_insert_with(|| json!({}));
                if !entry.is_object() {
                    *entry = json!({});
                }
                merge(entry, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}
impl ModuleManager {
    /// Install once from the trusted composition root. Existing constructors keep
    /// configuration unavailable until both durable storage and policy exist.
    pub fn set_configuration_services(
        &self,
        repository: Arc<dyn WorkflowRepository>,
        policy: Arc<dyn ConfigurationPolicy>,
    ) -> Result<()> {
        let mut services = self.configuration_services.write().unwrap();
        if services.is_some() {
            return Err(err(ErrorCode::Conflict));
        }
        *services = Some(ConfigurationServices { repository, policy });
        Ok(())
    }
    fn config_services(&self) -> Result<ConfigurationServices> {
        self.configuration_services
            .read()
            .unwrap()
            .clone()
            .ok_or_else(unavailable)
    }
    fn config_generation(
        &self,
        module: &ModuleId,
        guild: &GuildId,
    ) -> Result<(Arc<Generation>, u64)> {
        let generation = self.get(module)?;
        self.validate_dependencies(&generation, guild)?;
        if !generation
            .installed
            .package
            .manifest
            .subscriptions
            .is_empty()
        {
            self.require_event_intents(&generation)?;
        }
        let epoch = generation
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .filter(|a| {
                a.grants.contains("config.own") && generation.gate.is_active(guild, a.epoch)
            })
            .ok_or_else(unavailable)?
            .epoch;
        if generation
            .installed
            .package
            .manifest
            .configuration
            .is_none()
        {
            return Err(unavailable());
        }
        Ok((generation, epoch))
    }
    /// Presets and explicit overrides merge over saved values, preserving fields
    /// outside the selected patch. The full result must satisfy the schema.
    #[allow(clippy::too_many_arguments)]
    pub async fn configuration_plan(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        preset: Option<&str>,
        overrides: Value,
        ttl: Duration,
    ) -> Result<ConfigurationPlan> {
        let _lock = self.lifecycle.lock().await;
        self.core.authorize_module(actor, guild).await?;
        if !overrides.is_object() || ttl.as_secs() == 0 || ttl > Duration::from_secs(900) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let services = self.config_services()?;
        let (generation, epoch) = self.config_generation(module, guild)?;
        let descriptor = generation
            .installed
            .package
            .manifest
            .configuration
            .as_ref()
            .unwrap();
        let record = services
            .repository
            .workflow_get(guild, WorkflowKind::Configuration, module.as_str())
            .await?;
        let saved = decode(record.as_ref())?;
        let mut values = saved.values.unwrap_or_else(|| json!({}));
        if let Some(preset) = preset {
            merge(
                &mut values,
                descriptor
                    .presets
                    .get(preset)
                    .ok_or_else(|| err(ErrorCode::InvalidInput))?,
            );
        }
        merge(&mut values, &overrides);
        crate::package::schema_validator(&descriptor.schema)?
            .validate(&values)
            .map_err(|_| err(ErrorCode::SchemaInvalid))?;
        // The 64 KiB workflow row holds both prior desired values and a new
        // candidate plus receipt metadata. Bound each object before planning.
        if serde_json::to_vec(&values)
            .map_err(|_| err(ErrorCode::InvalidInput))?
            .len()
            > 24 * 1024
        {
            return Err(err(ErrorCode::QuotaExceeded));
        }
        services
            .policy
            .validate(actor, guild, module, &values)
            .await?;
        let deployment = self.core.status(actor, Some(guild)).await?.deployment;
        let plan = ConfigurationPlan {
            id: uuid::Uuid::new_v4().to_string(),
            expected_revision: saved.revision,
            schema_version: descriptor.schema_version,
            values,
            expires_at: now().saturating_add(ttl.as_secs()),
        };
        let mut plans = self.configuration_plans.lock().unwrap();
        plans.retain(|_, p| p.plan.expires_at > now());
        if plans.len() >= 128 {
            return Err(err(ErrorCode::QuotaExceeded));
        }
        plans.insert(
            plan.id.clone(),
            BoundPlan {
                plan: plan.clone(),
                guild: guild.clone(),
                module: module.clone(),
                actor: principal(actor),
                deployment,
                generation: generation.number,
                session: generation.session.clone(),
                epoch,
                record_revision: record.map(|r| r.revision),
            },
        );
        Ok(plan)
    }
    pub async fn configuration_apply(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        plan_id: &str,
    ) -> Result<ConfigurationReceipt> {
        let _lock = self.lifecycle.lock().await;
        self.core.authorize_module(actor, guild).await?;
        let services = self.config_services()?;
        let (generation, epoch) = self.config_generation(module, guild)?;
        let bound = self
            .configuration_plans
            .lock()
            .unwrap()
            .get(plan_id)
            .cloned()
            .ok_or_else(|| err(ErrorCode::InvalidInput))?;
        let deployment = self.core.status(actor, Some(guild)).await?.deployment;
        if bound.guild != *guild
            || bound.module != *module
            || bound.actor != principal(actor)
            || bound.deployment != deployment
            || bound.generation != generation.number
            || bound.session != generation.session
            || bound.epoch != epoch
            || now() >= bound.plan.expires_at
        {
            return Err(err(ErrorCode::ForbiddenScope));
        }
        services
            .policy
            .validate(actor, guild, module, &bound.plan.values)
            .await?;
        let record = services
            .repository
            .workflow_get(guild, WorkflowKind::Configuration, module.as_str())
            .await?;
        let mut saved = decode(record.as_ref())?;
        if saved.receipt.as_ref().is_some_and(|r| r.plan == plan_id) {
            return self
                .config_resume(actor, guild, module, &services, &generation, record, saved)
                .await;
        }
        if record.as_ref().map(|r| r.revision) != bound.record_revision
            || saved.revision != bound.plan.expected_revision
        {
            return Err(err(ErrorCode::Conflict));
        }
        saved.candidate = Some(Candidate {
            expires_at: bound.plan.expires_at,
            generation: bound.generation,
            session: bound.session,
            epoch: bound.epoch,
            plan: plan_id.into(),
            actor: principal(actor),
            deployment,
            schema_version: bound.plan.schema_version,
            revision: saved
                .revision
                .checked_add(1)
                .ok_or_else(|| err(ErrorCode::QuotaExceeded))?,
            values: bound.plan.values,
        });
        saved.receipt = Some(ConfigurationReceipt {
            plan: plan_id.into(),
            stored_revision: saved.revision,
            effective_revision: None,
            state: "pending".into(),
            problem: None,
        });
        let record = self
            .config_save(&services, guild, module, record.as_ref(), &saved)
            .await?;
        self.config_resume(
            actor,
            guild,
            module,
            &services,
            &generation,
            Some(record),
            saved,
        )
        .await
    }
    async fn config_save(
        &self,
        services: &ConfigurationServices,
        guild: &GuildId,
        module: &ModuleId,
        previous: Option<&WorkflowRecord>,
        saved: &Saved,
    ) -> Result<WorkflowRecord> {
        services
            .repository
            .workflow_put(
                guild,
                WorkflowKind::Configuration,
                module.as_str(),
                previous.map(|r| r.revision),
                &encode(saved)?,
            )
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn config_resume(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        services: &ConfigurationServices,
        generation: &Generation,
        mut record: Option<WorkflowRecord>,
        mut saved: Saved,
    ) -> Result<ConfigurationReceipt> {
        if !generation
            .installed
            .package
            .manifest
            .subscriptions
            .is_empty()
        {
            self.require_event_intents(generation)?;
        }
        let candidate = saved
            .candidate
            .clone()
            .ok_or_else(|| err(ErrorCode::InvalidInput))?;
        if candidate.actor != principal(actor)
            || candidate.deployment != self.core.status(actor, Some(guild)).await?.deployment
        {
            return Err(err(ErrorCode::ForbiddenScope));
        }
        let descriptor = generation
            .installed
            .package
            .manifest
            .configuration
            .as_ref()
            .ok_or_else(unavailable)?;
        if descriptor.schema_version != candidate.schema_version {
            return Err(err(ErrorCode::Compatibility));
        }
        crate::package::schema_validator(&descriptor.schema)?
            .validate(&candidate.values)
            .map_err(|_| err(ErrorCode::SchemaInvalid))?;
        services
            .policy
            .validate(actor, guild, module, &candidate.values)
            .await?;
        let mut receipt = saved
            .receipt
            .clone()
            .ok_or_else(|| err(ErrorCode::InvalidInput))?;
        if receipt.state == "rejected" {
            return Ok(receipt);
        }
        if generation
            .configuration(
                guild,
                "configuration.prepare",
                candidate.revision,
                candidate.values.clone(),
            )
            .await
            .is_err()
        {
            receipt.state = if saved.revision < candidate.revision {
                "rejected"
            } else {
                "unknown"
            }
            .into();
            receipt.problem = Some("prepare_unavailable_or_rejected".into());
            receipt.effective_revision = None;
            saved.receipt = Some(receipt.clone());
            self.config_save(services, guild, module, record.as_ref(), &saved)
                .await?;
            return Ok(receipt);
        }
        if saved.revision < candidate.revision {
            if now() >= candidate.expires_at
                || generation.number != candidate.generation
                || generation.session != candidate.session
                || !generation.gate.is_active(guild, candidate.epoch)
            {
                return Err(err(ErrorCode::ForbiddenScope));
            }
            self.core.authorize_module(actor, guild).await?;
            services
                .policy
                .validate(actor, guild, module, &candidate.values)
                .await?;
            if now() >= candidate.expires_at {
                return Err(err(ErrorCode::ForbiddenScope));
            }
            if !generation
                .installed
                .package
                .manifest
                .subscriptions
                .is_empty()
            {
                self.require_event_intents(generation)?;
            }
            // Desired values and receipt share a single durable CAS record.
            saved.effective_session = None;
            saved.schema_version = Some(candidate.schema_version);
            saved.revision = candidate.revision;
            saved.values = Some(candidate.values.clone());
            receipt.stored_revision = candidate.revision;
            receipt.state = "committed".into();
            saved.receipt = Some(receipt.clone());
            record = Some(
                self.config_save(services, guild, module, record.as_ref(), &saved)
                    .await?,
            );
        }
        self.core.authorize_module(actor, guild).await?;
        services
            .policy
            .validate(actor, guild, module, &candidate.values)
            .await?;
        if !generation
            .installed
            .package
            .manifest
            .subscriptions
            .is_empty()
        {
            self.require_event_intents(generation)?;
        }
        let applied = generation
            .configuration(
                guild,
                "configuration.apply",
                candidate.revision,
                candidate.values.clone(),
            )
            .await;
        receipt.state = "unknown".into();
        receipt.effective_revision = None;
        receipt.problem = Some("apply_or_readback_unavailable".into());
        if applied.is_ok() {
            let observed = generation
                .configuration(guild, "configuration.effective", 0, Value::Null)
                .await;
            if let Ok(value) = observed
                && let Ok(Some(effective)) =
                    serde_json::from_value::<Option<EffectiveConfiguration>>(value)
            {
                if effective.revision == candidate.revision && effective.values == candidate.values
                {
                    saved.effective_session = Some(generation.session.clone());
                    receipt.state = "effective".into();
                    receipt.effective_revision = Some(effective.revision);
                    receipt.problem = None;
                } else {
                    receipt.problem = Some("effective_config_mismatch".into());
                }
            }
        }
        saved.receipt = Some(receipt.clone());
        self.config_save(services, guild, module, record.as_ref(), &saved)
            .await?;
        Ok(receipt)
    }
    /// Explicit recovery reauthorizes the saved desired revision against the
    /// current generation. A restored deployment cannot reuse old authority.
    pub async fn configuration_recover(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
    ) -> Result<ConfigurationReceipt> {
        let _lock = self.lifecycle.lock().await;
        self.core.authorize_module(actor, guild).await?;
        let services = self.config_services()?;
        let (generation, _) = self.config_generation(module, guild)?;
        let record = services
            .repository
            .workflow_get(guild, WorkflowKind::Configuration, module.as_str())
            .await?;
        let saved = decode(record.as_ref())?;
        // Pending approval was never committed. It needs a fresh plan after a
        // restart; only durable desired configuration is recoverable here.
        if saved.receipt.as_ref().is_none_or(|r| r.state == "pending") {
            return Err(err(ErrorCode::InvalidInput));
        }
        self.config_resume(actor, guild, module, &services, &generation, record, saved)
            .await
    }
    /// Last independently verified config bound to this executable session. This
    /// path intentionally does not call back into a module waiting on host.notify.
    pub(super) async fn configuration_verified(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        generation: &Generation,
    ) -> Result<EffectiveConfiguration> {
        self.core.authorize_module(actor, guild).await?;
        let services = self.config_services()?;
        let module = &generation.installed.package.manifest.id;
        let record = services
            .repository
            .workflow_get(guild, WorkflowKind::Configuration, module.as_str())
            .await?;
        let saved = decode(record.as_ref())?;
        if saved.effective_session.as_deref() != Some(generation.session.as_str()) {
            return Err(unavailable());
        }
        let values = saved.values.ok_or_else(unavailable)?;
        services
            .policy
            .validate(actor, guild, module, &values)
            .await?;
        Ok(EffectiveConfiguration {
            revision: saved.revision,
            values,
        })
    }
    pub(super) async fn configuration_ready(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        generation: &Generation,
    ) -> Result<EffectiveConfiguration> {
        let expected = self
            .configuration_verified(actor, guild, generation)
            .await?;
        let value = generation
            .configuration(guild, "configuration.effective", 0, Value::Null)
            .await?;
        let actual = serde_json::from_value::<Option<EffectiveConfiguration>>(value)
            .map_err(|_| unavailable())?;
        if actual.as_ref() != Some(&expected) {
            return Err(unavailable());
        }
        Ok(expected)
    }
    pub async fn configuration_inspect(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
    ) -> Result<ConfigurationStatus> {
        self.core.authorize_module(actor, guild).await?;
        let services = self.config_services()?;
        let record = services
            .repository
            .workflow_get(guild, WorkflowKind::Configuration, module.as_str())
            .await?;
        let saved = decode(record.as_ref())?;
        let effective = if let Ok((generation, _)) = self.config_generation(module, guild) {
            generation
                .configuration(guild, "configuration.effective", 0, Value::Null)
                .await
                .ok()
                .and_then(|value| {
                    serde_json::from_value::<Option<EffectiveConfiguration>>(value)
                        .ok()
                        .flatten()
                })
        } else {
            None
        };
        Ok(ConfigurationStatus {
            schema_version: saved.schema_version,
            stored_revision: saved.revision,
            values: saved.values,
            effective,
            receipt: saved.receipt,
        })
    }
}
