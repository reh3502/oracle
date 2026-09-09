//! Durable host workflow records, scoped to a configured guild.
use async_trait::async_trait;
use oracle_contracts::{GuildId, Result, WorkflowKind, WorkflowRecord};
use serde_json::Value;

#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    async fn workflow_get(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        key: &str,
    ) -> Result<Option<WorkflowRecord>>;
    async fn workflow_put(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        key: &str,
        expected_revision: Option<u64>,
        value: &Value,
    ) -> Result<WorkflowRecord>;
    async fn workflow_list(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkflowRecord>>;
}
