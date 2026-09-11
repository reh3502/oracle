//! Deliberately portable descriptor subset established by the offline contract probe.
use crate::provider::ProviderError;
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn check_schema(schema: &Value) -> Result<(), ProviderError> {
    check_schema_at(schema, 0)
}
fn check_schema_at(schema: &Value, depth: usize) -> Result<(), ProviderError> {
    if depth > 32 {
        return Err(ProviderError::InvalidRequest);
    }
    let obj = schema.as_object().ok_or(ProviderError::InvalidRequest)?;
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
        return Err(ProviderError::InvalidRequest);
    }
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProviderError::InvalidRequest)?;
    if ![
        "object", "array", "string", "integer", "number", "boolean", "null",
    ]
    .contains(&kind)
    {
        return Err(ProviderError::InvalidRequest);
    }
    if schema.get("description").is_some_and(|d| !d.is_string()) {
        return Err(ProviderError::InvalidRequest);
    }
    if let Some(e) = schema.get("enum") {
        let values = e
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or(ProviderError::InvalidRequest)?;
        for value in values {
            if !type_matches(kind, value) {
                return Err(ProviderError::InvalidRequest);
            }
        }
    }
    match kind {
        "object" => {
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .ok_or(ProviderError::InvalidRequest)?;
            for s in properties.values() {
                check_schema_at(s, depth + 1)?;
            }
            if let Some(required) = schema.get("required") {
                let required = required.as_array().ok_or(ProviderError::InvalidRequest)?;
                let mut seen = BTreeSet::new();
                for item in required {
                    let name = item.as_str().ok_or(ProviderError::InvalidRequest)?;
                    if !properties.contains_key(name) || !seen.insert(name) {
                        return Err(ProviderError::InvalidRequest);
                    }
                }
            }
            if schema
                .get("additionalProperties")
                .is_some_and(|v| v != &Value::Bool(false))
            {
                return Err(ProviderError::InvalidRequest);
            }
            if schema.get("items").is_some() {
                return Err(ProviderError::InvalidRequest);
            }
        }
        "array" => {
            check_schema_at(
                schema.get("items").ok_or(ProviderError::InvalidRequest)?,
                depth + 1,
            )?;
            if ["properties", "required", "additionalProperties"]
                .iter()
                .any(|k| schema.get(k).is_some())
            {
                return Err(ProviderError::InvalidRequest);
            }
        }
        _ => {
            if ["properties", "required", "additionalProperties", "items"]
                .iter()
                .any(|k| schema.get(k).is_some())
            {
                return Err(ProviderError::InvalidRequest);
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
pub(super) fn validate(schema: &Value, value: &Value) -> Result<(), ProviderError> {
    if !type_matches(
        schema["type"]
            .as_str()
            .ok_or(ProviderError::InvalidRequest)?,
        value,
    ) {
        return Err(ProviderError::ProtocolMismatch);
    }
    if schema
        .get("enum")
        .is_some_and(|e| !e.as_array().unwrap().contains(value))
    {
        return Err(ProviderError::ProtocolMismatch);
    }
    if let Some(obj) = value.as_object() {
        let properties = schema["properties"]
            .as_object()
            .ok_or(ProviderError::InvalidRequest)?;
        if let Some(required) = schema.get("required") {
            for key in required.as_array().unwrap() {
                if !obj.contains_key(key.as_str().unwrap()) {
                    return Err(ProviderError::ProtocolMismatch);
                }
            }
        }
        for (key, value) in obj {
            if let Some(s) = properties.get(key) {
                validate(s, value)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(ProviderError::ProtocolMismatch);
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
