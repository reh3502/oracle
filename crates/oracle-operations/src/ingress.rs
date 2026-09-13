//! Shared typed requests for local CLI and authenticated Discord interactions.
use crate::structure::StructureRequest;
use async_trait::async_trait;
use oracle_core::{GuildId, ModuleId, PolicyContext, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Authenticated Discord option values. Duplicate option names are rejected at ingress.
#[derive(Clone, Debug)]
pub struct PublishedRequest {
    pub command_id: String,
    pub command_name: String,
    pub route: String,
    pub options: serde_json::Map<String, Value>,
}
pub struct PublishedReply {
    pub value: Value,
    pub text: Option<String>,
    pub policy: Option<oracle_core::member_read::MemberReadPermit>,
    pub fence: Option<std::sync::Arc<dyn crate::executor::DispatchFence>>,
}

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
    /// Resolve the currently bound route before selecting member-specific HTTP
    /// checks. Invocation resolves it again to fence intervening registry changes.
    async fn published_uses_member_identity(
        &self,
        _context: &PolicyContext,
        _member: &oracle_core::member_read::MemberContext,
        _guild: &GuildId,
        _request: &PublishedRequest,
    ) -> Result<bool> {
        Ok(false)
    }
    fn ai_available(&self) -> bool {
        false
    }
    async fn execute_published(
        &self,
        context: &PolicyContext,
        _member: &oracle_core::member_read::MemberContext,
        guild: &GuildId,
        request: PublishedRequest,
        cancel: &CancellationToken,
    ) -> Result<PublishedReply> {
        // Legacy adapters can retain their existing JSON-only behavior.
        let input = match request.options.len() {
            0 => serde_json::json!({}),
            1 => serde_json::from_str(
                request
                    .options
                    .get("input")
                    .and_then(Value::as_str)
                    .ok_or_else(|| oracle_core::Error::new(oracle_core::ErrorCode::InvalidInput))?,
            )
            .map_err(|_| oracle_core::Error::new(oracle_core::ErrorCode::InvalidInput))?,
            _ => {
                return Err(oracle_core::Error::new(
                    oracle_core::ErrorCode::InvalidInput,
                ));
            }
        };
        let value = self
            .execute(
                context,
                guild,
                OperationRequest::InvokePublished {
                    command_id: request.command_id,
                    command_name: request.command_name,
                    route: request.route,
                    input,
                },
                cancel,
            )
            .await?;
        Ok(PublishedReply {
            value,
            text: None,
            policy: None,
            fence: None,
        })
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
