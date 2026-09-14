//! One process generation and its opaque host-issued invocation authority.
use crate::{
    admission::{Admission, Authority},
    package::{validate_input, validate_output},
};
use async_trait::async_trait;
use oracle_core::{
    Error, ErrorCode, GuildEvent, GuildId, InstalledModule, ModuleHostHealth, ModuleId,
    ModuleManifest, ModuleRepository, PolicyContext, Result,
};
use oracle_process::{ModuleProcess, ProcessRuntime, RuntimeError, StopReport};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

mod callbacks;
use callbacks::Callbacks;

#[derive(Clone)]
pub(crate) struct Activation {
    pub epoch: u64,
    pub grants: BTreeSet<String>,
    pub bindings: BTreeMap<String, ModuleId>,
}
#[async_trait]
pub(crate) trait ContractRouter: Send + Sync {
    async fn shared_card(
        &self,
        module: &ModuleId,
        session: &str,
        generation: u64,
        authority: Authority,
        intent: Option<Value>,
        key: &str,
    ) -> Result<Value>;
    async fn run_reminder(
        &self,
        module: &ModuleId,
        session: &str,
        generation: u64,
        authority: Authority,
        key: &str,
        document: Value,
    ) -> Result<Value>;
    async fn host_health(
        &self,
        module: &ModuleId,
        session: &str,
        generation: u64,
        authority: Authority,
    ) -> Result<ModuleHostHealth>;

    #[allow(clippy::too_many_arguments)] // Explicit scope, lease and notification body cross one trusted boundary.
    async fn notify(
        &self,
        module: &ModuleId,
        purpose: String,
        destination: String,
        text: String,
        authority: Authority,
        permit: crate::DispatchPermit,
        cancel: CancellationToken,
    ) -> Result<Value>;

    async fn echo(
        &self,
        module: &ModuleId,
        purpose: String,
        body: Value,
        authority: Authority,
        permit: crate::DispatchPermit,
        cancel: CancellationToken,
    ) -> Result<Value>;

