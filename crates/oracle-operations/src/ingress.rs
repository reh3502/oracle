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
    Agent {
        request: AgentRequest,
    },
    InvokePublished {
        command_id: String,
        command_name: String,
        route: String,
        input: Value,
    },
    Inspect,
    Plan {
        request: StructureRequest,
    },
    Show {
        plan: String,
    },
    Approve {
        plan: String,
        hash: String,
    },
    Apply {
        plan: String,
    },
    ConfigurationInspect {
        module: ModuleId,
    },
    ConfigurationPlan {
        module: ModuleId,
        preset: Option<String>,
        values: Value,
    },
    ConfigurationApply {
        module: ModuleId,
        plan: String,
    },
    ConfigurationRecover {
        module: ModuleId,
    },
}

#[async_trait]
pub trait HumanOperations: Send + Sync {
    fn ai_available(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: OperationRequest,
        cancel: &CancellationToken,
    ) -> Result<Value>;
}

/// Authenticated human controls. These requests are never model tools.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRequest {
    Ask {
        goal: String,
    },
    Inspect {
        run: String,
    },
    Cancel {
        run: String,
    },
    Resume {
        run: String,
        #[serde(default)]
        clarification: Option<String>,
    },
    Approve {
        run: String,
        plan: String,
        hash: String,
    },
}
