//! Provider-neutral turns with a separate opaque, lossless continuation.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub id: String,
    pub model: String,
    pub api_version: String,
    pub max_context_tokens: u64,
    pub max_output_tokens: u32,
}

/// Never derive Debug: native continuation can contain private opaque reasoning.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    pub profile: String,
    pub opaque: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub value: Value,
    pub is_error: bool,
}

pub struct ModelRequest {
    pub goal: String,
    pub system_instruction: String,
    pub tools: Vec<ToolDefinition>,
    pub continuation: Option<Continuation>,
    pub results: Vec<ToolResult>,
    pub max_output_tokens: u32,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
}

/// Missing provider usage stays unknown; the host retains its reservation.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ToolCalls,
    Completed,
    Refused,
    Truncated,
    Failed,
    Cancelled,
}

pub struct ModelTurn {
    pub calls: Vec<ToolCall>,
    pub visible_text: Option<String>,
    pub continuation: Continuation,
    pub usage: Usage,
    pub stop: StopReason,
    pub model: Option<String>,
}

/// Prepared bytes are provider-owned and must never be logged. Reserve before send.
pub struct PreparedTurn {
    /// Provider-private preparation context; never included in the HTTP body or logs.
    pub provider_metadata: Option<String>,
    pub body: String,
    pub input_token_reservation: u64,
    pub output_token_reservation: u32,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
}

/// Fixed categories only: provider bodies, URLs and credentials are not diagnostics.
#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderError {
    #[error("provider authentication failed")]
    Auth,
    #[error("provider rate limited")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("transient provider failure")]
    Transient,
    #[error("invalid provider request")]
    InvalidRequest,
    #[error("provider context limit exceeded")]
    ContextExceeded,
    #[error("provider protocol mismatch")]
    ProtocolMismatch,
    /// A parsed model response proposed a call outside its advertised contract.
    #[error("provider returned an invalid tool call")]
    InvalidToolCall,
    #[error("provider request cancelled")]
    Cancelled,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn profile(&self) -> &ModelProfile;
    /// No I/O. Validate complete call-result batches and produce bounded native input.
    fn prepare(&self, request: ModelRequest) -> Result<PreparedTurn, ProviderError>;
    /// One potentially billable attempt; retries and admission belong to the host.
    async fn send(
        &self,
        request: PreparedTurn,
        cancel: &CancellationToken,
    ) -> Result<ModelTurn, ProviderError>;
}