    async fn invoke(
        &self,
        provider: &ModuleId,
        contract: &str,
        input: Value,
        authority: Authority,
    ) -> Result<Value>;
}
pub(crate) struct Generation {
    pub installed: InstalledModule,
    pub number: u64,
    pub session: String,
    pub gate: Arc<Admission>,
    pub process: OnceLock<ModuleProcess>,
    pub activations: Mutex<BTreeMap<GuildId, Activation>>,
    repository: Arc<dyn ModuleRepository>,
    router: Arc<dyn ContractRouter>,
    normal: bool,
}
fn error(code: ErrorCode) -> Error {
    Error::new(code)
}
fn unavailable() -> Error {
    error(ErrorCode::ModuleUnavailable)
}
fn runtime_error(error: RuntimeError) -> Error {
    match error {
        RuntimeError::DigestMismatch => crate::generation::error(ErrorCode::ArtifactChanged),
        _ => unavailable(),
    }
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| error(ErrorCode::InvalidInput))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    protocol_major: u32,
    protocol_minor: u32,
    manifest: ModuleManifest,
}
struct StopOnDrop {
    generation: Arc<Generation>,
    armed: bool,
}
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.generation.gate.fence(None);
            if let Some(process) = self.generation.process.get() {
                process.request_stop();
            }
        }
    }
}
struct QuiesceGuard<'a> {
    generation: &'a Generation,
    armed: bool,
}
impl Drop for QuiesceGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.generation.gate.fence(None);
            self.generation.process().request_stop();
        }
    }
}
impl Generation {
    #[allow(clippy::too_many_arguments)] // Explicit host-owned generation construction inputs.
    pub async fn spawn(
        runtime: &ProcessRuntime,
        artifact: &Path,
        installed: InstalledModule,
        number: u64,
        repository: Arc<dyn ModuleRepository>,
        router: Arc<dyn ContractRouter>,
        mode: &str,
        registry: Arc<crate::registry::RegistrySignal>,
        data_directory: Option<&Path>,
    ) -> Result<Arc<Self>> {
        if !matches!(mode, "normal" | "migration") || number == 0 {
            return Err(error(ErrorCode::InvalidInput));
        }
        let minor = installed.package.manifest.protocol_minor_min;
        if minor > 2 || (minor == 0 && data_directory.is_some()) {
            return Err(error(ErrorCode::Compatibility));
        }
        if installed
            .package
            .manifest
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.data_directory_required)
            && data_directory.is_none()
        {
            return Err(error(ErrorCode::InvalidInput));
        }
        let generation = Arc::new(Self {
            installed,
            number,
            session: uuid::Uuid::new_v4().to_string(),
            gate: Arc::new(Admission::with_registry(registry)),
            process: OnceLock::new(),
            activations: Mutex::new(BTreeMap::new()),
            repository,
            router,
            normal: mode == "normal",
        });
        let mut cleanup = StopOnDrop {
            generation: generation.clone(),
            armed: true,
        };
        let digest = generation
            .installed
            .package
            .files
            .get(&generation.installed.package.entrypoint)
            .ok_or_else(|| error(ErrorCode::ArtifactChanged))?;
        let process=runtime.spawn(artifact,digest,json!({"protocol_major":1,"protocol_minor":minor,"session":generation.session,"generation":number}),Arc::new(Callbacks(Arc::downgrade(&generation)))).await.map_err(runtime_error)?;
        generation.process.set(process).map_err(|_| unavailable())?;
        let initialized = async {
            let hello: Hello = decode(generation.process().hello().clone())
                .map_err(|_| error(ErrorCode::Compatibility))?;
            if hello.protocol_major != 1
                || hello.protocol_minor != minor
                || hello.manifest != generation.installed.package.manifest
            {
                return Err(error(ErrorCode::Compatibility));
            }
            let mut initialize =
                json!({"session":generation.session,"generation":number,"mode":mode});
            if minor >= 1 {
                initialize["runtime"] = match data_directory {
                    Some(path) => json!({"data_directory":path}),
                    None => json!({}),
                };
            }
            let result = generation
                .process()
                .call("initialize", initialize, Duration::from_secs(5))
                .await
                .map_err(runtime_error)?;
            if result != json!({"initialized":true}) {
                return Err(error(ErrorCode::Compatibility));
            }
            Ok(())
        }
        .await;
        if let Err(error) = initialized {
            let _ = generation.force_stop().await;
            return Err(error);
        }
        cleanup.armed = false;
        Ok(generation)
    }
    pub fn process(&self) -> &ModuleProcess {
        self.process
            .get()
            .expect("process installed before generation publication")
    }
    pub async fn prepare_activation(
        self: &Arc<Self>,
        guild: GuildId,
        epoch: u64,
        grants: BTreeSet<String>,
        bindings: BTreeMap<String, ModuleId>,
    ) -> Result<()> {
        if !self.normal || epoch == 0 || self.activations.lock().unwrap().contains_key(&guild) {
            return Err(unavailable());
        }
        if grants
            .iter()
            .any(|grant| !self.installed.package.manifest.capabilities.contains(grant))
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        let mut cleanup = StopOnDrop {
            generation: self.clone(),
            armed: true,
        };
        let result = self
            .process()
            .call(
                "activate",
                json!({"guild":guild,"epoch":epoch}),
                Duration::from_secs(5),
            )
            .await
            .map_err(runtime_error)?;
        if result != json!({"activated":true}) {
            return Err(unavailable());
        }
        self.activations.lock().unwrap().insert(
            guild.clone(),
            Activation {
                epoch,
                grants,
                bindings,
            },
        );
        cleanup.armed = false;
        Ok(())
    }
    pub fn publish_activation(&self, guild: &GuildId) -> Result<()> {
        if !self.process().is_alive() {
            return Err(unavailable());
        }
        let epoch = self
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .ok_or_else(unavailable)?
            .epoch;
        self.gate.activate(guild.clone(), epoch)
    }
    #[allow(clippy::too_many_arguments)] // Pin both configuration and activation revisions at admission.
    pub async fn invoke(
        &self,
        actor: PolicyContext,
        guild: &GuildId,
        operation: &str,
        input: Value,
        parent: Option<Authority>,
        configuration_revision: Option<u64>,
        expected_epoch: Option<u64>,
    ) -> Result<Value> {
        self.invoke_inner(
            actor,
            guild,
            operation,
            input,
            parent,
            configuration_revision,
            expected_epoch,
            None,
            None,
        )
        .await
    }
    pub async fn invoke_member(
        &self,
        actor: &oracle_core::member_read::MemberContext,
        operation: &str,
        input: Value,
        epoch: u64,
        permit: oracle_core::member_read::MemberReadPermit,
    ) -> Result<Value> {
        let context = PolicyContext::Discord {
            guild: actor.guild.clone(),
            user: actor.user.clone(),
            manage_guild: false,
        };
        self.invoke_inner(
            context,
            &actor.guild,
            operation,
            input,
            None,
            None,
            Some(epoch),
            Some(permit),
            None,
        )
        .await
    }
    pub async fn invoke_member_mutation(
        &self,
        actor: &oracle_core::member_read::MemberContext,
        operation: &str,
        input: Value,
        epoch: u64,
        permit: oracle_core::member_mutation::MemberMutationPermit,
    ) -> Result<Value> {
        self.invoke_inner(
            PolicyContext::Discord {
                guild: actor.guild.clone(),
                user: actor.user.clone(),
                manage_guild: false,
            },
            &actor.guild,
            operation,
            input,
            None,
            None,
            Some(epoch),
            None,
            Some(permit),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)] // Distinct host-only member permit; never taken from module input.
    async fn invoke_inner(
        &self,
        actor: PolicyContext,
        guild: &GuildId,
        operation: &str,
        input: Value,
        parent: Option<Authority>,
        configuration_revision: Option<u64>,
        expected_epoch: Option<u64>,
        member: Option<oracle_core::member_read::MemberReadPermit>,
        mutation: Option<oracle_core::member_mutation::MemberMutationPermit>,
    ) -> Result<Value> {
        if !self.normal || !self.process().is_alive() {
            return Err(unavailable());
        }
        if matches!(&actor,PolicyContext::Discord{guild:actor_guild,..} if actor_guild!=guild) {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        let operation = self
            .installed
            .package
            .manifest
            .operations
            .iter()
            .find(|op| op.name == operation)
            .ok_or_else(|| error(ErrorCode::NotFound))?;
        if self.installed.package.manifest.manifest_version == 3 && operation.name == "maintenance"
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        validate_input(operation, &input)?;
        if member.is_some() && operation.audience != oracle_core::ModuleAudience::MemberRead {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        if operation.audience == oracle_core::ModuleAudience::MemberRead
            && !operation.capabilities.is_empty()
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        if (mutation.is_some())
            != (operation.audience == oracle_core::ModuleAudience::MemberMutation)
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        if let Some(permit) = &mutation {
            permit.check()?;
        }
        if let Some(permit) = &member {
            permit.check()?;
        }
        let active = self
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .cloned()
            .ok_or_else(unavailable)?;
        if expected_epoch.is_some_and(|epoch| epoch != active.epoch) {
            return Err(unavailable());
        }
        let mut capabilities: BTreeSet<String> = operation.capabilities.iter().cloned().collect();
        if !capabilities.is_subset(&active.grants) {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        let mut authority = Authority {
            member: mutation.clone(),
            callback_methods: mutation
                .as_ref()
                .map(|_| operation.callback_methods.iter().cloned().collect()),
            callback_collections: mutation
                .as_ref()
                .map(|_| operation.callback_collections.iter().cloned().collect()),
            audience: operation.audience,
            guild: guild.clone(),
            epoch: active.epoch,
            actor,
            capabilities: BTreeSet::new(),
            deadline: Instant::now() + Duration::from_millis(operation.timeout_ms),
            depth: 0,
            configuration_revision,
            cancel: member
                .as_ref()
                .map_or_else(CancellationToken::new, |permit| permit.cancellation()),
        };
        if let Some(parent) = parent {
            if &parent.guild != guild {
                return Err(error(ErrorCode::ForbiddenScope));
            }
            if parent.depth >= 8 {
                return Err(error(ErrorCode::QuotaExceeded));
            }
            if !capabilities.is_subset(&parent.capabilities) {
                return Err(error(ErrorCode::ForbiddenPermission));
            }
            capabilities = capabilities
                .intersection(&parent.capabilities)
                .cloned()
                .collect();
            authority.deadline = authority.deadline.min(parent.deadline);
            authority.depth = parent.depth + 1;
            authority.cancel = parent.cancel.child_token();
            authority.actor = parent.actor;
        }
        if let Some(permit) = &mutation {
            authority.cancel = permit.cancellation();
            authority.deadline = authority
                .deadline
                .min(Instant::from_std(permit.expires_at()));
        }
        authority.capabilities = capabilities;
        let lease = self.gate.admit(authority)?;
        let mut envelope = json!({"invocation":lease.handle,"session":self.session,"generation":self.number,"guild":guild,"epoch":active.epoch,"operation":operation.name,"input":input});
        if self.installed.package.manifest.manifest_version == 3 {
            envelope["member"] = json!(mutation.as_ref().and_then(|permit| permit.actor()));
        }
        let result = self
            .process()
            .call_with_cancel(
                "operation.invoke",
                envelope,
                lease
                    .authority
                    .deadline
                    .saturating_duration_since(Instant::now()),
                lease.authority.cancel.clone(),
            )
            .await
            .map_err(runtime_error)?;
        self.gate.authority(&lease.handle)?;
        if let Some(permit) = &member {
            permit.check()?;
        }
        validate_output(operation, &result)?;
        Ok(result)
    }
    pub async fn event(
        &self,
        guild: &GuildId,
        event: GuildEvent,
        configuration_revision: u64,
        cancel: CancellationToken,
        worker: Option<oracle_core::member_mutation::MemberMutationPermit>,
    ) -> Result<Value> {
        if !self.normal
            || !self.process().is_alive()
            || !self
                .installed
                .package
                .manifest
                .subscriptions
                .contains(&event.kind)
        {
            return Err(unavailable());
        }
        let active = self
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .cloned()
            .ok_or_else(unavailable)?;
        if !active.grants.contains("events.guild") || !active.grants.contains("config.own") {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        let maintenance = if self.installed.package.manifest.manifest_version == 3
            && event.kind == oracle_core::GuildEventKind::Maintenance
        {
            let operation = self
                .installed
                .package
                .manifest
                .operations
                .iter()
                .find(|op| op.name == "maintenance")
                .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?;
            if worker.is_none()
                || operation.audience != oracle_core::ModuleAudience::Operator
                || operation
                    .capabilities
                    .iter()
                    .any(|c| !matches!(c.as_str(), "storage.own" | "shared_cards.publish"))
            {
                return Err(error(ErrorCode::ForbiddenPermission));
            }
            Some(operation)
        } else {
            None
        };
        let authority = Authority {
            member: worker.clone(),
            callback_methods: maintenance.map(|op| op.callback_methods.iter().cloned().collect()),
            callback_collections: maintenance
                .map(|op| op.callback_collections.iter().cloned().collect()),
            audience: oracle_core::ModuleAudience::Operator,
            guild: guild.clone(),
            epoch: active.epoch,
            actor: PolicyContext::LocalOperator,
            capabilities: self
                .installed
                .package
                .manifest
                .capabilities
                .iter()
                .filter(|c| {
                    active.grants.contains(*c)
                        && maintenance.is_none_or(|op| op.capabilities.contains(c))
                })
                .cloned()
                .collect(),
            deadline: Instant::now() + Duration::from_secs(30),
            depth: 0,
            configuration_revision: Some(configuration_revision),
            cancel: worker
                .as_ref()
                .map_or(cancel, |permit| permit.cancellation()),
        };
        let lease = self.gate.admit(authority)?;
        let mut envelope = json!({
            "invocation":lease.handle,"session":self.session,"generation":self.number,"guild":guild,"epoch":active.epoch,
            "operation":"event.deliver","input":event
        });
        if self.installed.package.manifest.manifest_version == 3 {
            envelope["member"] = Value::Null;
        }
        let value = self
            .process()
            .call_with_cancel(
                "event.deliver",
                envelope,
                Duration::from_secs(30),
                lease.authority.cancel.clone(),
            )
            .await
            .map_err(runtime_error)?;
        self.gate.authority(&lease.handle)?;
        Ok(value)
    }
    /// Quiescing closes new admission while existing callbacks retain their leases.
    pub async fn quiesce(&self, guild: Option<&GuildId>, grace: Duration) -> Result<bool> {
        let mut cleanup = QuiesceGuard {
            generation: self,
            armed: true,
        };
        self.gate.close(guild);
        let drained = self.gate.drain(guild, grace).await;
        self.gate.fence(guild);
        if !drained {
            return Ok(false);
        }
        let result = match guild {
            Some(guild) => {
                let Some(active) = self.activations.lock().unwrap().get(guild).cloned() else {
                    cleanup.armed = false;
                    return Ok(true);
                };
                self.process()
                    .call(
                        "deactivate",
                        json!({"guild":guild,"epoch":active.epoch}),
                        Duration::from_secs(5),
                    )
                    .await
                    .map(|value| value == json!({"deactivated":true}))
            }
            None => self
                .process()
                .call("quiesce", json!({"guild":null}), Duration::from_secs(5))
                .await
                .map(|value| value == json!({"quiesced":true})),
        };
        let mut activations = self.activations.lock().unwrap();
        match guild {
            Some(guild) => {
                activations.remove(guild);
            }
            None => activations.clear(),
        }
        let success = result.unwrap_or(false);
        cleanup.armed = !success;
        Ok(success)
    }
    pub async fn configuration(
        &self,
        guild: &GuildId,
        method: &str,
        revision: u64,
        values: Value,
    ) -> Result<Value> {
        self.configuration_for_activation(guild, method, revision, values, true)
            .await
    }
    pub async fn configuration_for_activation(
        &self,
        guild: &GuildId,
        method: &str,
        revision: u64,
        values: Value,
        published: bool,
    ) -> Result<Value> {
        let epoch = self
            .activations
            .lock()
            .unwrap()
            .get(guild)
            .filter(|a| {
                a.grants.contains("config.own")
                    && (!published || self.gate.is_active(guild, a.epoch))
            })
            .ok_or_else(unavailable)?
            .epoch;
        if !self.normal || !self.process().is_alive() {
            return Err(unavailable());
        }
        let result = self
            .process()
            .call(
                method,
                serde_json::json!({
                    "session": self.session, "generation": self.number, "guild": guild,
                    "epoch": epoch, "revision": revision, "values": values
                }),
                Duration::from_secs(5),
            )
            .await
            .map_err(runtime_error)?;
        if !self.process().is_alive()
            || (published && !self.gate.is_active(guild, epoch))
            || self
                .activations
                .lock()
                .unwrap()
                .get(guild)
                .is_none_or(|a| a.epoch != epoch)
        {
            return Err(unavailable());
        }
        Ok(result)
    }
    pub async fn health(&self) -> Result<Value> {
        let mut health = self
            .process()
            .call("health", json!({}), Duration::from_secs(2))
            .await
            .map_err(runtime_error)?;
        let object = health.as_object_mut().ok_or_else(unavailable)?;
        object.insert("host".into(), json!({"pid":self.process().pid(),"generation":self.number,"session":self.session,"in_flight":self.gate.in_flight()}));
        Ok(health)
    }
    pub async fn force_stop(&self) -> Result<StopReport> {
        self.gate.fence(None);
        self.process().force_stop().await.map_err(runtime_error)
    }
    pub async fn stop(&self, grace: Duration) -> Result<StopReport> {
        self.gate.fence(None);
        self.process().stop(grace).await.map_err(runtime_error)
    }
}
