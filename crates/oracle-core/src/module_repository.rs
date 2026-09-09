//! Host module persistence port.
use async_trait::async_trait;
use oracle_contracts::*;

/// Host-only module persistence port. Module RPC supplies opaque invocation handles,
/// never this namespace/scope. All batches and migration checkpoints are atomic.
#[async_trait]
pub trait ModuleRepository: Send + Sync {
    async fn installations(&self) -> Result<Vec<InstalledModule>>;
    async fn install_module(&self, installed: &InstalledModule) -> Result<()>;
    async fn remove_installation(&self, digest: &str) -> Result<()>;
    async fn desired_modules(&self) -> Result<Vec<DesiredModule>>;
    async fn set_module_desired(&self, desired: &DesiredModule) -> Result<()>;
    async fn desired_activations(&self) -> Result<Vec<DesiredActivation>>;
    async fn set_activation_desired(&self, activation: &DesiredActivation) -> Result<()>;
    async fn document_get(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        collection: &str,
        key: &str,
    ) -> Result<Option<ModuleDocument>>;
    async fn document_batch(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        data_version: u32,
        writes: &[DocumentWrite],
    ) -> Result<Vec<ModuleDocument>>;
    async fn migration_status(
        &self,
        module: &ModuleId,
        guild: &GuildId,
    ) -> Result<MigrationProgress>;
    /// Initialize an empty namespace or resume a digest-bound forward migration.
    async fn begin_migration(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        from: u32,
        to: u32,
        digest: &str,
    ) -> Result<MigrationProgress>;
    async fn migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        limit: u32,
    ) -> Result<MigrationPage>;
    /// Check expected cursor, update a bounded page and cursor in one transaction.
    #[allow(clippy::too_many_arguments)] // Atomic persisted migration checkpoint contract.
    async fn commit_migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        digest: &str,
        expected_cursor: Option<&str>,
        writes: &[DocumentWrite],
        next_cursor: Option<&str>,
        complete: bool,
    ) -> Result<MigrationProgress>;
}
