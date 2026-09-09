//! Shared typed requests for local CLI and authenticated Discord interactions.
use crate::structure::StructureRequest;
use async_trait::async_trait;
use oracle_core::{GuildId, ModuleId, PolicyContext, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationRequest {
    Inspect,
    Plan { request: StructureRequest },
    Show { plan: String },
    Approve { plan: String, hash: String },
    Apply { plan: String },
    ConfigurationInspect { module: ModuleId },
    ConfigurationPlan { module: ModuleId, preset: Option<String>, values: Value },
    ConfigurationApply { module: ModuleId, plan: String },
    ConfigurationRecover { module: ModuleId },
}

#[async_trait]
pub trait HumanOperations: Send + Sync {
    async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: OperationRequest,
        cancel: &CancellationToken,
    ) -> Result<Value>;
}
