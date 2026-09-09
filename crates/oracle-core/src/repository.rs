//! Host operation and effect persistence port.
use async_trait::async_trait;
use oracle_contracts::*;

/// Host-only repository port. Untrusted requests enter CoreService, never this port.
#[async_trait]
pub trait Repository: Send + Sync {
    async fn status(&self, guild: Option<&GuildId>) -> Result<Status>;
    async fn set_paused(
        &self,
        guild: &GuildId,
        paused: bool,
        expected_revision: u64,
        actor: &str,
        operation: &OperationId,
    ) -> Result<ControlReceipt>;
    async fn begin_operation(&self, operation: &Operation) -> Result<()>;
    /// Stable guild/purpose reservation: an existing effect is returned, never overwritten.
    async fn reserve_effect(&self, effect: &Effect) -> Result<Effect>;
    async fn effect(&self, guild: &GuildId, id: &EffectId) -> Result<Effect>;
    async fn transition_effect(
        &self,
        guild: &GuildId,
        id: &EffectId,
        expected_revision: u64,
        next: EffectState,
        receipt: Option<serde_json::Value>,
    ) -> Result<Effect>;
    async fn finish_operation(
        &self,
        guild: &GuildId,
        id: &OperationId,
        state: OperationState,
    ) -> Result<()>;
    async fn recovery(&self, guild: &GuildId, limit: u32) -> Result<Vec<Effect>>;
}
