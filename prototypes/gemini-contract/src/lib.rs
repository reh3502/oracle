//! P5: a deliberately narrow, non-streaming Interactions REST contract probe.
//! Provider continuation stays raw JSON; this is not the production agent or SDK.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::collections::{BTreeMap, BTreeSet};

pub const MODEL: &str = "gemini-3.7-flash";
pub const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1/interactions";
pub const OPENAPI: &str = include_str!("../fixtures/interactions-v1.openapi.json");
pub const BETA_OPENAPI: &str = include_str!("../fixtures/interactions-v1beta.openapi.json");
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub name: &'static str,
    pub model: &'static str,
    pub endpoint: &'static str,
    pub api_version: &'static str,
    pub openapi: &'static str,
}
pub const STABLE_PROFILE: Profile = Profile {
    name: "stable-v1-3.7",
    model: MODEL,
    endpoint: ENDPOINT,
    api_version: "v1",
    openapi: OPENAPI,
};
pub const BETA_PROFILE: Profile = Profile {
    name: "beta-3.8",
    model: "gemini-3.8-flash",
    endpoint: "https://generativelanguage.googleapis.com/v1beta/interactions",
    api_version: "v1beta",
    openapi: BETA_OPENAPI,
};
pub fn profile(name: &str) -> Option<Profile> {
    match name {
        "stable-v1-3.7" => Some(STABLE_PROFILE),
        "beta-3.8" => Some(BETA_PROFILE),
        _ => None,
    }
}
pub const SYSTEM: &str = "You are a contract-test assistant. Use only the declared test function. The function does no external work. Follow the requested two sequential rounds. Never invent a function result.";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("invalid provider JSON or wire shape")]
    Protocol,
    #[error("unsupported or malformed schema")]
    Schema,
    #[error("tool arguments do not match the canonical schema")]
    Arguments,
    #[error("unknown tool or duplicate tool-call ID")]
    Call,
    #[error("tool round has outstanding, missing, or duplicate results")]
    Pending,
    #[error("provider turn is terminal or incomplete")]
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    RequiresAction,
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Deserialize)]
struct Wire {
    status: String,
    model: Option<String>,
    #[serde(default)]
    steps: Vec<Box<RawValue>>,
    usage: Option<Value>,
    errors: Option<Value>,
}

pub struct Turn {
    pub stop: Stop,
    pub model: Option<String>,
    pub calls: Vec<Call>,
    pub usage: Option<Value>,
    pub errors: Option<Value>,
    /// Exact bytes are retained for private, caller-controlled diagnostics only.
    pub raw_response: String,
    steps: Vec<Box<RawValue>>,
}

pub fn parse_turn(raw: &str) -> Result<Turn, Error> {
    let wire: Wire = serde_json::from_str(raw).map_err(|_| Error::Protocol)?;
    let stop = match wire.status.as_str() {
        "requires_action" => Stop::RequiresAction,
        "completed" => Stop::Completed,
        "incomplete" => Stop::Incomplete,
        "failed" => Stop::Failed,
        "cancelled" => Stop::Cancelled,
        // This non-streaming probe does not poll background interactions.
        _ => return Err(Error::Protocol),
    };
    let mut calls = Vec::new();
    for step in &wire.steps {
        let value: Value = serde_json::from_str(step.get()).map_err(|_| Error::Protocol)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        if kind == "function_call" {
            let string = |field| {
                value
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .ok_or(Error::Protocol)
            };
            let arguments = value
                .get("arguments")
                .filter(|v| v.is_object())
                .ok_or(Error::Protocol)?
                .clone();
            calls.push(Call {
                id: string("id")?,
                name: string("name")?,
                arguments,
            });
        }
    }
    // Never release even valid-looking calls from a truncated/failed response.
    if stop != Stop::RequiresAction {
        calls.clear();
    } else if calls.is_empty() {
        return Err(Error::Protocol);
    }
    Ok(Turn {
        stop,
        model: wire.model,
        calls,
        usage: wire.usage,
        errors: wire.errors,
        raw_response: raw.to_owned(),
        steps: wire.steps,
    })
}

