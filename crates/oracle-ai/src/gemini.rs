//! Bounded, single-attempt Gemini Interactions adapter. Native history is private.
use crate::provider::*;
use async_trait::async_trait;
use reqwest::{Client, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;

#[path = "gemini/schema.rs"]
mod schema;

// Hard ceilings also apply to forged PreparedTurn values, before any network I/O.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_METADATA_BYTES: usize = MAX_REQUEST_BYTES;
const MAX_CONTINUATION_BYTES: usize = MAX_REQUEST_BYTES + MAX_RESPONSE_BYTES + MAX_METADATA_BYTES;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Minimal,
    #[default]
    Low,
    Medium,
    High,
}

/// No endpoint, client, proxy, or retry override is exposed in production.
/// Keys and native continuation intentionally have no Debug implementation.
pub struct GeminiProvider {
    profile: ModelProfile,
    thinking: ThinkingLevel,
    client: Client,
    key: HeaderValue,
    endpoint: String,
}

impl GeminiProvider {
    pub fn new(profile: ModelProfile, key: impl AsRef<str>) -> Result<Self, ProviderError> {
        Self::with_thinking(profile, key, ThinkingLevel::Low)
    }

    /// Only the explicitly evidenced profile pairs are supported. There is no fallback.
    pub fn with_thinking(
        profile: ModelProfile,
        key: impl AsRef<str>,
        thinking: ThinkingLevel,
    ) -> Result<Self, ProviderError> {
        if !matches!(
            (profile.api_version.as_str(), profile.model.as_str()),
            ("v1", "gemini-3.7-flash") | ("v1beta", "gemini-3.8-flash")
        ) || profile.id.is_empty()
            || profile.id.len() > 256
            || profile.max_context_tokens == 0
            || profile.max_output_tokens == 0
            || profile.max_output_tokens > i32::MAX as u32
            || u64::from(profile.max_output_tokens) > profile.max_context_tokens
        {
            return Err(ProviderError::InvalidRequest);
        }
        let key_text = key.as_ref();
        if key_text.is_empty() || key_text.len() > 4096 {
            return Err(ProviderError::Auth);
        }
        let mut key = HeaderValue::from_str(key_text).map_err(|_| ProviderError::Auth)?;
        key.set_sensitive(true);
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ProviderError::InvalidRequest)?;
        let endpoint = format!(
            "https://generativelanguage.googleapis.com/{}/interactions",
            profile.api_version
        );
        Ok(Self {
            profile,
            thinking,
            client,
            key,
            endpoint,
        })
    }

    fn binding(&self, system: &str) -> Result<String, ProviderError> {
        let bytes = serde_json::to_vec(&(&self.profile, self.thinking, system))
            .map_err(|_| ProviderError::InvalidRequest)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn check_native(
        &self,
        native: &NativeRequest,
        historical_tools: &[NativeTool],
    ) -> Result<History, ProviderError> {
        if native.model != self.profile.model
            || native.store
            || native.stream
            || native.generation_config.thinking_level != self.thinking
            || native.generation_config.thinking_summaries != "none"
            || native.generation_config.tool_choice != "auto"
            || native.generation_config.max_output_tokens == 0
            || native.generation_config.max_output_tokens > self.profile.max_output_tokens
        {
            return Err(ProviderError::InvalidRequest);
        }
        check_tools(&native.tools)?;
        check_tools(historical_tools)?;
        check_aliases(historical_tools, &native.tools)?;
        let history = check_history(&native.input, historical_tools)?;
        if !history.pending.is_empty() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(history)
    }

    fn prepare_metadata(
        &self,
        body: &str,
        system: &str,
        historical_tools: Vec<NativeTool>,
    ) -> Result<String, ProviderError> {
        bounded_json(
            &PreparedMetadata {
                binding: self.binding(system)?,
                body_sha256: format!("{:x}", Sha256::digest(body.as_bytes())),
                historical_tools,
            },
            MAX_METADATA_BYTES,
        )
    }

    fn check_metadata(
        &self,
        body: &str,
        native: &NativeRequest,
        metadata: &str,
    ) -> Result<PreparedMetadata, ProviderError> {
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(ProviderError::InvalidRequest);
        }
        let metadata: PreparedMetadata =
            serde_json::from_str(metadata).map_err(|_| ProviderError::InvalidRequest)?;
        if metadata.binding != self.binding(&native.system_instruction)?
            || metadata.body_sha256 != format!("{:x}", Sha256::digest(body.as_bytes()))
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(metadata)
    }

    fn decode(
        &self,
        raw: &[u8],
        mut native: NativeRequest,
        mut history: History,
        mut historical_tools: Vec<NativeTool>,
    ) -> Result<ModelTurn, ProviderError> {
        let wire: Wire =
            serde_json::from_slice(raw).map_err(|_| ProviderError::ProtocolMismatch)?;
        let stop = match wire.status.as_str() {
            "requires_action" => StopReason::ToolCalls,
            "completed" => StopReason::Completed,
            "incomplete" => StopReason::Truncated,
            "failed" => StopReason::Failed,
            "cancelled" => StopReason::Cancelled,
            _ => return Err(ProviderError::ProtocolMismatch),
        };
        if wire.model.as_ref().is_some_and(|m| {
            m != &self.profile.model && m != &format!("models/{}", self.profile.model)
        }) {
            return Err(ProviderError::ProtocolMismatch);
        }
        let mut calls = Vec::new();
        let mut text = String::new();
        // Validate the complete response before releasing any proposals or visible text.
        for step in &wire.steps {
            match parse_step(step)? {
                Step::FunctionCall {
                    id,
                    name,
                    arguments,
                } => {
                    let call = ToolCall {
                        id,
                        name,
                        arguments,
                    };
                    check_call(&call, &native.tools, &mut history.seen)?;
                    if !historical_tools.iter().any(|tool| tool.name == call.name) {
                        historical_tools.push(
                            native
                                .tools
                                .iter()
                                .find(|tool| tool.name == call.name)
                                .ok_or(ProviderError::ProtocolMismatch)?
                                .clone(),
                        );
                    }
                    calls.push(call);
                }
                Step::Thought {} => {}
                Step::ModelOutput { content } => {
                    for part in content {
                        let kind = part
                            .get("type")
                            .and_then(Value::as_str)
                            .ok_or(ProviderError::ProtocolMismatch)?;
                        if kind == "text" {
                            text.push_str(
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .ok_or(ProviderError::ProtocolMismatch)?,
                            );
                        }
                    }
                }
                _ => return Err(ProviderError::ProtocolMismatch),
            }
        }
        if stop == StopReason::ToolCalls
            && (calls.is_empty() || wire.errors.is_some_and(|e| !e.is_null()))
        {
            return Err(ProviderError::ProtocolMismatch);
        }
        if stop != StopReason::ToolCalls {
            calls.clear();
        }
        native.input.extend(wire.steps);
        let state = State {
            binding: self.binding(&native.system_instruction)?,
            historical_tools,
            history: native.input,
            terminal: stop != StopReason::ToolCalls,
        };
        Ok(ModelTurn {
            calls,
            visible_text: if text.is_empty() { None } else { Some(text) },
            continuation: Continuation {
                profile: self.profile.id.clone(),
                opaque: bounded_json(&state, MAX_CONTINUATION_BYTES)
                    .map_err(|_| ProviderError::ProtocolMismatch)?,
            },
            usage: wire.usage.unwrap_or_default().into(),
            stop,
            model: wire.model,
        })
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn prepare(&self, request: ModelRequest) -> Result<PreparedTurn, ProviderError> {
        check_bounds(
            request.max_request_bytes,
            request.max_response_bytes,
            request.timeout_ms,
        )?;
        // Reject obviously oversized data before parsing opaque history or serializing it again.
        if request
            .goal
            .len()
            .saturating_add(request.system_instruction.len())
            > request.max_request_bytes
            || request
                .continuation
                .as_ref()
                .is_some_and(|c| c.opaque.len() > MAX_CONTINUATION_BYTES)
        {
            return Err(ProviderError::InvalidRequest);
        }
        let tools: Vec<_> = request
            .tools
            .into_iter()
            .map(|t| NativeTool {
                kind: "function".into(),
                name: t.name,
                description: t.description,
                parameters: t.parameters,
            })
            .collect();
        check_tools(&tools)?;
        let (input, historical_tools) = match request.continuation {
            None => {
                if !request.results.is_empty() {
                    return Err(ProviderError::InvalidRequest);
                }
                (
                    vec![raw(
                        json!({"type":"user_input","content":[{"type":"text","text":request.goal}]}),
                    )?],
                    Vec::new(),
                )
            }
            Some(continuation) => {
                if continuation.profile != self.profile.id {
                    return Err(ProviderError::InvalidRequest);
                }
                let state: State = serde_json::from_str(&continuation.opaque)
                    .map_err(|_| ProviderError::InvalidRequest)?;
                if state.terminal || state.binding != self.binding(&request.system_instruction)? {
                    return Err(ProviderError::InvalidRequest);
                }
                check_tools(&state.historical_tools)?;
                check_aliases(&state.historical_tools, &tools)?;
                let history = check_history(&state.history, &state.historical_tools)?;
                if history.pending.is_empty() || request.results.len() != history.pending.len() {
                    return Err(ProviderError::InvalidRequest);
                }
                let mut results = std::collections::BTreeMap::new();
                for result in request.results {
                    if !(result.value.is_object() || result.value.is_string())
                        || results.insert(result.call_id.clone(), result).is_some()
                    {
                        return Err(ProviderError::InvalidRequest);
                    }
                }
                let mut input = state.history;
                for call in history.pending {
                    let result = results
                        .remove(&call.id)
                        .ok_or(ProviderError::InvalidRequest)?;
                    input.push(raw(json!({"type":"function_result","name":call.name,"call_id":call.id,"result":result.value,"is_error":result.is_error}))?);
                }
                (input, state.historical_tools)
            }
        };
        let native = NativeRequest {
            model: self.profile.model.clone(),
            input,
            tools,
            system_instruction: request.system_instruction,
            store: false,
            stream: false,
            generation_config: GenerationConfig {
                max_output_tokens: request.max_output_tokens,
                tool_choice: "auto".into(),
                thinking_level: self.thinking,
                thinking_summaries: "none".into(),
            },
        };
        self.check_native(&native, &historical_tools)?;
        let body = bounded_json(&native, request.max_request_bytes)?;
        let reservation = body.len() as u64;
        if reservation.saturating_add(u64::from(request.max_output_tokens))
            > self.profile.max_context_tokens
        {
            return Err(ProviderError::ContextExceeded);
        }
        let provider_metadata =
            Some(self.prepare_metadata(&body, &native.system_instruction, historical_tools)?);
        Ok(PreparedTurn {
            body,
            provider_metadata,
            input_token_reservation: reservation,
            output_token_reservation: request.max_output_tokens,
            max_response_bytes: request.max_response_bytes,
            timeout_ms: request.timeout_ms,
        })
    }

    async fn send(
        &self,
        request: PreparedTurn,
        cancel: &CancellationToken,
    ) -> Result<ModelTurn, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let started = tokio::time::Instant::now();
        check_bounds(
            request.body.len(),
            request.max_response_bytes,
            request.timeout_ms,
        )?;
        let native: NativeRequest =
            serde_json::from_str(&request.body).map_err(|_| ProviderError::InvalidRequest)?;
        let metadata = self.check_metadata(
            &request.body,
            &native,
            request
                .provider_metadata
                .as_deref()
                .ok_or(ProviderError::InvalidRequest)?,
        )?;
        let history = self.check_native(&native, &metadata.historical_tools)?;
        if request.input_token_reservation != request.body.len() as u64
            || request.output_token_reservation != native.generation_config.max_output_tokens
        {
            return Err(ProviderError::InvalidRequest);
        }
        if request
            .input_token_reservation
            .saturating_add(u64::from(request.output_token_reservation))
            > self.profile.max_context_tokens
        {
            return Err(ProviderError::ContextExceeded);
        }
        let deadline = started + Duration::from_millis(request.timeout_ms);
        if tokio::time::Instant::now() >= deadline {
            return Err(ProviderError::Transient);
        }
        let operation = async {
            let mut response = self
                .client
                .post(&self.endpoint)
                .header("x-goog-api-key", self.key.clone())
                .header("content-type", "application/json")
                .body(request.body)
                .send()
                .await
                .map_err(|_| ProviderError::Transient)?;
            let status = response.status();
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 | 403 => ProviderError::Auth,
                    429 => ProviderError::RateLimited {
                        retry_after_ms: response
                            .headers()
                            .get("retry-after")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .and_then(|s| s.checked_mul(1000)),
                    },
                    408 | 500..=599 => ProviderError::Transient,
                    300..=399 => ProviderError::ProtocolMismatch,
                    _ => ProviderError::InvalidRequest,
                });
            }
            if response
                .content_length()
                .is_some_and(|n| n > request.max_response_bytes as u64)
            {
                return Err(ProviderError::ProtocolMismatch);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ProviderError::ProtocolMismatch)?
            {
                if bytes.len().saturating_add(chunk.len()) > request.max_response_bytes {
                    return Err(ProviderError::ProtocolMismatch);
                }
                bytes.extend_from_slice(&chunk);
            }
            let turn = self.decode(&bytes, native, history, metadata.historical_tools)?;
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProviderError::Transient);
            }
            Ok(turn)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = tokio::time::timeout_at(deadline, operation) => result.map_err(|_| ProviderError::Transient)?,
        }
    }
}

