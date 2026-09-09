//! Pure command normalization and identity validation, independent of publication I/O.
use oracle_core::{Error, ErrorCode, Result};
use serde_json::Value;

/// Server-owned identity and empty localization maps do not change a command definition.
pub fn canonical_definition(value: &Value) -> Result<Value> {
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    for field in ["id", "application_id", "guild_id", "version"] {
        object.remove(field);
    }
    if !object.contains_key("type") {
        object.insert("type".into(), Value::from(1));
    }
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                // Discord's localized display fields are derived from the localization maps.
                map.remove("name_localized");
                map.remove("description_localized");
                for field in [
                    "min_value",
                    "max_value",
                    "min_length",
                    "max_length",
                    "handler",
                    "dm_permission",
                ] {
                    if map.get(field).is_some_and(Value::is_null) {
                        map.remove(field);
                    }
                }
                for field in ["required", "autocomplete"] {
                    if map.get(field).and_then(Value::as_bool) == Some(false) {
                        map.remove(field);
                    }
                }
                for field in [
                    "options",
                    "choices",
                    "channel_types",
                    "file_types",
                    "integration_types",
                ] {
                    if map
                        .get(field)
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                    {
                        map.remove(field);
                    }
                }
                for field in ["name_localizations", "description_localizations"] {
                    if map
                        .get(field)
                        .is_some_and(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty()))
                    {
                        map.remove(field);
                    }
                }
                for v in map.values_mut() {
                    normalize(v);
                }
            }
            Value::Array(values) => {
                for v in values {
                    normalize(v);
                }
            }
            _ => {}
        }
    }
    normalize(&mut value);
    // Discord materializes these optional defaults in its readback objects.
    let object = value.as_object_mut().unwrap();
    for field in [
        "default_member_permissions",
        "contexts",
        "integration_types",
    ] {
        if object.get(field).is_some_and(Value::is_null) {
            object.remove(field);
        }
    }
    for (field, default) in [
        ("dm_permission", true),
        ("default_permission", true),
        ("nsfw", false),
    ] {
        if object.get(field).and_then(Value::as_bool) == Some(default) {
            object.remove(field);
        }
    }
    command_key(&value)?;
    Ok(value)
}
pub(super) fn command_key(value: &Value) -> Result<String> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    let kind = match value.get("type") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?,
        None => 1,
    };
    if !(1..=3).contains(&kind)
        || name.is_empty()
        || name.chars().count() > 32
        || name.chars().any(char::is_control)
        || (kind == 1
            && name
                .chars()
                .any(|c| !(c.is_lowercase() || c.is_numeric() || c == '-' || c == '_')))
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    let key = format!("{kind}:{name}");
    if key.len() > 128 {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(key)
}