/// The only supported descriptor dialect in this prototype. Reject every
/// unsupported keyword instead of silently dropping a constraint for the model.
pub fn check_schema(schema: &Value) -> Result<(), Error> {
    let obj = schema.as_object().ok_or(Error::Schema)?;
    let allowed = [
        "type",
        "description",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
    ];
    if obj.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Schema);
    }
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Schema)?;
    if ![
        "object", "array", "string", "integer", "number", "boolean", "null",
    ]
    .contains(&kind)
    {
        return Err(Error::Schema);
    }
    if schema.get("description").is_some_and(|d| !d.is_string()) {
        return Err(Error::Schema);
    }
    if let Some(e) = schema.get("enum") {
        let values = e
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or(Error::Schema)?;
        for value in values {
            if !type_matches(kind, value) {
                return Err(Error::Schema);
            }
        }
    }
    match kind {
        "object" => {
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .ok_or(Error::Schema)?;
            for s in properties.values() {
                check_schema(s)?;
            }
            if let Some(required) = schema.get("required") {
                let required = required.as_array().ok_or(Error::Schema)?;
                let mut seen = BTreeSet::new();
                for item in required {
                    let name = item.as_str().ok_or(Error::Schema)?;
                    if !properties.contains_key(name) || !seen.insert(name) {
                        return Err(Error::Schema);
                    }
                }
            }
            if schema
                .get("additionalProperties")
                .is_some_and(|v| v != &Value::Bool(false))
            {
                return Err(Error::Schema);
            }
            if schema.get("items").is_some() {
                return Err(Error::Schema);
            }
        }
        "array" => {
            check_schema(schema.get("items").ok_or(Error::Schema)?)?;
            if ["properties", "required", "additionalProperties"]
                .iter()
                .any(|k| schema.get(k).is_some())
            {
                return Err(Error::Schema);
            }
        }
        _ => {
            if ["properties", "required", "additionalProperties", "items"]
                .iter()
                .any(|k| schema.get(k).is_some())
            {
                return Err(Error::Schema);
            }
        }
    }
    Ok(())
}
fn type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}
fn validate(schema: &Value, value: &Value) -> Result<(), Error> {
    if !type_matches(schema["type"].as_str().ok_or(Error::Schema)?, value) {
        return Err(Error::Arguments);
    }
    if schema
        .get("enum")
        .is_some_and(|e| !e.as_array().unwrap().contains(value))
    {
        return Err(Error::Arguments);
    }
    if let Some(obj) = value.as_object() {
        let properties = schema["properties"].as_object().ok_or(Error::Schema)?;
        if let Some(required) = schema.get("required") {
            for key in required.as_array().unwrap() {
                if !obj.contains_key(key.as_str().unwrap()) {
                    return Err(Error::Arguments);
                }
            }
        }
        for (key, value) in obj {
            if let Some(s) = properties.get(key) {
                validate(s, value)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(Error::Arguments);
            }
        }
    }
    if let Some(items) = value.as_array() {
        for item in items {
            validate(&schema["items"], item)?;
        }
    }
    Ok(())
}

pub fn test_tool() -> Value {
    json!({"type":"function","name":"oracle_probe","description":"Read-only deterministic test function. Pass the round number and prior receipt.",
        "parameters":{"type":"object","properties":{"round":{"type":"integer","enum":[1,2]},"prior":{"type":"string"}},"required":["round","prior"],"additionalProperties":false}})
}

