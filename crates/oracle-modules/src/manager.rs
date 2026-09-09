//! Host-owned registry. Executables and feature manifests enter only at runtime.
use crate::{
    admission::Authority,
    dependencies,
    generation::{ContractRouter, Generation},
    package::ArtifactStore,
};
use async_trait::async_trait;
use oracle_core::*;
use oracle_process::ProcessRuntime;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[path = "configuration.rs"]
mod configuration;
pub use configuration::{
    ConfigurationPlan, ConfigurationPolicy, ConfigurationReceipt, ConfigurationStatus,
};
#[path = "events.rs"]
mod events;
pub use events::{
    EventDispatch, EventHealth, NotificationCheck, NotificationRequest, NotificationTransport,
};
mod recovery;
mod upgrade;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModuleCatalogEntry {
    pub module: ModuleId,
    pub session: String,
    pub generation: u64,
    pub epoch: u64,
    pub operations: Vec<ModuleOperation>,
    pub commands: ModuleCommands,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModuleCatalogSnapshot {
    pub revision: u64,
    pub entries: Vec<ModuleCatalogEntry>,
}
pub struct ModuleManager {
    registry_signal: Arc<crate::registry::RegistrySignal>,
    event_services: RwLock<Option<events::EventServices>>,
    event_queues: std::sync::Mutex<BTreeMap<(ModuleId, GuildId), events::EventQueue>>,
    event_tasks: tasks::HostTasks,
    configuration_services: RwLock<Option<configuration::ConfigurationServices>>,
    configuration_plans: std::sync::Mutex<BTreeMap<String, configuration::BoundPlan>>,
    repository: Arc<dyn ModuleRepository>,
    core: Arc<CoreService>,
    artifacts: ArtifactStore,
    runtime: ProcessRuntime,
    registry: RwLock<BTreeMap<ModuleId, Arc<Generation>>>,
    lifecycle: tokio::sync::Mutex<()>,
    next: AtomicU64,
    effect_tasks: tasks::HostTasks,
    effect_slots: Arc<tokio::sync::Semaphore>,
    transport: Arc<dyn crate::SendTransport>,
    recovery: std::sync::Mutex<BTreeMap<ModuleId, recovery::Recovery>>,
}
fn unavailable() -> Error {
    Error::new(ErrorCode::ModuleUnavailable)
}
// Keep one guild command slot for /oracle. Count only explicitly published,
// granted routes; callers exclude the candidate's previous activation.
fn validate_command_admission(
    candidate: &ModuleManifest,
    grants: &BTreeSet<String>,
    active: &[(ModuleManifest, BTreeSet<String>)],
) -> Result<()> {
    let Some(commands) = &candidate.commands else {
        return Ok(());
    };
    if active.iter().any(|(manifest, _)| {
        manifest
            .commands
            .as_ref()
            .is_some_and(|other| other.namespace == commands.namespace)
    }) {
        return Err(Error::new(ErrorCode::Conflict));
    }
    let publishes = |manifest: &ModuleManifest, grants: &BTreeSet<String>| {
        manifest.commands.as_ref().is_some_and(|commands| {
            commands.routes.iter().any(|route| {
                manifest.operations.iter().any(|operation| {
                    operation.name == route.operation
                        && operation
                            .capabilities
                            .iter()
                            .all(|capability| grants.contains(capability))
                })
            })
        })
    };
    let count = active
        .iter()
        .filter(|(manifest, grants)| publishes(manifest, grants))
        .count()
        + usize::from(publishes(candidate, grants));
    if count > 99 {
        return Err(Error::new(ErrorCode::QuotaExceeded));
    }
    Ok(())
}
// Own provisional processes across persistence awaits. A cancelled lifecycle
// request must never leave unpublished code running in the background.
struct Provisional(Option<Arc<Generation>>);
impl Drop for Provisional {
    fn drop(&mut self) {
        if let Some(generation) = &self.0 {
            generation.gate.fence(None);
            generation.process().request_stop();
        }
    }
}
// Keep cleanup reachable across a cancelled request. A retry must await reaping
// this generation before it may launch a replacement or migrate its documents.
struct RetainStopping<'a> {
    manager: &'a ModuleManager,
    module: ModuleId,
    generation: Arc<Generation>,
    finished: bool,
}
impl Drop for RetainStopping<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.generation.gate.fence(None);
            self.generation.process().request_stop();
            self.manager
                .registry_insert(self.module.clone(), self.generation.clone());
            self.manager.publish_counts();
        }
    }
}
struct Router(Weak<ModuleManager>);
#[async_trait]
impl ContractRouter for Router {
    async fn host_health(
        &self,
        module: &ModuleId,
        session: &str,
        generation: u64,
        authority: Authority,
    ) -> Result<ModuleHostHealth> {
        self.0
            .upgrade()
            .ok_or_else(unavailable)?
            .invocation_host_health(module, session, generation, &authority)
            .await
    }

