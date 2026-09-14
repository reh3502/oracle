//! Host-only authenticated mutation entry point; no command publication is implied.
use super::*;
use oracle_core::member_mutation::{MemberContext, MemberMutationPermit, MemberMutationPolicy};

pub struct MemberMutationInvocation {
    pub value: Value,
    pub policy: MemberMutationPermit,
    pub registry: crate::RegistryDispatchPermit,
}
impl ModuleManager {
    pub async fn configure_member_mutations(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        module: &ModuleId,
        policy: Option<MemberMutationPolicy>,
    ) -> Result<()> {
        self.core.authorize_member_policy(actor, guild)?;
        self.registry_signal.mutate(|| {
            self.core
                .member_mutation_gate()
                .configure(guild.clone(), module.clone(), policy)
        })
    }
    pub fn invalidate_member_mutations(&self, guild: &GuildId) {
        self.core.member_mutation_gate().invalidate_guild(guild);
    }
    #[allow(clippy::too_many_arguments)] // Every binding and identity component is host supplied.
    pub async fn invoke_member_mutation_bound(
        &self,
        actor: &MemberContext,
        interaction_id: &str,
        guild: &GuildId,
        module: &ModuleId,
        operation: &str,
        input: Value,
        session: &str,
        expected_generation: u64,
        epoch: u64,
    ) -> Result<MemberMutationInvocation> {
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
        if manifest.manifest_version != 3 || operation.audience != ModuleAudience::MemberMutation {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let policy = self.core.member_mutation_gate().admit(
            actor,
            guild,
            module,
            interaction_id,
            &manifest.member_permissions,
        )?;
        let value = generation
            .invoke_member_mutation(actor, &operation.name, input, epoch, policy.clone())
            .await?;
        self.core.authorize_member_read(actor, guild).await?;
        policy.check()?;
        if self.registry_revision() != revision {
            return Err(unavailable());
        }
        Ok(MemberMutationInvocation {
            value,
            policy,
            registry: self.registry_permit(revision),
        })
    }
}