pub struct Session {
    model: String,
    tools: Vec<Value>,
    schemas: BTreeMap<String, Value>,
    history: Vec<Box<RawValue>>,
    pending: Vec<Call>,
    seen: BTreeSet<String>,
    terminal: bool,
}
impl Session {
    pub fn new(model: &str, prompt: &str, tools: Vec<Value>) -> Result<Self, Error> {
        let mut schemas = BTreeMap::new();
        for tool in &tools {
            if tool["type"] != "function" {
                return Err(Error::Schema);
            }
            let name = tool["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::Schema)?;
            let schema = tool.get("parameters").ok_or(Error::Schema)?;
            check_schema(schema)?;
            if schema["type"] != "object"
                || schemas.insert(name.to_owned(), schema.clone()).is_some()
            {
                return Err(Error::Schema);
            }
        }
        Ok(Self {
            model: model.to_owned(),
            tools,
            schemas,
            history: vec![raw(
                json!({"type":"user_input","content":[{"type":"text","text":prompt}]}),
            )],
            pending: Vec::new(),
            seen: BTreeSet::new(),
            terminal: false,
        })
    }
    /// Every request contains complete native history, policy and tools.
    /// RawValue preserves signatures, unknown step fields and number lexemes.
    pub fn request(&self, tool_choice: &str) -> Result<String, Error> {
        if self.terminal {
            return Err(Error::Terminal);
        }
        if !self.pending.is_empty() {
            return Err(Error::Pending);
        }
        #[derive(Serialize)]
        struct Request<'a> {
            model: &'a str,
            input: &'a [Box<RawValue>],
            tools: &'a [Value],
            system_instruction: &'a str,
            store: bool,
            stream: bool,
            generation_config: Value,
        }
        serde_json::to_string(&Request {
            model: &self.model,
            input: &self.history,
            tools: &self.tools,
            system_instruction: SYSTEM,
            store: false,
            stream: false,
            generation_config: json!({"max_output_tokens":1024,"tool_choice":tool_choice,"thinking_level":"low","thinking_summaries":"none"}),
        })
        .map_err(|_| Error::Protocol)
    }
    pub fn accept(&mut self, turn: Turn) -> Result<Stop, Error> {
        if self.terminal {
            return Err(Error::Terminal);
        }
        if !self.pending.is_empty() {
            return Err(Error::Pending);
        }
        let mut new_ids = BTreeSet::new();
        for call in &turn.calls {
            if self.seen.contains(&call.id) || !new_ids.insert(call.id.clone()) {
                return Err(Error::Call);
            }
            validate(
                self.schemas.get(&call.name).ok_or(Error::Call)?,
                &call.arguments,
            )?;
        }
        // Commit continuation only after the whole proposal batch validates.
        self.seen.extend(new_ids);
        self.history.extend(turn.steps);
        self.pending = turn.calls;
        self.terminal = turn.stop != Stop::RequiresAction;
        Ok(turn.stop)
    }
    pub fn calls(&self) -> &[Call] {
        &self.pending
    }
    /// Results must be a complete, unique batch. Emit in original call order.
    pub fn results(&mut self, results: Vec<(String, Value, bool)>) -> Result<(), Error> {
        if self.pending.is_empty() || results.len() != self.pending.len() {
            return Err(Error::Pending);
        }
        let mut by_id = BTreeMap::new();
        for (id, result, is_error) in results {
            if !(result.is_object() || result.is_string()) {
                return Err(Error::Protocol);
            }
            if by_id.insert(id, (result, is_error)).is_some() {
                return Err(Error::Pending);
            }
        }
        if self.pending.iter().any(|c| !by_id.contains_key(&c.id)) {
            return Err(Error::Pending);
        }
        for call in self.pending.drain(..) {
            let (result, is_error) = by_id.remove(&call.id).unwrap();
            self.history.push(raw(json!({"type":"function_result","name":call.name,"call_id":call.id,"result":result,"is_error":is_error})));
        }
        Ok(())
    }
}
fn raw(value: Value) -> Box<RawValue> {
    RawValue::from_string(value.to_string()).unwrap()
}

pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Restricted provider-error diagnostics. Never include message, descriptions,
/// quota dimensions, arbitrary metadata, headers, or raw response bodies.
/// The caller passes known secrets so even an echoed key in an otherwise valid
/// identifier is suppressed before serialization.
pub fn safe_http_error(http_status: u16, body: &str, secrets: &[&str]) -> Value {
    let mut summary = json!({"http_status":http_status,"response_sha256":digest(body.as_bytes())});
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return summary;
    };
    let error = &value["error"];
    summary["error_shape"] = json!(if error.is_object() {
        "object"
    } else if error.is_string() {
        "string"
    } else {
        "other"
    });
    if let Some(code) = error["code"].as_u64().filter(|code| *code <= 599) {
        summary["provider_code"] = json!(code);
    }
    if let Some(message) = error.as_str().or_else(|| error["message"].as_str()) {
        let lower = message.to_ascii_lowercase();
        let category = if lower.contains("insufficient quota") {
            "insufficient_quota"
        } else if lower.contains("quota") {
            "quota"
        } else if lower.contains("billing") {
            "billing"
        } else if lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("too many requests")
        {
            "rate_limit"
        } else if lower.contains("api key")
            || lower.contains("api_key")
            || lower.contains("credential")
        {
            "credentials"
        } else if lower.contains("permission") || lower.contains("forbidden") {
            "permission"
        } else if lower.contains("overload") || lower.contains("temporarily unavailable") {
            "service_capacity"
        } else if lower.contains("unsupported") || lower.contains("not supported") {
            "unsupported"
        } else {
            "unclassified"
        };
        summary["message_category"] = json!(category);
    }

    let safe = |value: &str| {
        !secrets
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    };
    let statuses = [
        "CANCELLED",
        "UNKNOWN",
        "INVALID_ARGUMENT",
        "DEADLINE_EXCEEDED",
        "NOT_FOUND",
        "ALREADY_EXISTS",
        "PERMISSION_DENIED",
        "UNAUTHENTICATED",
        "RESOURCE_EXHAUSTED",
        "FAILED_PRECONDITION",
        "ABORTED",
        "OUT_OF_RANGE",
        "UNIMPLEMENTED",
        "INTERNAL",
        "UNAVAILABLE",
        "DATA_LOSS",
    ];
    if let Some(status) = error["status"]
        .as_str()
        .filter(|status| statuses.contains(status) && safe(status))
    {
        summary["provider_status"] = json!(status);
    }
    let identifier = |value: &Value, metric: bool| -> Option<String> {
        let value = value.as_str()?;
        if !safe(value) || value.len() > 256 {
            return None;
        }
        let suffix = if metric {
            value.strip_prefix("generativelanguage.googleapis.com/")?
        } else {
            value
        };
        if suffix.is_empty()
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return None;
        }
        Some(value.to_owned())
    };
    let mut quotas = Vec::new();
    if let Some(details) = error["details"].as_array() {
        for detail in details.iter().take(16) {
            if detail["@type"] == "type.googleapis.com/google.rpc.QuotaFailure"
                && let Some(violations) = detail["violations"].as_array()
            {
                for violation in violations.iter().take(16) {
                    let mut quota = json!({});
                    if let Some(metric) = identifier(&violation["quotaMetric"], true) {
                        quota["metric"] = json!(metric);
                    }
                    if let Some(limit) = identifier(&violation["quotaId"], false) {
                        quota["limit_id"] = json!(limit);
                    }
                    if let Some(number) = violation["quotaValue"].as_u64().or_else(|| {
                        violation["quotaValue"]
                            .as_str()
                            .and_then(|s| s.parse::<u64>().ok())
                    }) {
                        quota["limit_value"] = json!(number);
                    }
                    if quota.as_object().is_some_and(|q| !q.is_empty()) {
                        quotas.push(quota);
                    }
                }
            }
            if detail["@type"] == "type.googleapis.com/google.rpc.ErrorInfo" {
                let metadata = &detail["metadata"];
                let mut quota = json!({});
                if let Some(metric) = identifier(&metadata["quota_metric"], true) {
                    quota["metric"] = json!(metric);
                }
                if let Some(limit) = identifier(&metadata["quota_limit"], false) {
                    quota["limit_id"] = json!(limit);
                }
                if let Some(number) = metadata["quota_limit_value"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    quota["limit_value"] = json!(number);
                }
                if quota.as_object().is_some_and(|q| !q.is_empty()) {
                    quotas.push(quota);
                }
            }
            if detail["@type"] == "type.googleapis.com/google.rpc.RetryInfo"
                && let Some(delay) = detail["retryDelay"].as_str().filter(|delay| safe(delay))
                && let Some(seconds) = delay
                    .strip_suffix('s')
                    .filter(|s| s.len() <= 24 && s.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|s| s.is_finite() && *s >= 0.0 && *s <= 604800.0)
            {
                summary["retry_after_seconds"] = json!(seconds);
            }
        }
    }
    if !quotas.is_empty() {
        summary["quotas"] = json!(quotas);
    }
    summary
}

/// Compile-time source identities bind live evidence to the actual built probe.
pub fn source_hashes() -> BTreeMap<String, String> {
    let sources: &[(&str, &[u8])] = &[
        ("Cargo.toml", include_bytes!("../Cargo.toml")),
        ("Cargo.lock", include_bytes!("../Cargo.lock")),
        ("src/lib.rs", include_bytes!("lib.rs")),
        ("src/bin/live.rs", include_bytes!("bin/live.rs")),
        ("fixtures/interactions-v1.openapi.json", OPENAPI.as_bytes()),
        (
            "fixtures/interactions-v1beta.openapi.json",
            BETA_OPENAPI.as_bytes(),
        ),
    ];
    sources
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), digest(bytes)))
        .collect()
}