// Stop serialization at the caller's byte limit instead of allocating an oversized body.
fn bounded_json(value: &impl Serialize, max: usize) -> Result<String, ProviderError> {
    struct Bounded {
        bytes: Vec<u8>,
        max: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.len() > self.max.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("request byte limit"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded {
        bytes: Vec::new(),
        max,
    };
    serde_json::to_writer(&mut out, value).map_err(|_| ProviderError::InvalidRequest)?;
    String::from_utf8(out.bytes).map_err(|_| ProviderError::InvalidRequest)
}

fn check_bounds(request: usize, response: usize, timeout: u64) -> Result<(), ProviderError> {
    if request == 0
        || request > MAX_REQUEST_BYTES
        || response == 0
        || response > MAX_RESPONSE_BYTES
        || timeout == 0
        || timeout > MAX_TIMEOUT_MS
    {
        Err(ProviderError::InvalidRequest)
    } else {
        Ok(())
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    parameters: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationConfig {
    max_output_tokens: u32,
    tool_choice: String,
    thinking_level: ThinkingLevel,
    thinking_summaries: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRequest {
    model: String,
    input: Vec<Box<RawValue>>,
    tools: Vec<NativeTool>,
    system_instruction: String,
    store: bool,
    stream: bool,
    generation_config: GenerationConfig,
}
/// Private handoff data, never part of Google's request body and never logged.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedMetadata {
    binding: String,
    body_sha256: String,
    historical_tools: Vec<NativeTool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    binding: String,
    // Only aliases actually called are retained. These are not active declarations.
    historical_tools: Vec<NativeTool>,
    history: Vec<Box<RawValue>>,
    terminal: bool,
}
#[derive(Deserialize)]
struct Wire {
    status: String,
    model: Option<String>,
    steps: Vec<Box<RawValue>>,
    usage: Option<WireUsage>,
    errors: Option<Value>,
}
#[derive(Default, Deserialize)]
struct WireUsage {
    total_input_tokens: Option<u64>,
    total_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    total_cached_tokens: Option<u64>,
    total_thought_tokens: Option<u64>,
}
impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Self {
            input_tokens: u.total_input_tokens,
            output_tokens: u.total_output_tokens,
            total_tokens: u.total_tokens,
            cached_tokens: u.total_cached_tokens,
            reasoning_tokens: u.total_thought_tokens,
        }
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Step {
    UserInput {
        content: Vec<Value>,
    },
    Thought {},
    ModelOutput {
        content: Vec<Value>,
    },
    FunctionCall {
        id: String,
        name: String,
        arguments: Value,
    },
    FunctionResult {
        call_id: String,
        name: String,
        result: Value,
        is_error: bool,
    },
}
fn parse_step(raw: &RawValue) -> Result<Step, ProviderError> {
    serde_json::from_str(raw.get()).map_err(|_| ProviderError::ProtocolMismatch)
}
fn raw(value: Value) -> Result<Box<RawValue>, ProviderError> {
    serde_json::value::to_raw_value(&value).map_err(|_| ProviderError::InvalidRequest)
}
fn check_tools(tools: &[NativeTool]) -> Result<(), ProviderError> {
    let mut names = BTreeSet::new();
    for t in tools {
        if t.kind != "function"
            || t.name.is_empty()
            || t.name.len() > 64
            || !t
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !t.name.as_bytes()[0].is_ascii_alphabetic()
            || !names.insert(&t.name)
            || t.parameters["type"] != "object"
        {
            return Err(ProviderError::InvalidRequest);
        }
        schema::check_schema(&t.parameters)?;
    }
    Ok(())
}
// Host selection may add/remove tools, but a used alias cannot acquire new semantics.
fn check_aliases(historical: &[NativeTool], active: &[NativeTool]) -> Result<(), ProviderError> {
    for tool in active {
        if historical
            .iter()
            .any(|old| old.name == tool.name && old != tool)
        {
            return Err(ProviderError::InvalidRequest);
        }
    }
    Ok(())
}

fn check_call(
    call: &ToolCall,
    tools: &[NativeTool],
    seen: &mut BTreeSet<String>,
) -> Result<(), ProviderError> {
    if call.id.is_empty() || !seen.insert(call.id.clone()) || !call.arguments.is_object() {
        return Err(ProviderError::InvalidToolCall);
    }
    let tool = tools
        .iter()
        .find(|t| t.name == call.name)
        .ok_or(ProviderError::InvalidToolCall)?;
    schema::validate(&tool.parameters, &call.arguments).map_err(|error| match error {
        ProviderError::ProtocolMismatch => ProviderError::InvalidToolCall,
        error => error,
    })
}
#[derive(Default)]
struct History {
    pending: Vec<ToolCall>,
    seen: BTreeSet<String>,
}
fn check_history(input: &[Box<RawValue>], tools: &[NativeTool]) -> Result<History, ProviderError> {
    if input.is_empty() {
        return Err(ProviderError::InvalidRequest);
    }
    let mut h = History::default();
    let mut resolved = 0;
    for (index, step) in input.iter().enumerate() {
        match parse_step(step)? {
            Step::UserInput { content }
                if index == 0
                    && content.len() == 1
                    && content[0]["type"] == "text"
                    && content[0]["text"].is_string() => {}
            Step::FunctionCall {
                id,
                name,
                arguments,
            } if index > 0 && resolved == 0 => {
                let call = ToolCall {
                    id,
                    name,
                    arguments,
                };
                check_call(&call, tools, &mut h.seen).map_err(|error| match error {
                    ProviderError::InvalidToolCall => ProviderError::InvalidRequest,
                    error => error,
                })?;
                h.pending.push(call);
            }
            Step::FunctionResult {
                call_id,
                name,
                result,
                is_error,
            } if index > 0 => {
                let _ = is_error;
                let call = h
                    .pending
                    .get(resolved)
                    .ok_or(ProviderError::InvalidRequest)?;
                if call.id != call_id
                    || call.name != name
                    || !(result.is_object() || result.is_string())
                {
                    return Err(ProviderError::InvalidRequest);
                }
                resolved += 1;
                if resolved == h.pending.len() {
                    h.pending.clear();
                    resolved = 0;
                }
            }
            Step::Thought {} | Step::ModelOutput { .. } if index > 0 && resolved == 0 => {}
            _ => return Err(ProviderError::InvalidRequest),
        }
    }
    if resolved != 0 {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(h)
}

#[cfg(test)]
#[path = "gemini/tests.rs"]
mod tests;