    async fn notify(
        &self,
        module: &ModuleId,
        purpose: String,
        destination: String,
        text: String,
        authority: Authority,
        permit: crate::DispatchPermit,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        let manager = self.0.upgrade().ok_or_else(unavailable)?;
        let request = NotificationRequest {
            actor: authority.actor.clone(),
            guild: authority.guild.clone(),
            module: module.clone(),
            configuration_revision: authority
                .configuration_revision
                .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?,
            destination,
            text,
        };
        manager
            .notify(request, purpose, authority, permit, cancel)
            .await
    }

    async fn echo(
        &self,
        module: &ModuleId,
        purpose: String,
        body: Value,
        authority: Authority,
        permit: crate::DispatchPermit,
        rpc_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        use sha2::{Digest, Sha256};
        let manager = self.0.upgrade().ok_or_else(unavailable)?;
        let generation = manager.get(module)?;
        manager.validate_dependencies(&generation, &authority.guild)?;
        let slot = manager
            .effect_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::new(ErrorCode::QuotaExceeded))?;
        let purpose = format!(
            "module:{module}:echo:{:.32}",
            format!("{:x}", Sha256::digest(purpose.as_bytes()))
        );
        let core = manager.core.clone();
        let transport = manager.transport.clone();
        let shutdown = manager.effect_tasks.token();
        let (reply, receive) = tokio::sync::oneshot::channel();
        manager.effect_tasks.spawn("module_effect",async move {
            let _slot=slot;
            let cancel=authority.cancel.child_token();
            let operation=crate::effects::execute(&core,&authority.actor,&authority.guild,&purpose,body,permit,cancel.clone(),transport);
            tokio::pin!(operation);
            let result=tokio::select! { biased;
                _=rpc_cancel.cancelled()=>{cancel.cancel(); operation.await},
                _=shutdown.cancelled()=>{cancel.cancel(); operation.await},
                _=tokio::time::sleep_until(authority.deadline)=>{cancel.cancel(); operation.await},
                result=&mut operation=>result,
            };
            let _=reply.send(result);
            Ok(())
        }).map_err(|_|unavailable())?;
        receive.await.map_err(|_| unavailable())?
    }

    async fn invoke(
        &self,
        provider: &ModuleId,
        contract: &str,
        input: Value,
        authority: Authority,
    ) -> Result<Value> {
        let manager = self.0.upgrade().ok_or_else(unavailable)?;
        manager
            .core
            .authorize_module(&authority.actor, &authority.guild)
            .await?;
        let generation = manager.get(provider)?;
        manager.validate_dependencies(&generation, &authority.guild)?;
        let provided = generation
            .installed
            .package
            .manifest
            .provides
            .iter()
            .find(|p| p.name == contract)
            .ok_or_else(|| Error::new(ErrorCode::DependencyUnavailable))?;
        let configuration_revision =
            if generation
                .installed
                .package
                .manifest
                .operations
                .iter()
                .any(|op| {
                    op.name == provided.operation
                        && op.capabilities.iter().any(|c| c == "discord.notify")
                })
            {
                Some(
                    manager
                        .configuration_ready(&authority.actor, &authority.guild, &generation)
                        .await?
                        .revision,
                )
            } else {
                None
            };
        generation
            .invoke(
                authority.actor.clone(),
                &authority.guild.clone(),
                &provided.operation,
                input,
                Some(authority),
                configuration_revision,
                None,
            )
            .await
    }
}
impl ModuleManager {
    pub fn new(
        repository: Arc<dyn ModuleRepository>,
        core: Arc<CoreService>,
        root: PathBuf,
    ) -> Result<Arc<Self>> {
        Self::with_transport(repository, core, root, Arc::new(crate::EchoTransport))
    }
    /// Trusted host transports alone control the immediate dispatch boundary.
    pub fn with_transport(
        repository: Arc<dyn ModuleRepository>,
        core: Arc<CoreService>,
        root: PathBuf,
        transport: Arc<dyn crate::SendTransport>,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            registry_signal: Arc::new(crate::registry::RegistrySignal::default()),
            event_services: RwLock::new(None),
            event_queues: std::sync::Mutex::new(BTreeMap::new()),
            event_tasks: tasks::HostTasks::new(),
            configuration_services: RwLock::new(None),
            configuration_plans: std::sync::Mutex::new(BTreeMap::new()),
            effect_tasks: tasks::HostTasks::new(),
            effect_slots: Arc::new(tokio::sync::Semaphore::new(64)),
            transport,
            repository,
            core,
            artifacts: ArtifactStore::new(root)?,
            runtime: ProcessRuntime::new().map_err(|e| Error::with_source(ErrorCode::Io, e))?,
            registry: RwLock::new(BTreeMap::new()),
            lifecycle: tokio::sync::Mutex::new(()),
            next: AtomicU64::new(1),
            recovery: std::sync::Mutex::new(BTreeMap::new()),
        }))
    }
    pub fn registry_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.registry_signal.subscribe()
    }
    pub fn registry_revision(&self) -> u64 {
        self.registry_signal.snapshot(|revision| revision)
    }
    pub fn registry_permit(&self, revision: u64) -> crate::RegistryDispatchPermit {
        self.registry_signal
            .snapshot(|_| crate::RegistryDispatchPermit {
                signal: self.registry_signal.clone(),
                revision,
                processes: self
                    .registry
                    .read()
                    .unwrap()
                    .values()
                    .filter(|g| g.process().is_alive() || g.gate.has_active())
                    .map(|g| g.process().clone())
                    .collect(),
            })
    }
    fn registry_insert(&self, module: ModuleId, generation: Arc<Generation>) {
        self.registry_signal
            .mutate(|| self.registry.write().unwrap().insert(module, generation));
    }
    fn registry_remove(&self, module: &ModuleId) -> Option<Arc<Generation>> {
        self.registry_signal
            .mutate(|| self.registry.write().unwrap().remove(module))
    }
    fn watch_generation(&self, generation: &Arc<Generation>) -> Result<()> {
        let generation = generation.clone();
        let cancel = self.event_tasks.token();
        self.event_tasks.spawn("module_registry_process",async move {
            loop {
                tokio::select! {biased;_=cancel.cancelled()=>break,
                    result=generation.process().wait_stopped(Duration::from_secs(3600))=>{
                        if result.is_ok()||!generation.process().is_alive() {generation.gate.fence(None);break;}
                    }
                }
            }
            Ok(())
        }).map_err(|_|unavailable())?;
        Ok(())
    }
    fn publish_counts(&self) {
        self.prune_event_queues();
        let inventory = self
            .registry
            .read()
            .unwrap()
            .iter()
            .filter(|(_, generation)| generation.process().is_alive())
            .map(|(id, generation)| {
                (
                    id.clone(),
                    generation
                        .activations
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(guild, activation)| {
                            generation.gate.is_active(guild, activation.epoch)
                        })
                        .map(|(guild, _)| guild.clone())
                        .collect(),
                )
            })
            .collect();
        self.core.set_module_inventory(inventory);
    }
    fn number(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
    fn get(&self, module: &ModuleId) -> Result<Arc<Generation>> {
        let generation = self
            .registry
            .read()
            .unwrap()
            .get(module)
            .cloned()
            .ok_or_else(unavailable)?;
        if !generation.process().is_alive() {
            generation.gate.fence(None);
            return Err(unavailable());
        }
        Ok(generation)
    }
    pub async fn install(&self, source: &Path, trust_native: bool) -> Result<InstalledModule> {
        let _guard = self.lifecycle.lock().await;
        let installed = self.artifacts.install(source, trust_native)?;
        self.repository.install_module(&installed).await?;
        Ok(installed)
    }
    pub async fn installations(&self) -> Result<Vec<InstalledModule>> {
        self.repository.installations().await
    }
    async fn installation(&self, digest: &str) -> Result<InstalledModule> {
        self.repository
            .installations()
            .await?
            .into_iter()
            .find(|i| i.digest == digest)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))
    }
    /// Installation is static. Only this explicit operation executes trusted code.
    pub async fn load(self: &Arc<Self>, digest: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let module = self.installation(digest).await?.package.manifest.id;
        self.recovery.lock().unwrap().remove(&module);
        self.load_locked(digest).await?;
        let mut failure = None;
        for desired in self
            .repository
            .desired_activations()
            .await?
            .into_iter()
            .filter(|a| a.active && a.module == module)
        {
            if let Err(error) = self
                .activate_locked(&PolicyContext::LocalOperator, desired)
                .await
            {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn load_locked(self: &Arc<Self>, digest: &str) -> Result<()> {
        // Heap-own the orchestration future: nested storage/RPC state machines
        // otherwise accumulate large debug-build frames on the host task stack.
        Box::pin(async {
            let installed = self.installation(digest).await?;
            let module = installed.package.manifest.id.clone();
            let previous = self.registry.read().unwrap().get(&module).cloned();
            if let Some(previous) = previous {
                if previous.process().is_alive() {
                    return Err(Error::new(ErrorCode::Conflict));
                }
                let report = previous.force_stop().await?;
                if report.cleanup_error.is_some() {
                    return Err(Error::new(ErrorCode::Io));
                }
                self.registry_remove(&module);
            }
            let artifact = self.artifacts.verify(&installed)?;
            // Resume all known namespaces before ordinary code can become visible,
            // including desired-inactive guilds left by an interrupted upgrade.
            for activation in self
                .repository
                .desired_activations()
                .await?
                .into_iter()
                .filter(|a| a.module == module)
            {
                self.migrate(&installed, &activation.guild).await?;
            }
            let generation = Generation::spawn(
                &self.runtime,
                &artifact,
                installed.clone(),
                self.number(),
                self.repository.clone(),
                Arc::new(Router(Arc::downgrade(self))),
                "normal",
                self.registry_signal.clone(),
            )
            .await?;
            let mut provisional = RetainStopping {
                manager: self,
                module: module.clone(),
                generation: generation.clone(),
                finished: false,
            };
            if let Err(error) = self
                .repository
                .set_module_desired(&DesiredModule {
                    module: module.clone(),
                    digest: digest.into(),
                    loaded: true,
                })
                .await
            {
                let report = generation.force_stop().await?;
                if report.cleanup_error.is_some() {
                    return Err(Error::new(ErrorCode::Io));
                }
                provisional.finished = true;
                return Err(error);
            }
            self.watch_generation(&generation)?;
            self.registry_insert(module, generation);
            provisional.finished = true;
            self.publish_counts();
            Ok(())
        })
        .await
    }
    pub async fn activate(
        self: &Arc<Self>,
        context: &PolicyContext,
        desired: DesiredActivation,
    ) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        self.activate_locked(context, desired).await
    }
    async fn activate_locked(
        self: &Arc<Self>,
        context: &PolicyContext,
        desired: DesiredActivation,
    ) -> Result<()> {
        // Heap-own the orchestration future: nested storage/RPC state machines
        // otherwise accumulate large debug-build frames on the host task stack.
        Box::pin(async {
            self.core.authorize_module(context, &desired.guild).await?;
            if !desired.active {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let generation = self.get(&desired.module)?;
            let manifest = &generation.installed.package.manifest;
            let grants: BTreeSet<_> = desired.grants.iter().cloned().collect();
            let active_commands: Vec<_> = self
                .registry
                .read()
                .unwrap()
                .values()
                .filter_map(|other| {
                    if other.installed.package.manifest.id == manifest.id
                        || !other.process().is_alive()
                    {
                        return None;
                    }
                    let active = other
                        .activations
                        .lock()
                        .unwrap()
                        .get(&desired.guild)
                        .cloned()?;
                    other
                        .gate
                        .is_active(&desired.guild, active.epoch)
                        .then(|| (other.installed.package.manifest.clone(), active.grants))
                })
                .collect();
            validate_command_admission(manifest, &grants, &active_commands)?;
            if !manifest.subscriptions.is_empty() {
                self.require_event_intents(&generation)?;
                self.validate_subscription_policy(context, &desired.guild, &generation)
                    .await?;
            }
            if grants.iter().any(|c| !manifest.capabilities.contains(c)) {
                return Err(Error::new(ErrorCode::ForbiddenPermission));
            }
            let (providers, mut graph) = self.guild_graph(&desired.guild);
            let bindings = dependencies::resolve(manifest, &providers, &desired.bindings)?;
            let mut manifests = providers;
            manifests.insert(manifest.id.clone(), manifest.clone());
            graph.insert(manifest.id.clone(), bindings.clone());
            dependencies::validate_graph(&manifests, &graph)?;
            self.migrate(&generation.installed, &desired.guild).await?;
            generation
                .prepare_activation(
                    desired.guild.clone(),
                    self.number(),
                    grants,
                    bindings.clone(),
                )
                .await?;
            let mut provisional = Provisional(Some(generation.clone()));
            let desired = DesiredActivation {
                bindings,
                ..desired
            };
            if let Err(error) = self.repository.set_activation_desired(&desired).await {
                let _ = generation
                    .quiesce(Some(&desired.guild), Duration::from_secs(2))
                    .await;
                return Err(error);
            }
            self.restore_configuration_locked(&desired.guild, &generation)
                .await?;
            // Admission becomes visible only after activation intent and restored
            // configuration have both been verified.
            // Keep this publication synchronous with disarming cancellation cleanup.
            self.validate_subscription_policy(context, &desired.guild, &generation)
                .await?;
            generation.publish_activation(&desired.guild)?;
            provisional.0 = None;
            self.publish_counts();
            Ok(())
        })
        .await
    }
    fn guild_graph(
        &self,
        guild: &GuildId,
    ) -> (
        BTreeMap<ModuleId, ModuleManifest>,
        BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
    ) {
        let mut manifests = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        for (id, generation) in self.registry.read().unwrap().iter() {
            if !generation.process().is_alive() {
                continue;
            }
            if let Some(active) = generation.activations.lock().unwrap().get(guild)
                && generation.gate.is_active(guild, active.epoch)
            {
                manifests.insert(id.clone(), generation.installed.package.manifest.clone());
                bindings.insert(id.clone(), active.bindings.clone());
            }
        }
        (manifests, bindings)
    }
    pub async fn invoke(
        &self,
        context: &PolicyContext,
        module: &ModuleId,
        guild: &GuildId,
        operation: &str,
        input: Value,
    ) -> Result<Value> {
        self.core.authorize_module(context, guild).await?;
        let generation = self.get(module)?;
        self.validate_dependencies(&generation, guild)?;
        let configuration_revision = if generation
            .installed
            .package
            .manifest
            .operations
            .iter()
            .any(|op| op.name == operation && op.capabilities.iter().any(|c| c == "discord.notify"))
        {
            Some(
                self.configuration_ready(context, guild, &generation)
                    .await?
                    .revision,
            )
        } else {
            None
        };
        generation
            .invoke(
                context.clone(),
                guild,
                operation,
                input,
                None,
                configuration_revision,
                None,
            )
            .await
    }
    pub async fn catalog(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
    ) -> Result<Vec<ModuleCatalogEntry>> {
        Ok(self.catalog_snapshot(actor, guild).await?.entries)
    }
    pub async fn catalog_snapshot(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
    ) -> Result<ModuleCatalogSnapshot> {
        self.core.authorize_module(actor, guild).await?;
        Ok(self
            .registry_signal
            .snapshot(|revision| ModuleCatalogSnapshot {
                revision,
                entries: self.catalog_current(guild),
            }))
    }
    fn catalog_current(&self, guild: &GuildId) -> Vec<ModuleCatalogEntry> {
        let generations: Vec<_> = self.registry.read().unwrap().values().cloned().collect();
        let mut catalog = Vec::new();
        for generation in generations {
            if !generation.process().is_alive()
                || self.validate_dependencies(&generation, guild).is_err()
            {
                continue;
            }
            let active = generation.activations.lock().unwrap().get(guild).cloned();
            let Some(active) = active else {
                continue;
            };
            if !generation.gate.is_active(guild, active.epoch) {
                continue;
            }
            let Some(mut commands) = generation.installed.package.manifest.commands.clone() else {
                continue;
            };
            let operations = generation
                .installed
                .package
                .manifest
                .operations
                .iter()
                .filter(|op| op.capabilities.iter().all(|c| active.grants.contains(c)))
                .cloned()
                .collect::<Vec<_>>();
            commands
                .routes
                .retain(|route| operations.iter().any(|op| op.name == route.operation));
            if !commands.routes.is_empty() {
                let operations = operations
                    .into_iter()
                    .filter(|op| {
                        commands
                            .routes
                            .iter()
                            .any(|route| route.operation == op.name)
                    })
                    .collect();
                catalog.push(ModuleCatalogEntry {
                    module: generation.installed.package.manifest.id.clone(),
                    session: generation.session.clone(),
                    generation: generation.number,
                    epoch: active.epoch,
                    operations,
                    commands,
                });
            }
        }
        catalog
    }
    #[allow(clippy::too_many_arguments)] // Persisted command identity and invocation share one pinned generation.
    pub async fn invoke_bound(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        operation: &str,
        input: Value,
        session: &str,
        expected_generation: u64,
        epoch: u64,
    ) -> Result<Value> {
        self.core.authorize_module(actor, guild).await?;
        let generation = self.get(module)?;
        if generation.session != session || generation.number != expected_generation {
            return Err(unavailable());
        }
        self.validate_dependencies(&generation, guild)?;
        let configuration_revision = if generation
            .installed
            .package
            .manifest
            .operations
            .iter()
            .any(|op| op.name == operation && op.capabilities.iter().any(|c| c == "discord.notify"))
        {
            Some(
                self.configuration_ready(actor, guild, &generation)
                    .await?
                    .revision,
            )
        } else {
            None
        };
        generation
            .invoke(
                actor.clone(),
                guild,
                operation,
                input,
                None,
                configuration_revision,
                Some(epoch),
            )
            .await
    }
    fn validate_dependencies(&self, generation: &Generation, guild: &GuildId) -> Result<()> {
        let (providers, active_bindings) = self.guild_graph(guild);
        let mut pending = vec![generation.installed.package.manifest.id.clone()];
        let mut manifests = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if manifests.contains_key(&id) {
                continue;
            }
            let mut manifest = providers
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::new(ErrorCode::DependencyUnavailable))?;
            manifest.consumes.retain(|c| !c.optional);
            let explicit = active_bindings
                .get(&id)
                .ok_or_else(|| Error::new(ErrorCode::DependencyUnavailable))?
                .iter()
                .filter(|(name, _)| manifest.consumes.iter().any(|c| &c.name == *name))
                .map(|(name, id)| (name.clone(), id.clone()))
                .collect();
            let resolved = dependencies::resolve(&manifest, &providers, &explicit)?;
            pending.extend(resolved.values().cloned());
            manifests.insert(id.clone(), manifest);
            bindings.insert(id, resolved);
        }
        dependencies::validate_graph(&manifests, &bindings)
    }
    /// A provider cannot be removed while a required consumer is active.
    fn ensure_no_dependents(&self, module: &ModuleId, guild: Option<&GuildId>) -> Result<()> {
        for (id, generation) in self.registry.read().unwrap().iter() {
            if id == module {
                continue;
            }
            for (scope, active) in generation.activations.lock().unwrap().iter() {
                if guild.is_some_and(|g| g != scope) {
                    continue;
                }
                for consumed in &generation.installed.package.manifest.consumes {
                    if !consumed.optional && active.bindings.get(&consumed.name) == Some(module) {
                        return Err(Error::new(ErrorCode::DependencyUnavailable));
                    }
                }
            }
        }
        Ok(())
    }
    pub async fn unload(
        &self,
        module: &ModuleId,
        grace: Duration,
    ) -> Result<Option<oracle_process::StopReport>> {
        let _guard = self.lifecycle.lock().await;
        self.ensure_no_dependents(module, None)?;
        let generation = self.registry.read().unwrap().get(module).cloned();
        let digest = match &generation {
            Some(generation) => generation.installed.digest.clone(),
            None => {
                self.repository
                    .desired_modules()
                    .await?
                    .into_iter()
                    .find(|d| &d.module == module)
                    .ok_or_else(|| Error::new(ErrorCode::NotFound))?
                    .digest
            }
        };
        self.repository
            .set_module_desired(&DesiredModule {
                module: module.clone(),
                digest,
                loaded: false,
            })
            .await?;
        self.recovery.lock().unwrap().remove(module);
        let Some(generation) = generation else {
            return Ok(None);
        };
        let mut cleanup = RetainStopping {
            manager: self,
            module: module.clone(),
            generation: generation.clone(),
            finished: false,
        };
        generation.gate.close(None);
        self.registry_remove(module);
        self.publish_counts();
        let drained = generation.quiesce(None, grace).await?;
        let report = if drained {
            generation.stop(Duration::from_secs(2)).await?
        } else {
            generation.force_stop().await?
        };
        if report.cleanup_error.is_some() {
            return Err(Error::new(ErrorCode::Io));
        }
        cleanup.finished = true;
        Ok(Some(report))
    }
    /// Replay durable intent in provider order. Failures leave that module unavailable
    /// and are reported individually; they do not prevent the empty host from serving.
    pub async fn restore_desired(self: &Arc<Self>) -> Result<BTreeMap<ModuleId, ErrorCode>> {
        let _guard = self.lifecycle.lock().await;
        let mut errors = BTreeMap::new();
        let desired = self.repository.desired_modules().await?;
        for module in desired.iter().filter(|m| m.loaded) {
            if let Err(error) = self.load_locked(&module.digest).await {
                errors.insert(module.module.clone(), error.code);
            }
        }
        let mut remaining: Vec<_> = self
            .repository
            .desired_activations()
            .await?
            .into_iter()
            .filter(|a| a.active && desired.iter().any(|m| m.loaded && m.module == a.module))
            .collect();
        let mut activation_errors = BTreeMap::new();
        loop {
            let before = remaining.len();
            let mut retry = Vec::new();
            for activation in remaining {
                match self
                    .activate_locked(&PolicyContext::LocalOperator, activation.clone())
                    .await
                {
                    Ok(()) => {
                        activation_errors.remove(&(activation.module, activation.guild));
                    }
                    Err(error) => {
                        activation_errors.insert(
                            (activation.module.clone(), activation.guild.clone()),
                            error.code,
                        );
                        retry.push(activation);
                    }
                }
            }
            if retry.is_empty() || retry.len() == before {
                break;
            }
            remaining = retry;
        }
        for ((module, _guild), code) in activation_errors {
            errors.entry(module).or_insert(code);
        }
        Ok(errors)
    }
    /// Deactivation retains the shared process on a graceful drain. A forced
    /// cutoff kills that generation and reports every affected guild.
    pub async fn deactivate(
        self: &Arc<Self>,
        context: &PolicyContext,
        module: &ModuleId,
        guild: &GuildId,
        grace: Duration,
    ) -> Result<Vec<GuildId>> {
        let _guard = self.lifecycle.lock().await;
        self.core.authorize_module(context, guild).await?;
        self.ensure_no_dependents(module, Some(guild))?;
        let generation = self.get(module)?;
        let mut desired = self
            .repository
            .desired_activations()
            .await?
            .into_iter()
            .find(|a| &a.module == module && &a.guild == guild)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        desired.active = false;
        self.repository.set_activation_desired(&desired).await?;
        let impacted: Vec<_> = generation
            .activations
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        if generation.quiesce(Some(guild), grace).await? {
            self.publish_counts();
            return Ok(vec![guild.clone()]);
        }
        generation.gate.close(None);
        generation.gate.fence(None);
        self.registry_remove(module);
        self.publish_counts();
        generation.force_stop().await?;
        self.load_locked(&generation.installed.digest).await?;
        for activation in self
            .repository
            .desired_activations()
            .await?
            .into_iter()
            .filter(|a| &a.module == module && a.active)
        {
            self.activate_locked(&PolicyContext::LocalOperator, activation)
                .await?;
        }
        Ok(impacted)
    }
    pub async fn health(&self) -> BTreeMap<ModuleId, Value> {
        self.publish_counts();
        let generations: Vec<_> = self
            .registry
            .read()
            .unwrap()
            .iter()
            .map(|(id, g)| (id.clone(), g.clone()))
            .collect();
        let mut result = self.recovery_health();
        for (id, generation) in generations {
            let health = match generation.health().await {
                Ok(health) => health,
                Err(error) => serde_json::json!({"available":false,"error":error.code}),
            };
            result.insert(id, health);
        }
        result
    }
    pub async fn shutdown(&self) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let generations: Vec<_> = self.registry.read().unwrap().values().cloned().collect();
        for generation in generations {
            generation.gate.close(None);
            generation.gate.fence(None);
        }
        self.registry_signal
            .mutate(|| self.registry.write().unwrap().clear());
        self.publish_counts();
        let events = self.event_tasks.shutdown(Duration::from_secs(5)).await;
        let effects = self.effect_tasks.shutdown(Duration::from_secs(10)).await;
        self.runtime
            .shutdown()
            .await
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        if effects.forced || events.forced {
            return Err(Error::new(ErrorCode::UnknownOutcome));
        }
        Ok(())
    }
    async fn migrate(self: &Arc<Self>, installed: &InstalledModule, guild: &GuildId) -> Result<()> {
        // Heap-own the orchestration future: nested storage/RPC state machines
        // otherwise accumulate large debug-build frames on the host task stack.
        Box::pin(async {

        let manifest = &installed.package.manifest;
        let mut progress = self
            .repository
            .migration_status(&manifest.id, guild)
            .await?;
        if progress.data_version == manifest.data_version && progress.target_version.is_none() {
            return Ok(());
        }
        if progress.data_version > manifest.data_version {
            return Err(Error::new(ErrorCode::DataVersionMismatch));
        }
        if progress.data_version == 0 {
            self.repository
                .begin_migration(
                    &manifest.id,
                    guild,
                    0,
                    manifest.data_version,
                    &installed.digest,
                )
                .await?;
            let page = self
                .repository
                .migration_page(&manifest.id, guild, 1)
                .await?;
            if !page.documents.is_empty() || page.next_cursor.is_some() {
                return Err(Error::new(ErrorCode::MigrationMismatch));
            }
            self.repository
                .commit_migration_page(
                    &manifest.id,
                    guild,
                    &installed.digest,
                    None,
                    &[],
                    None,
                    true,
                )
                .await?;
            return Ok(());
        }
        let artifact = self.artifacts.verify(installed)?;
        let migrator = Generation::spawn(
            &self.runtime,
            &artifact,
            installed.clone(),
            self.number(),
            self.repository.clone(),
            Arc::new(Router(Arc::downgrade(self))),
            "migration",
            self.registry_signal.clone(),
        )
        .await?;
        let _provisional = Provisional(Some(migrator.clone()));
        let result = async {
            while progress.data_version < manifest.data_version {
                let step = manifest.migrations.iter().find(|m| m.from == progress.data_version).ok_or_else(|| Error::new(ErrorCode::DataVersionMismatch))?;
                progress = self.repository.begin_migration(&manifest.id,guild,step.from,step.to,&installed.digest).await?;
                loop {
                    let page = self.repository.migration_page(&manifest.id,guild,64).await?;
                    let transformed = migrator.process().call("migration.transform",serde_json::json!({"operation":step.operation,"from":step.from,"to":step.to,"documents":page.documents}),Duration::from_secs(30)).await.map_err(|e| Error::with_source(ErrorCode::MigrationMismatch,e))?;
                    let writes: Vec<DocumentWrite> = serde_json::from_value(transformed).map_err(|e| Error::with_source(ErrorCode::SchemaInvalid,e))?;
                    if writes.len() > 100 { return Err(Error::new(ErrorCode::QuotaExceeded)); }
                    for write in &writes {
                        let collection = manifest.collections.iter().find(|c| c.name == write.collection).ok_or_else(|| Error::new(ErrorCode::SchemaInvalid))?;
                        if let Some(value) = &write.value && !crate::package::schema_validator(&collection.schema)?.is_valid(value) { return Err(Error::new(ErrorCode::SchemaInvalid)); }
                    }
                    progress = self.repository.commit_migration_page(&manifest.id,guild,&installed.digest,progress.cursor.as_deref(),&writes,page.next_cursor.as_deref(),page.next_cursor.is_none()).await?;
                    if progress.target_version.is_none() { break; }
                }
            }
            Ok(())
        }.await;
        let stopped = migrator.stop(Duration::from_secs(2)).await;
        result?;
        stopped?;
        Ok(())
        }).await
    }
}

#[cfg(test)]
mod command_admission_tests {
    use super::*;
    #[test]
    fn publication_limit_counts_granted_routes_and_reserves_bootstrap() {
        let mut candidate: ModuleManifest = serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest-events.json"
        ))
        .unwrap();
        let grants: BTreeSet<String> = candidate.capabilities.iter().cloned().collect();
        let active: Vec<_> = (0..99)
            .map(|index| {
                let mut manifest = candidate.clone();
                manifest.commands.as_mut().unwrap().namespace = format!("module-{index}");
                (manifest, grants.clone())
            })
            .collect();
        validate_command_admission(&candidate, &grants, &active[..98]).unwrap();
        assert_eq!(
            validate_command_admission(&candidate, &grants, &active)
                .unwrap_err()
                .code,
            ErrorCode::QuotaExceeded
        );
        // An operation without its grant contributes no command to publication.
        for operation in &mut candidate.operations {
            operation.capabilities = vec!["host.echo".into()];
        }
        validate_command_admission(&candidate, &BTreeSet::new(), &active).unwrap();
        candidate.commands = None;
        validate_command_admission(&candidate, &grants, &active).unwrap();
    }
}
