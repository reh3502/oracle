//! Member-only admission over the same active generation and binding fences.
use super::*;
use oracle_core::member_read::{MemberContext, MemberReadPermit, MemberReadPolicy};

pub struct MemberInvocation {
    pub value: Value,
    pub policy: MemberReadPermit,
    pub registry: crate::RegistryDispatchPermit,
}

impl ModuleManager {
    pub async fn configure_runtime_settings(
        &self,
        actor: &PolicyContext,
        settings: BTreeMap<ModuleId, crate::runtime_settings::ModuleRuntimeSettings>,
        protected_paths: &[PathBuf],
        expected_uid: u32,
    ) -> Result<()> {
        if !matches!(actor, PolicyContext::LocalOperator) {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let _lifecycle = self.lifecycle.lock().await;
        if !self.registry.read().unwrap().is_empty() {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let settings = if settings.is_empty() {
            settings
        } else {
            crate::runtime_settings::prepare_runtime_settings(
                &settings,
                self.artifacts.root(),
                protected_paths,
                expected_uid,
            )?
        };
        *self.runtime_settings.write().unwrap() = settings;
        Ok(())
    }
    pub fn runtime_settings(
        &self,
        module: &ModuleId,
    ) -> Option<crate::runtime_settings::ModuleRuntimeSettings> {
        self.runtime_settings.read().unwrap().get(module).cloned()
    }
    pub async fn configure_member_reads(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        policy: Option<MemberReadPolicy>,
    ) -> Result<()> {
        self.core.authorize_member_policy(actor, guild)?;
        self.registry_signal.mutate(|| {
            self.member_gate
                .configure(guild.clone(), module.clone(), policy)
        })
    }

    /// Called by trusted host pause/role/channel/membership event ingress.
    pub fn invalidate_member_reads(&self, guild: &GuildId) {
        self.member_gate.invalidate_guild(guild);
    }

    pub async fn member_catalog(
        &self,
        actor: &MemberContext,
        guild: &GuildId,
    ) -> Result<ModuleCatalogSnapshot> {
        self.core.authorize_member_read(actor, guild).await?;
        Ok(self.registry_signal.snapshot(|revision| {
            let mut entries = self.catalog_current(guild);
            for entry in &mut entries {
                if self
                    .member_gate
                    .check_access(actor, guild, &entry.module)
                    .is_err()
                {
                    entry.operations.clear();
                } else {
                    entry.operations.retain(|op| {
                        op.audience == ModuleAudience::MemberRead && op.capabilities.is_empty()
                    });
                }
                entry
                    .commands
                    .routes
                    .retain(|route| entry.operations.iter().any(|op| op.name == route.operation));
            }
            entries.retain(|entry| !entry.commands.routes.is_empty());
            ModuleCatalogSnapshot { revision, entries }
        }))
    }

    #[allow(clippy::too_many_arguments)] // All durable binding identity components must match.
    pub async fn invoke_member_bound(
        &self,
        actor: &MemberContext,
        guild: &GuildId,
        module: &ModuleId,
        operation: &str,
        input: Value,
        session: &str,
        expected_generation: u64,
        epoch: u64,
    ) -> Result<MemberInvocation> {
        self.core.authorize_member_read(actor, guild).await?;
        let revision = self.registry_revision();
        let generation = self.get(module)?;
        if generation.session != session || generation.number != expected_generation {
            return Err(unavailable());
        }
        self.validate_dependencies(&generation, guild)?;
        let manifest = &generation.installed.package.manifest;
        let operation = manifest
            .operations
            .iter()
            .find(|op| op.name == operation)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if operation.audience != ModuleAudience::MemberRead
            || !operation.capabilities.is_empty()
            || !manifest.commands.as_ref().is_some_and(|commands| {
                commands
                    .routes
                    .iter()
                    .any(|route| route.operation == operation.name)
            })
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let policy = self.member_gate.admit(actor, guild, module)?;
        let value = generation
            .invoke_member(actor, &operation.name, input, epoch, policy.clone())
            .await?;
        self.core.authorize_member_read(actor, guild).await?;
        policy.check()?;
        if self.registry_revision() != revision {
            return Err(unavailable());
        }
        Ok(MemberInvocation {
            value,
            policy,
            registry: self.registry_permit(revision),
        })
    }
}
