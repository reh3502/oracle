//! Static, local installation of operator-trusted native ELF packages. Never executes code.
use oracle_core::{
    Error, ErrorCode, InstalledModule, ModuleAudience, ModuleCommandInput, ModuleCommandOptionType,
    ModuleManifest, ModuleOperation, ModulePackage, ModulePresentation, Result,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

pub const HOST_TARGET: &str = env!("ORACLE_TARGET");
const MAX_PACKAGE: u64 = 512 * 1024;
const MAX_FILE: u64 = 128 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;
fn err(code: ErrorCode) -> Error {
    Error::new(code)
}
fn io(_: std::io::Error) -> Error {
    err(ErrorCode::Io)
}
fn changed(_: std::io::Error) -> Error {
    err(ErrorCode::ArtifactChanged)
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/@".contains(&b))
}
fn unique<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let mut set = BTreeSet::new();
    values.all(|value| set.insert(value))
}
fn allowed_capability(value: &str) -> bool {
    matches!(
        value,
        "storage.own"
            | "contracts.invoke"
            | "host.echo"
            | "config.own"
            | "events.guild"
            | "discord.notify"
    )
}

/// Reject every external reference before passing a schema to the offline-only validator.
pub fn schema_validator(schema: &Value) -> Result<jsonschema::Validator> {
    fn local(value: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match value {
            Value::Object(map) => map.iter().all(|(key, value)| {
                if matches!(
                    key.as_str(),
                    "$ref" | "$dynamicRef" | "$recursiveRef" | "$id"
                ) {
                    return value.as_str().is_some_and(|s| s.starts_with('#'));
                }
                if key == "$schema" {
                    return value.as_str().is_some_and(|s| {
                        matches!(
                            s,
                            "https://json-schema.org/draft/2020-12/schema"
                                | "https://json-schema.org/draft/2019-09/schema"
                                | "http://json-schema.org/draft-07/schema#"
                                | "http://json-schema.org/draft-06/schema#"
                                | "http://json-schema.org/draft-04/schema#"
                        )
                    });
                }
                local(value, depth + 1)
            }),
            Value::Array(values) => values.iter().all(|v| local(v, depth + 1)),
            _ => true,
        }
    }
    if serde_json::to_vec(schema)
        .map_err(|_| err(ErrorCode::SchemaInvalid))?
        .len()
        > MAX_PACKAGE as usize
        || !local(schema, 0)
    {
        return Err(err(ErrorCode::SchemaInvalid));
    }
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|_| err(ErrorCode::SchemaInvalid))
}
pub fn validate_input(operation: &ModuleOperation, input: &Value) -> Result<()> {
    validate_schema(&operation.input_schema, input)
}
pub fn validate_output(operation: &ModuleOperation, output: &Value) -> Result<()> {
    validate_schema(&operation.output_schema, output)
}
fn validate_schema(schema: &Value, value: &Value) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|_| err(ErrorCode::InvalidInput))?
        .len()
        > 512 * 1024
    {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    schema_validator(schema)?
        .validate(value)
        .map_err(|_| err(ErrorCode::SchemaInvalid))
}
// Typed command schemas deliberately use a small direct subset. This lets install
// prove descriptor/schema equivalence without approximating arbitrary JSON Schema.
fn validate_typed_options(
    options: &[oracle_core::ModuleCommandOption],
    schema: &Value,
) -> Result<()> {
    fn invalid() -> Error {
        err(ErrorCode::InvalidInput)
    }
    fn keys(value: &Value, allowed: &[&str]) -> bool {
        value
            .as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str())))
    }
    if options.len() > 25
        || !unique(options.iter().map(|o| o.name.as_str()))
        || !keys(
            schema,
            &[
                "type",
                "properties",
                "required",
                "additionalProperties",
                "title",
                "description",
                "$schema",
            ],
        )
        || schema["type"] != "object"
        || schema["additionalProperties"] != false
    {
        return Err(invalid());
    }
    let properties = schema["properties"].as_object().ok_or_else(invalid)?;
    let required: Vec<&str> = match schema.get("required") {
        None => vec![],
        Some(value) => value
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|v| v.as_str().ok_or_else(invalid))
            .collect::<Result<_>>()?,
    };
    if properties.len() != options.len()
        || !unique(required.iter().copied())
        || required.iter().any(|r| !properties.contains_key(*r))
    {
        return Err(invalid());
    }
    let mut saw_optional = false;
    for option in options {
        if option.name.is_empty()
            || option.name.len() > 32
            || !option
                .name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase())
            || !option
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || option.description.is_empty()
            || option.description.chars().count() > 100
            || option.description.chars().any(char::is_control)
            || (option.required && saw_optional)
            || option.required != required.contains(&option.name.as_str())
        {
            return Err(invalid());
        }
        saw_optional |= !option.required;
        let property = properties.get(&option.name).ok_or_else(invalid)?;
        let compatible = match &option.value_type {
            ModuleCommandOptionType::String {
                min_length,
                max_length,
                choices,
            } => {
                min_length <= max_length
                    && *max_length <= 6000
                    && *max_length > 0
                    && choices.len() <= 25
                    && unique(choices.iter().map(String::as_str))
                    && choices.iter().all(|c| {
                        !c.is_empty()
                            && c.chars().count() <= 100
                            && c.chars().count() >= *min_length as usize
                            && c.chars().count() <= *max_length as usize
                            && !c.chars().any(char::is_control)
                    })
                    && keys(
                        property,
                        &[
                            "type",
                            "minLength",
                            "maxLength",
                            "enum",
                            "title",
                            "description",
                        ],
                    )
                    && property["type"] == "string"
                    && property
                        .get("minLength")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        == u64::from(*min_length)
                    && property["maxLength"].as_u64() == Some(u64::from(*max_length))
                    && if choices.is_empty() {
                        property.get("enum").is_none()
                    } else {
                        property.get("enum") == Some(&serde_json::json!(choices))
                    }
            }
            ModuleCommandOptionType::Integer {
                min_value,
                max_value,
            } => {
                const SAFE: i64 = 9_007_199_254_740_991;
                *min_value >= -SAFE
                    && *max_value <= SAFE
                    && min_value <= max_value
                    && keys(
                        property,
                        &["type", "minimum", "maximum", "title", "description"],
                    )
                    && property["type"] == "integer"
                    && property["minimum"].as_i64() == Some(*min_value)
                    && property["maximum"].as_i64() == Some(*max_value)
            }
            ModuleCommandOptionType::Boolean => {
                keys(property, &["type", "title", "description"]) && property["type"] == "boolean"
            }
        };
        if !compatible {
            return Err(invalid());
        }
    }
    Ok(())
}
fn validate_presentation(pointer: &str, schema: &Value) -> Result<()> {
    // Presentation remains host-owned; only this fixed, schema-declared field opts in.
    // Exclude schema forms (notably draft-7 $ref siblings and prefixItems) that
    // could make the visible descriptor weaker than the effective output schema.
    fn keys(value: &Value, allowed: &[&str]) -> bool {
        value
            .as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str())))
    }
    if !keys(
        schema,
        &[
            "type",
            "properties",
            "required",
            "additionalProperties",
            "title",
            "description",
            "$schema",
        ],
    ) || pointer != "/reply"
        || schema["type"] != "object"
        || !schema["required"]
            .as_array()
            .is_some_and(|r| r.contains(&serde_json::json!("reply")))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    let reply = &schema["properties"]["reply"];
    if !keys(
        reply,
        &[
            "type",
            "properties",
            "required",
            "additionalProperties",
            "title",
            "description",
        ],
    ) || reply["type"] != "object"
        || reply["additionalProperties"] != false
        || !reply["properties"]
            .as_object()
            .is_some_and(|p| p.len() == 2 && p.contains_key("text") && p.contains_key("citations"))
        || !reply["required"].as_array().is_some_and(|r| {
            r.len() == 2
                && r.contains(&serde_json::json!("text"))
                && r.contains(&serde_json::json!("citations"))
        })
        || reply["properties"]["text"]["type"] != "string"
        || reply["properties"]["citations"]["type"] != "array"
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    let text = &reply["properties"]["text"];
    let citations = &reply["properties"]["citations"];
    let item = &citations["items"];
    let label = &item["properties"]["label"];
    let revision = &item["properties"]["revision"];
    if !keys(
        text,
        &["type", "minLength", "maxLength", "title", "description"],
    ) || !keys(
        citations,
        &[
            "type",
            "minItems",
            "maxItems",
            "items",
            "title",
            "description",
        ],
    ) || !keys(
        item,
        &[
            "type",
            "properties",
            "required",
            "additionalProperties",
            "title",
            "description",
        ],
    ) || !keys(
        label,
        &["type", "minLength", "maxLength", "title", "description"],
    ) || !keys(
        revision,
        &["type", "minimum", "maximum", "title", "description"],
    ) || !text["minLength"].as_u64().is_some_and(|n| n >= 1)
        || !label["minLength"].as_u64().is_some_and(|n| n >= 1)
        || !text["maxLength"]
            .as_u64()
            .is_some_and(|n| (1..=1800).contains(&n))
        || citations["maxItems"].as_u64() != Some(5)
        || item["type"] != "object"
        || item["additionalProperties"] != false
        || !item["required"].as_array().is_some_and(|r| {
            r.len() == 2
                && r.contains(&serde_json::json!("label"))
                && r.contains(&serde_json::json!("revision"))
        })
        || !item["properties"]
            .as_object()
            .is_some_and(|p| p.len() == 2 && p.contains_key("label") && p.contains_key("revision"))
        || label["type"] != "string"
        || !label["maxLength"]
            .as_u64()
            .is_some_and(|n| (1..=120).contains(&n))
        || revision["type"] != "integer"
        || !revision["minimum"].as_u64().is_some_and(|n| n >= 1)
        || !revision["maximum"]
            .as_u64()
            .is_some_and(|n| n <= 9_007_199_254_740_991)
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    Ok(())
}
fn validate_card_presentation(pointer: &str, schema: &Value) -> Result<()> {
    // Runtime rendering applies the exact bounded host shape independently of
    // the module's output schema. Require an explicit, closed reply descriptor.
    let reply = &schema["properties"]["reply"];
    let names = ["text", "card", "citations", "buttons", "choices"];
    if pointer != "/reply"
        || schema["type"] != "object"
        || !schema["required"]
            .as_array()
            .is_some_and(|r| r.contains(&serde_json::json!("reply")))
        || reply["type"] != "object"
        || reply["additionalProperties"] != false
        || !reply["properties"].as_object().is_some_and(|p| {
            (p.len() == names.len() || (p.len() == names.len() + 1 && p.contains_key("image")))
                && names.iter().all(|n| p.contains_key(*n))
        })
        || !reply["required"].as_array().is_some_and(|r| {
            r.len() == names.len() && names.iter().all(|n| r.contains(&serde_json::json!(n)))
        })
        || reply["properties"]["text"]["type"] != "string"
        || reply["properties"]["card"]["type"] != "object"
        || ["citations", "buttons", "choices"]
            .iter()
            .any(|n| reply["properties"][n]["type"] != "array")
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    if let Some(image) = reply["properties"].get("image")
        && (image["type"] != "object"
            || image["additionalProperties"] != false
            || !image["properties"].as_object().is_some_and(|p| {
                p.len() == 2 && p.contains_key("url") && p.contains_key("revision")
            })
            || !image["required"].as_array().is_some_and(|r| {
                r.len() == 2
                    && r.contains(&serde_json::json!("url"))
                    && r.contains(&serde_json::json!("revision"))
            })
            || image["properties"]["url"]["type"] != "string"
            || image["properties"]["revision"]["type"] != "integer")
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    Ok(())
}
pub fn validate_manifest(manifest: &ModuleManifest) -> Result<()> {
    if !matches!(
        (manifest.manifest_version, manifest.protocol_minor_min),
        (1, 0) | (2, 1) | (3, 2)
    ) || manifest.protocol_major != 1
        || manifest.target != HOST_TARGET
    {
        return Err(err(ErrorCode::Compatibility));
    }
    semver::Version::parse(&manifest.version).map_err(|_| err(ErrorCode::Compatibility))?;
    let host =
        semver::VersionReq::parse(&manifest.host_api).map_err(|_| err(ErrorCode::Compatibility))?;
    if !host.matches(&semver::Version::new(
        1,
        match manifest.manifest_version {
            3 => 4,
            2 => 3,
            _ => 0,
        },
        0,
    )) || (manifest.manifest_version == 2
        && (host.matches(&semver::Version::new(1, 0, 0))
            || host.matches(&semver::Version::new(1, 0, u64::MAX))))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.manifest_version == 3
        && (0..4).any(|minor| {
            host.matches(&semver::Version::new(1, minor, 0))
                || host.matches(&semver::Version::new(1, minor, u64::MAX))
        })
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if (manifest.manifest_version < 3
        && (!manifest.member_permissions.is_empty()
            || manifest.operations.iter().any(|o| {
                o.audience == ModuleAudience::MemberMutation
                    || !o.callback_methods.is_empty()
                    || !o.callback_collections.is_empty()
            })))
        || manifest.member_permissions.len() > 16
        || !unique(manifest.member_permissions.iter().map(String::as_str))
        || manifest.member_permissions.iter().any(|p| !name(p))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    let uses_cards = manifest.commands.as_ref().is_some_and(|commands| {
        commands
            .routes
            .iter()
            .any(|route| matches!(route.presentation, Some(ModulePresentation::CardV1 { .. })))
    });
    if uses_cards
        && (host.matches(&semver::Version::new(1, 1, 0))
            || host.matches(&semver::Version::new(1, 1, u64::MAX)))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    let uses_images = manifest.commands.as_ref().is_some_and(|commands| {
        commands.routes.iter().any(|route| {
            matches!(route.presentation, Some(ModulePresentation::CardV1 { .. }))
                && manifest.operations.iter().any(|op| {
                    op.name == route.operation
                        && op.output_schema["properties"]["reply"]["properties"]
                            .get("image")
                            .is_some()
                })
        })
    });
    if uses_images
        && (host.matches(&semver::Version::new(1, 2, 0))
            || host.matches(&semver::Version::new(1, 2, u64::MAX)))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.manifest_version == 1
        && (manifest.runtime.is_some()
            || manifest
                .operations
                .iter()
                .any(|o| o.audience != ModuleAudience::Operator)
            || manifest.commands.as_ref().is_some_and(|c| {
                c.routes
                    .iter()
                    .any(|r| r.input.is_some() || r.presentation.is_some())
            }))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.capabilities.len() > 16
        || !unique(manifest.capabilities.iter().map(String::as_str))
        || manifest.capabilities.iter().any(|c| !allowed_capability(c))
        || manifest
            .required_intents
            .iter()
            .any(|v| !matches!(v.as_str(), "guilds" | "guild_members" | "guild_moderation"))
        || !unique(manifest.required_intents.iter().map(String::as_str))
    {
        return Err(err(ErrorCode::Compatibility));
    }
    if manifest.data_version > 1024
        || manifest.readable_data_versions.len() > 1025
        || !manifest
            .readable_data_versions
            .contains(&manifest.data_version)
        || manifest
            .readable_data_versions
            .iter()
            .any(|v| *v > manifest.data_version)
        || manifest
            .readable_data_versions
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != manifest.readable_data_versions.len()
    {
        return Err(err(ErrorCode::DataVersionMismatch));
    }
    if manifest.operations.len() > 64
        || manifest.collections.len() > 32
        || manifest.provides.len() > 64
        || manifest.consumes.len() > 64
        || manifest.migrations.len() > 1024
    {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    if !unique(manifest.operations.iter().map(|v| v.name.as_str()))
        || !unique(manifest.collections.iter().map(|v| v.name.as_str()))
        || !unique(manifest.provides.iter().map(|v| v.name.as_str()))
        || !unique(manifest.consumes.iter().map(|v| v.name.as_str()))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    if let Some(configuration) = &manifest.configuration {
        if configuration.schema_version == 0
            || configuration.presets.len() > 32
            || !manifest.capabilities.iter().any(|c| c == "config.own")
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        schema_validator(&configuration.schema)?;
        for (preset, values) in &configuration.presets {
            if !name(preset)
                || !values.is_object()
                || serde_json::to_vec(values)
                    .map_err(|_| err(ErrorCode::InvalidInput))?
                    .len()
                    > 65536
            {
                return Err(err(ErrorCode::InvalidInput));
            }
        }
    }
    if !manifest.subscriptions.is_empty()
        && (manifest.subscriptions.len() > 16
            || manifest.subscriptions.iter().collect::<BTreeSet<_>>().len()
                != manifest.subscriptions.len()
            || manifest
                .subscriptions
                .contains(&oracle_core::GuildEventKind::Unknown)
            || !manifest.capabilities.iter().any(|c| c == "events.guild")
            || manifest.configuration.is_none())
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    for subscription in &manifest.subscriptions {
        use oracle_core::GuildEventKind::*;
        let required = match subscription {
            ModerationAudit | Ban | Unban => Some("guild_moderation"),
            MemberRolesChanged | MemberJoined | MemberLeft => Some("guild_members"),
            ChannelChanged | RoleAccessChanged => Some("guilds"),
            Maintenance | Unknown => None,
        };
        if required.is_some_and(|intent| !manifest.required_intents.iter().any(|v| v == intent)) {
            return Err(err(ErrorCode::Compatibility));
        }
    }
    if manifest.capabilities.iter().any(|c| c == "discord.notify")
        && manifest.configuration.is_none()
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    if let Some(commands) = &manifest.commands {
        fn slug(value: &str, max: usize) -> bool {
            !value.is_empty()
                && value.len() <= max
                && value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }
        fn description(value: &str) -> bool {
            !value.is_empty()
                && value.chars().count() <= 100
                && !value.chars().any(char::is_control)
        }
        if !slug(&commands.namespace, 20)
            || commands.namespace == "oracle"
            || !description(&commands.description)
            || commands.routes.is_empty()
            || commands.routes.len() > 25
            || !unique(commands.routes.iter().map(|r| r.name.as_str()))
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        for route in &commands.routes {
            let operation = manifest
                .operations
                .iter()
                .find(|op| op.name == route.operation)
                .ok_or_else(|| err(ErrorCode::InvalidInput))?;
            if !slug(&route.name, 32)
                || !description(&route.description)
                || (route.input.is_none()
                    && !route.input_required
                    && !schema_validator(&operation.input_schema)?.is_valid(&serde_json::json!({})))
            {
                return Err(err(ErrorCode::InvalidInput));
            }
            if manifest.manifest_version >= 2 {
                let input = route
                    .input
                    .as_ref()
                    .ok_or_else(|| err(ErrorCode::InvalidInput))?;
                if route.input_required {
                    return Err(err(ErrorCode::InvalidInput));
                }
                match input {
                    ModuleCommandInput::Json { required } => {
                        if !required
                            && !schema_validator(&operation.input_schema)?
                                .is_valid(&serde_json::json!({}))
                        {
                            return Err(err(ErrorCode::InvalidInput));
                        }
                    }
                    ModuleCommandInput::Typed { options } => {
                        validate_typed_options(options, &operation.input_schema)?
                    }
                }
                match &route.presentation {
                    Some(ModulePresentation::PlainTextV1 { pointer }) => {
                        validate_presentation(pointer, &operation.output_schema)?
                    }
                    Some(ModulePresentation::CardV1 { pointer }) => {
                        validate_card_presentation(pointer, &operation.output_schema)?
                    }
                    None => {}
                }
            }
        }
    }
    for operation in &manifest.operations {
        if !name(&operation.name)
            || operation.description.len() > 2048
            || operation.timeout_ms == 0
            || operation.timeout_ms > 300_000
            || operation
                .capabilities
                .iter()
                .any(|c| !manifest.capabilities.contains(c))
            || !unique(operation.capabilities.iter().map(String::as_str))
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        if operation.audience != ModuleAudience::Operator
            && ((operation.audience == ModuleAudience::MemberRead
                && !operation.capabilities.is_empty())
                || operation.ai.is_some()
                || manifest
                    .migrations
                    .iter()
                    .any(|m| m.operation == operation.name)
                || manifest
                    .provides
                    .iter()
                    .any(|p| p.operation == operation.name)
                || [
                    "configuration.",
                    "config.",
                    "migration.",
                    "event.",
                    "guild.",
                    "lifecycle.",
                    "host.",
                ]
                .iter()
                .any(|prefix| operation.name.starts_with(prefix))
                || matches!(
                    operation.name.as_str(),
                    "initialize" | "shutdown" | "activate" | "deactivate"
                ))
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        if operation.audience != ModuleAudience::MemberMutation
            && (manifest.manifest_version != 3 || operation.name != "maintenance")
        {
            if !operation.callback_methods.is_empty() || !operation.callback_collections.is_empty()
            {
                return Err(err(ErrorCode::InvalidInput));
            }
        } else if operation.ai.is_some()
            || operation.callback_methods.len() > 2
            || !unique(operation.callback_methods.iter().map(String::as_str))
            || !unique(operation.callback_collections.iter().map(String::as_str))
            || operation
                .callback_collections
                .iter()
                .any(|c| !manifest.collections.iter().any(|v| &v.name == c))
            || operation.capabilities.iter().any(|c| c != "storage.own")
            || operation
                .callback_methods
                .iter()
                .any(|m| !matches!(m.as_str(), "host.document_get" | "host.document_batch"))
            || (!operation.callback_methods.is_empty()
                && (!operation.capabilities.iter().any(|c| c == "storage.own")
                    || operation.callback_collections.is_empty()))
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        if let Some(ai) = &operation.ai {
            use oracle_core::ModuleAiOperationKind;
            if operation
                .capabilities
                .iter()
                .any(|c| c == "contracts.invoke" || c == "host.echo")
                || (ai.kind == ModuleAiOperationKind::Inspection
                    && operation.capabilities.iter().any(|c| c == "discord.notify"))
                || (ai.kind == ModuleAiOperationKind::Verification
                    && (!operation.capabilities.iter().any(|c| c == "discord.notify")
                        || ai.success_pointer.is_none()))
                || (ai.kind == ModuleAiOperationKind::Inspection
                    && ai.success_pointer.is_some()
                    && !schema_validator(&operation.input_schema)?.is_valid(&serde_json::json!({})))
                || ai.success_pointer.as_ref().is_some_and(|pointer| {
                    !pointer.starts_with('/')
                        || pointer.len() > 256
                        || pointer.chars().any(char::is_control)
                })
            {
                return Err(err(ErrorCode::InvalidInput));
            }
        }
        schema_validator(&operation.input_schema)?;
        schema_validator(&operation.output_schema)?;
    }
    for collection in &manifest.collections {
        if !name(&collection.name) {
            return Err(err(ErrorCode::InvalidInput));
        }
        schema_validator(&collection.schema)?;
    }
    for provided in &manifest.provides {
        if !name(&provided.name)
            || !manifest
                .operations
                .iter()
                .any(|v| v.name == provided.operation)
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        semver::Version::parse(&provided.version).map_err(|_| err(ErrorCode::Compatibility))?;
    }
    for consumed in &manifest.consumes {
        if !name(&consumed.name) {
            return Err(err(ErrorCode::InvalidInput));
        }
        semver::VersionReq::parse(&consumed.version).map_err(|_| err(ErrorCode::Compatibility))?;
    }
    let mut from = BTreeSet::new();
    for migration in &manifest.migrations {
        if migration.from.checked_add(1) != Some(migration.to)
            || migration.to > manifest.data_version
            || !from.insert(migration.from)
            || !manifest
                .operations
                .iter()
                .any(|v| v.name == migration.operation)
        {
            return Err(err(ErrorCode::DataVersionMismatch));
        }
    }
    Ok(())
}
fn relative(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.components().count() > 16
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    Ok(path.to_owned())
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical(package: &ModulePackage) -> Result<Vec<u8>> {
    serde_json::to_vec(package).map_err(|_| err(ErrorCode::InvalidInput))
}
fn package_digest(package: &ModulePackage) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(canonical(package)?)))
}
fn validate_package(package: &ModulePackage) -> Result<()> {
    if canonical(package)?.len() > MAX_PACKAGE as usize {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    validate_manifest(&package.manifest)?;
    if package.files.is_empty() || package.files.len() > 64 {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    relative(&package.entrypoint)?;
    if !package.files.contains_key(&package.entrypoint) {
        return Err(err(ErrorCode::InvalidInput));
    }
    for (path, digest) in &package.files {
        relative(path)?;
        if path == "package.json" || !digest_valid(digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
    }
    for value in [
        &package.source_revision,
        &package.toolchain,
        &package.license,
    ] {
        if value.is_empty() || value.len() > 1024 {
            return Err(err(ErrorCode::InvalidInput));
        }
    }
    Ok(())
}
fn regular(path: &Path, limit: u64) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path).map_err(changed)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    if meta.len() > limit {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    Ok(meta)
}
fn read_package(root: &Path) -> Result<ModulePackage> {
    regular(&root.join("package.json"), MAX_PACKAGE)?;
    let mut bytes = Vec::new();
    fs::File::open(root.join("package.json"))
        .map_err(changed)?
        .take(MAX_PACKAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(changed)?;
    if bytes.len() > MAX_PACKAGE as usize {
        return Err(err(ErrorCode::QuotaExceeded));
    }
    serde_json::from_slice(&bytes).map_err(|_| err(ErrorCode::InvalidInput))
}
fn inventory(root: &Path, package: &ModulePackage, readonly: bool) -> Result<()> {
    let mut pending = vec![(root.to_owned(), PathBuf::new())];
    let mut found = BTreeSet::new();
    let mut total = 0u64;
    let mut count = 0;
    while let Some((directory, relative_dir)) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory).map_err(changed)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || (readonly && metadata.mode() & 0o222 != 0)
        {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        for entry in fs::read_dir(&directory).map_err(changed)? {
            count += 1;
            if count > 256 {
                return Err(err(ErrorCode::QuotaExceeded));
            }
            let entry = entry.map_err(changed)?;
            let rel = relative_dir.join(entry.file_name());
            let text = rel.to_str().ok_or_else(|| err(ErrorCode::InvalidInput))?;
            relative(text)?;
            let meta = fs::symlink_metadata(entry.path()).map_err(changed)?;
            if meta.file_type().is_symlink() {
                return Err(err(ErrorCode::ArtifactChanged));
            }
            if meta.is_dir() {
                if !package
                    .files
                    .keys()
                    .any(|name| name.starts_with(&format!("{text}/")))
                {
                    return Err(err(ErrorCode::ArtifactChanged));
                }
                pending.push((entry.path(), rel));
                continue;
            }
            let limit = if text == "package.json" {
                MAX_PACKAGE
            } else {
                MAX_FILE
            };
            regular(&entry.path(), limit)?;
            total = total
                .checked_add(meta.len())
                .ok_or_else(|| err(ErrorCode::QuotaExceeded))?;
            if total > MAX_TOTAL {
                return Err(err(ErrorCode::QuotaExceeded));
            }
            if readonly
                && (meta.mode() & 0o7777
                    != if text == package.entrypoint {
                        0o555
                    } else {
                        0o444
                    })
            {
                return Err(err(ErrorCode::ArtifactChanged));
            }
            if text != "package.json" {
                if !package.files.contains_key(text) {
                    return Err(err(ErrorCode::ArtifactChanged));
                }
                found.insert(text.to_owned());
            }
        }
    }
    if found.len() != package.files.len() {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    Ok(())
}
fn native_elf(header: &[u8]) -> bool {
    let machine = if HOST_TARGET.starts_with("x86_64-") {
        62u16
    } else if HOST_TARGET.starts_with("aarch64-") {
        183u16
    } else {
        return false;
    };
    header.len() >= 64
        && &header[..4] == b"\x7fELF"
        && header[4] == 2
        && header[5] == 1
        && header[6] == 1
        && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
        && u16::from_le_bytes([header[18], header[19]]) == machine
        && u32::from_le_bytes([header[20], header[21], header[22], header[23]]) == 1
}
fn checked_copy(source: &Path, target: Option<&Path>, expected: &str, entry: bool) -> Result<()> {
    regular(source, MAX_FILE)?;
    let mut source = fs::File::open(source).map_err(changed)?;
    let mut target = target
        .map(|p| fs::OpenOptions::new().write(true).create_new(true).open(p))
        .transpose()
        .map_err(io)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut prefix = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = source.read(&mut buffer).map_err(changed)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_FILE {
            return Err(err(ErrorCode::QuotaExceeded));
        }
        if prefix.len() < 64 {
            prefix.extend_from_slice(&buffer[..n.min(64 - prefix.len())]);
        }
        hash.update(&buffer[..n]);
        if let Some(target) = &mut target {
            target.write_all(&buffer[..n]).map_err(io)?;
        }
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(err(ErrorCode::ArtifactChanged));
    }
    if entry && !native_elf(&prefix) {
        return Err(err(ErrorCode::Compatibility));
    }
    if let Some(target) = target {
        target.sync_all().map_err(io)?;
    }
    Ok(())
}
fn permissions(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io)
}
fn remove_tree(path: &Path) -> Result<()> {
    // Never follow links during cleanup, including a tampered staging directory.
    let meta = fs::symlink_metadata(path).map_err(io)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return fs::remove_file(path).map_err(io);
    }
    permissions(path, 0o700)?;
    for entry in fs::read_dir(path).map_err(io)? {
        remove_tree(&entry.map_err(io)?.path())?;
    }
    fs::remove_dir(path).map_err(io)
}
struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = remove_tree(&self.0);
        }
    }
}
pub struct ArtifactStore {
    root: PathBuf,
}
impl ArtifactStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).map_err(io)?;
        if fs::symlink_metadata(&root)
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        Ok(Self {
            root: fs::canonicalize(root).map_err(io)?,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn install(&self, source_dir: &Path, trusted: bool) -> Result<InstalledModule> {
        if !trusted {
            return Err(err(ErrorCode::TrustedCodeRequired));
        }
        let package = read_package(source_dir)?;
        validate_package(&package)?;
        inventory(source_dir, &package, false)?;
        let installed = InstalledModule {
            digest: package_digest(&package)?,
            package,
        };
        let destination = self.root.join(&installed.digest);
        if fs::symlink_metadata(&destination).is_ok() {
            self.verify(&installed)?;
            return Ok(installed);
        }
        let stage = Stage(self.root.join(format!(".install-{}", uuid::Uuid::new_v4())));
        fs::create_dir(&stage.0).map_err(io)?;
        let mut directories = BTreeSet::from([stage.0.clone()]);
        for (name, digest) in &installed.package.files {
            let target = stage.0.join(name);
            let parent = target
                .parent()
                .ok_or_else(|| err(ErrorCode::InvalidInput))?;
            fs::create_dir_all(parent).map_err(io)?;
            let mut dir = parent;
            while dir.starts_with(&stage.0) {
                directories.insert(dir.to_owned());
                let Some(parent) = dir.parent() else { break };
                dir = parent;
            }
            checked_copy(
                &source_dir.join(name),
                Some(&target),
                digest,
                name == &installed.package.entrypoint,
            )?;
            permissions(
                &target,
                if name == &installed.package.entrypoint {
                    0o555
                } else {
                    0o444
                },
            )?;
        }
        let metadata = stage.0.join("package.json");
        fs::write(&metadata, canonical(&installed.package)?).map_err(io)?;
        fs::File::open(&metadata)
            .map_err(io)?
            .sync_all()
            .map_err(io)?;
        permissions(&metadata, 0o444)?;
        for directory in directories.iter().rev() {
            permissions(directory, 0o555)?;
        }
        inventory(&stage.0, &installed.package, true)?;
        match fs::rename(&stage.0, &destination) {
            Ok(()) => {}
            Err(_) if destination.exists() => {
                self.verify(&installed)?;
            }
            Err(error) => return Err(io(error)),
        }
        fs::File::open(&self.root)
            .map_err(io)?
            .sync_all()
            .map_err(io)?;
        self.verify(&installed)?;
        Ok(installed)
    }
    pub fn verify(&self, installed: &InstalledModule) -> Result<PathBuf> {
        validate_package(&installed.package)?;
        if !digest_valid(&installed.digest)
            || package_digest(&installed.package)? != installed.digest
        {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        let root = self.root.join(&installed.digest);
        inventory(&root, &installed.package, true)?;
        if read_package(&root)? != installed.package {
            return Err(err(ErrorCode::ArtifactChanged));
        }
        for (name, digest) in &installed.package.files {
            checked_copy(
                &root.join(name),
                None,
                digest,
                name == &installed.package.entrypoint,
            )?;
        }
        Ok(root.join(&installed.package.entrypoint))
    }
    pub fn remove(&self, installed: &InstalledModule) -> Result<()> {
        self.verify(installed)?;
        let removed = self.root.join(format!(".remove-{}", uuid::Uuid::new_v4()));
        fs::rename(self.root.join(&installed.digest), &removed).map_err(io)?;
        remove_tree(&removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("oracle-package-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = remove_tree(&self.0);
        }
    }
    fn fixture() -> (Temp, ModulePackage) {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("source")).unwrap();
        let bytes = fs::read("/bin/true").unwrap();
        fs::write(temp.0.join("source/module"), &bytes).unwrap();
        let package:ModulePackage=serde_json::from_value(json!({
            "manifest":{"manifest_version":1,"id":"test.module","version":"1.0.0","target":HOST_TARGET,
                "protocol_major":1,"protocol_minor_min":0,"host_api":"^1.0.0","data_version":1,
                "readable_data_versions":[1],"capabilities":["storage.own"],"required_intents":["guilds"],
                "operations":[{"name":"echo@1","description":"Echo","input_schema":{"type":"object","properties":{"id":{"type":"string"}},"additionalProperties":false},"output_schema":{"type":"object"},"timeout_ms":1000}]},
            "entrypoint":"module","files":{"module":format!("{:x}",Sha256::digest(&bytes))},
            "source_revision":"fixture","toolchain":"system fixture","license":"fixture"
        })).unwrap();
        write(&temp, &package);
        (temp, package)
    }
    fn write(temp: &Temp, package: &ModulePackage) {
        fs::write(
            temp.0.join("source/package.json"),
            serde_json::to_vec(package).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn static_install_reuses_digest_verifies_permissions_and_removes() {
        let (temp, _) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        let first = store.install(&temp.0.join("source"), true).unwrap();
        let entry = store.verify(&first).unwrap();
        assert_eq!(fs::metadata(&entry).unwrap().mode() & 0o7777, 0o555);
        assert_eq!(store.install(&temp.0.join("source"), true).unwrap(), first);
        store.remove(&first).unwrap();
        assert!(!entry.exists());
    }
    #[test]
    fn explicit_trust_and_compatibility_are_required() {
        let (temp, mut package) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        assert_eq!(
            store
                .install(&temp.0.join("source"), false)
                .unwrap_err()
                .code,
            ErrorCode::TrustedCodeRequired
        );
        package.manifest.protocol_major = 2;
        write(&temp, &package);
        assert_eq!(
            store
                .install(&temp.0.join("source"), true)
                .unwrap_err()
                .code,
            ErrorCode::Compatibility
        );
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }
    #[test]
    fn inventory_tampering_traversal_links_and_unlisted_files_are_rejected() {
        let (temp, mut package) = fixture();
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        let source = temp.0.join("source");
        package.entrypoint = "../escape".into();
        write(&temp, &package);
        assert!(store.install(&source, true).is_err());
        package.entrypoint = "module".into();
        write(&temp, &package);
        fs::write(source.join("extra"), b"extra").unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("extra")).unwrap();
        fs::rename(source.join("module"), temp.0.join("real-module")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("real-module"), source.join("module")).unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("module")).unwrap();
        fs::hard_link(temp.0.join("real-module"), source.join("module")).unwrap();
        assert!(store.install(&source, true).is_err());
        fs::remove_file(source.join("module")).unwrap();
        fs::rename(temp.0.join("real-module"), source.join("module")).unwrap();
        let installed = store.install(&source, true).unwrap();
        let entry = store.verify(&installed).unwrap();
        permissions(&entry, 0o755).unwrap();
        fs::write(&entry, b"tamper").unwrap();
        permissions(&entry, 0o555).unwrap();
        assert_eq!(
            store.verify(&installed).unwrap_err().code,
            ErrorCode::ArtifactChanged
        );
    }
    #[test]
    fn event_descriptors_require_config_grants_and_matching_intents() {
        let manifest: ModuleManifest = serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest-events.json"
        ))
        .unwrap();
        validate_manifest(&manifest).unwrap();
        let mut missing_intent = manifest.clone();
        missing_intent
            .required_intents
            .retain(|v| v != "guild_members");
        assert_eq!(
            validate_manifest(&missing_intent).unwrap_err().code,
            ErrorCode::Compatibility
        );
        let mut unknown = manifest.clone();
        unknown
            .subscriptions
            .push(oracle_core::GuildEventKind::Unknown);
        assert!(validate_manifest(&unknown).is_err());
        let mut grant = manifest.clone();
        grant.capabilities.retain(|v| v != "events.guild");
        assert!(validate_manifest(&grant).is_err());
        let mut config = manifest;
        config.configuration = None;
        assert!(validate_manifest(&config).is_err());
    }
    #[test]
    fn command_descriptors_validate_names_routes_and_input_contracts() {
        let manifest: ModuleManifest = serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest-events.json"
        ))
        .unwrap();
        validate_manifest(&manifest).unwrap();
        let mut bad = manifest.clone();
        bad.commands.as_mut().unwrap().namespace = "oracle".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        let commands = bad.commands.as_mut().unwrap();
        commands.routes.push(commands.routes[0].clone());
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        bad.commands.as_mut().unwrap().routes[0].operation = "missing".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        let operation = bad.commands.as_ref().unwrap().routes[0].operation.clone();
        bad.operations
            .iter_mut()
            .find(|o| o.name == operation)
            .unwrap()
            .input_schema = json!({"type":"object", "required":["value"]});
        assert!(validate_manifest(&bad).is_err());
        bad.commands.as_mut().unwrap().routes[0].input_required = true;
        validate_manifest(&bad).unwrap();
        let mut legacy = manifest;
        legacy.commands = None;
        assert!(
            !serde_json::to_value(&legacy)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("commands")
        );
    }
    #[test]
    fn scripts_are_rejected_without_running_install_hooks() {
        let (temp, mut package) = fixture();
        let marker = temp.0.join("executed");
        let script = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
        fs::write(temp.0.join("source/module"), &script).unwrap();
        permissions(&temp.0.join("source/module"), 0o755).unwrap();
        package.files.insert(
            "module".into(),
            format!("{:x}", Sha256::digest(script.as_bytes())),
        );
        write(&temp, &package);
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        assert_eq!(
            store
                .install(&temp.0.join("source"), true)
                .unwrap_err()
                .code,
            ErrorCode::Compatibility
        );
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }
    #[test]
    fn ai_projection_cannot_disguise_notification_or_contract_capabilities() {
        use oracle_core::{ModuleAiOperation, ModuleAiOperationKind};
        let (_, mut package) = fixture();
        let manifest = &mut package.manifest;
        manifest.configuration = Some(oracle_core::ModuleConfiguration {
            schema_version: 1,
            schema: json!({"type":"object"}),
            presets: Default::default(),
        });
        manifest.capabilities.extend([
            "config.own".into(),
            "discord.notify".into(),
            "contracts.invoke".into(),
        ]);
        manifest.operations[0].ai = Some(ModuleAiOperation {
            kind: ModuleAiOperationKind::Inspection,
            success_pointer: None,
        });
        validate_manifest(manifest).unwrap();
        manifest.operations[0].ai.as_mut().unwrap().success_pointer = Some("/ready".into());
        manifest.operations[0].input_schema["required"] = json!(["id"]);
        assert!(validate_manifest(manifest).is_err());
        manifest.operations[0]
            .input_schema
            .as_object_mut()
            .unwrap()
            .remove("required");
        manifest.operations[0].ai.as_mut().unwrap().success_pointer = None;
        manifest.operations[0].capabilities = vec!["discord.notify".into()];
        assert!(validate_manifest(manifest).is_err());
        manifest.operations[0].ai.as_mut().unwrap().kind = ModuleAiOperationKind::Verification;
        assert!(validate_manifest(manifest).is_err());
        manifest.operations[0].ai.as_mut().unwrap().success_pointer = Some("/verified".into());
        validate_manifest(manifest).unwrap();
        manifest.operations[0]
            .capabilities
            .push("contracts.invoke".into());
        assert!(validate_manifest(manifest).is_err());
        manifest.operations[0].capabilities.clear();
        assert!(validate_manifest(manifest).is_err());
    }
    #[test]
    fn schemas_are_offline_validated_and_enforced() {
        assert!(schema_validator(&json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(schema_validator(&json!({"$ref":"file:///etc/passwd"})).is_err());
        assert!(schema_validator(&json!({"type":"nonsense"})).is_err());
        let local = json!({"$defs":{"x":{"type":"integer"}},"$ref":"#/$defs/x"});
        assert!(schema_validator(&local).unwrap().is_valid(&json!(1)));
        assert!(!schema_validator(&local).unwrap().is_valid(&json!("1")));
        let (_temp, package) = fixture();
        let operation = &package.manifest.operations[0];
        assert!(validate_input(operation, &json!({"id":"123"})).is_ok());
        assert!(validate_input(operation, &json!({"id":123})).is_err());
    }
    fn member_manifest() -> ModuleManifest {
        let output_schema = json!({"type":"object","required":["reply"],"properties":{"reply":{
                    "type":"object","additionalProperties":false,"required":["text","citations"],"properties":{
                        "text":{"type":"string","minLength":1,"maxLength":1800},
                        "citations":{"type":"array","maxItems":5,"items":{"type":"object","additionalProperties":false,
                            "required":["label","revision"],"properties":{"label":{"type":"string","minLength":1,"maxLength":120},
                            "revision":{"type":"integer","minimum":1,"maximum":9007199254740991_u64}}}}}}}});
        serde_json::from_value(json!({
            "manifest_version":2,"id":"game.test","version":"1.0.0","target":HOST_TARGET,
            "protocol_major":1,"protocol_minor_min":1,"host_api":"^1.1.0","data_version":1,
            "readable_data_versions":[1],"runtime":{"data_directory_required":true},
            "operations":[{"name":"lookup@1","description":"Lookup","audience":"member_read",
                "input_schema":{"type":"object","additionalProperties":false,"required":["name"],"properties":{
                    "name":{"type":"string","minLength":1,"maxLength":100},
                    "limit":{"type":"integer","minimum":1,"maximum":10},
                    "details":{"type":"boolean"}}},
                "output_schema":output_schema,
                "timeout_ms":1000}],
            "commands":{"namespace":"dw","description":"Wiki queries","routes":[{"name":"lookup","description":"Lookup an entity","operation":"lookup@1",
                "input":{"kind":"typed","options":[
                    {"name":"name","description":"Entity name","required":true,"type":"string","min_length":1,"max_length":100},
                    {"name":"limit","description":"Result limit","required":false,"type":"integer","min_value":1,"max_value":10},
                    {"name":"details","description":"Include details","required":false,"type":"boolean"}]},
                "presentation":{"kind":"plain_text_v1","pointer":"/reply"}}]}
        })).unwrap()
    }
    #[test]
    fn member_mutation_requires_v3_and_scoped_declared_callbacks() {
        let mut m = member_manifest();
        m.manifest_version = 3;
        m.protocol_minor_min = 2;
        m.host_api = "^1.4".into();
        m.commands = None;
        m.member_permissions = vec!["manage_all_runs".into()];
        m.capabilities = vec!["storage.own".into()];
        m.collections = vec![oracle_core::ModuleCollection {
            name: "runs".into(),
            schema: json!({"type":"object"}),
        }];
        m.operations[0].audience = ModuleAudience::MemberMutation;
        m.operations[0].capabilities = vec!["storage.own".into()];
        m.operations[0].callback_methods = vec!["host.document_get".into()];
        m.operations[0].callback_collections = vec!["runs".into()];
        validate_manifest(&m).unwrap();
        for api in ["^1.3", ">=1.0", "~1.3"] {
            let mut bad = m.clone();
            bad.host_api = api.into();
            assert!(validate_manifest(&bad).is_err());
        }
        for method in [
            "host.echo",
            "host.notify",
            "host.contract_invoke",
            "unknown",
        ] {
            let mut bad = m.clone();
            bad.operations[0].callback_methods = vec![method.into()];
            assert!(validate_manifest(&bad).is_err());
        }
        let mut bad = m.clone();
        bad.operations[0].callback_collections = vec!["foreign".into()];
        assert!(validate_manifest(&bad).is_err());
        let mut bad = m.clone();
        bad.operations[0].audience = ModuleAudience::MemberRead;
        assert!(validate_manifest(&bad).is_err());
        let mut bad = m.clone();
        bad.operations[0].name = "initialize".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = m.clone();
        bad.operations[0].capabilities.clear();
        assert!(validate_manifest(&bad).is_err());
        m.operations[0].audience = ModuleAudience::Operator;
        m.operations[0].name = "maintenance".into();
        validate_manifest(&m).unwrap();
    }
    #[test]
    fn cards_require_host_api_1_2_and_declared_reply_shape() {
        let mut manifest = member_manifest();
        // Existing 1.1 plain text modules remain accepted on the 1.2 host.
        validate_manifest(&manifest).unwrap();
        manifest.commands.as_mut().unwrap().routes[0].presentation =
            Some(ModulePresentation::CardV1 {
                pointer: "/reply".into(),
            });
        let reply = &mut manifest.operations[0].output_schema["properties"]["reply"];
        reply["required"] = json!(["text", "card", "citations", "buttons", "choices"]);
        reply["properties"]["card"] = json!({"type":"object"});
        reply["properties"]["buttons"] = json!({"type":"array"});
        reply["properties"]["choices"] = json!({"type":"array"});
        assert!(validate_manifest(&manifest).is_err());
        manifest.host_api = "^1.2".into();
        validate_manifest(&manifest).unwrap();
        manifest.host_api = "^1.1.4".into();
        assert!(validate_manifest(&manifest).is_err());
        manifest.host_api = "^1.2".into();
        manifest.operations[0].output_schema["properties"]["reply"]["additionalProperties"] =
            json!(true);
        assert!(validate_manifest(&manifest).is_err());
    }
    #[test]
    fn optional_image_schema_requires_host_1_3_and_exact_metadata_shape() {
        let mut manifest = member_manifest();
        manifest.host_api = "^1.3".into();
        manifest.commands.as_mut().unwrap().routes[0].presentation =
            Some(ModulePresentation::CardV1 {
                pointer: "/reply".into(),
            });
        let reply = &mut manifest.operations[0].output_schema["properties"]["reply"];
        reply["required"] = json!(["text", "card", "citations", "buttons", "choices"]);
        reply["properties"]["card"] = json!({"type":"object"});
        reply["properties"]["buttons"] = json!({"type":"array"});
        reply["properties"]["choices"] = json!({"type":"array"});
        reply["properties"]["image"] = json!({"type":"object","additionalProperties":false,"required":["url","revision"],"properties":{"url":{"type":"string"},"revision":{"type":"integer"}}});
        validate_manifest(&manifest).unwrap();
        manifest.host_api = "^1.2".into();
        assert!(validate_manifest(&manifest).is_err());
        manifest.host_api = "^1.3".into();
        manifest.operations[0].output_schema["properties"]["reply"]["properties"]["image"]["properties"]
            ["source_url"] = json!({"type":"string"});
        assert!(validate_manifest(&manifest).is_err());
    }
    #[test]
    fn v2_requires_new_protocol_host_api_and_typed_schema_equivalence() {
        let manifest = member_manifest();
        validate_manifest(&manifest).unwrap();
        for (pointer, replacement) in [
            ("/protocol_minor_min", json!(0)),
            ("/host_api", json!("^1.0")),
            ("/host_api", json!("^1.0.1")),
            ("/commands/routes/0/input/options/1/max_value", json!(11)),
            ("/commands/routes/0/input/options/0/max_length", json!(101)),
            ("/commands/routes/0/input/options/0/required", json!(false)),
            ("/commands/routes/0/input/options/1/name", json!("unknown")),
            (
                "/operations/0/input_schema/additionalProperties",
                json!(true),
            ),
            (
                "/operations/0/input_schema/properties/name/pattern",
                json!("a+"),
            ),
            ("/commands/routes/0/input_required", json!(true)),
            (
                "/operations/0/output_schema/properties/reply/properties/citations/prefixItems",
                json!([true]),
            ),
            (
                "/operations/0/output_schema/properties/reply/$ref",
                json!("#"),
            ),
            ("/commands/routes/0/presentation/pointer", json!("/other")),
            (
                "/operations/0/output_schema/properties/reply/additionalProperties",
                json!(true),
            ),
            (
                "/operations/0/output_schema/properties/reply/properties/citations/maxItems",
                json!(6),
            ),
        ] {
            let mut value = serde_json::to_value(&manifest).unwrap();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            value
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(key.into(), replacement);
            let altered: ModuleManifest = serde_json::from_value(value).unwrap();
            assert!(
                validate_manifest(&altered).is_err(),
                "accepted invalid descriptor {pointer}"
            );
        }
    }

    #[test]
    fn member_read_cannot_acquire_privileged_registration_or_callbacks() {
        let manifest = member_manifest();
        let mut bad = manifest.clone();
        bad.capabilities = vec!["host.echo".into()];
        bad.operations[0].capabilities = bad.capabilities.clone();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        bad.operations[0].ai = Some(oracle_core::ModuleAiOperation {
            kind: oracle_core::ModuleAiOperationKind::Inspection,
            success_pointer: None,
        });
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        bad.migrations.push(oracle_core::ModuleMigration {
            from: 0,
            to: 1,
            operation: bad.operations[0].name.clone(),
        });
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        bad.provides.push(oracle_core::ProvidedContract {
            name: "read".into(),
            version: "1.0.0".into(),
            operation: bad.operations[0].name.clone(),
        });
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest;
        bad.operations[0].name = "event.deliver".into();
        bad.commands.as_mut().unwrap().routes[0].operation = "event.deliver".into();
        assert!(validate_manifest(&bad).is_err());
    }
    #[test]
    fn typed_choices_order_duplicates_and_safe_integer_bounds_are_enforced() {
        let manifest = member_manifest();
        let mut bad = manifest.clone();
        let Some(ModuleCommandInput::Typed { options }) =
            &mut bad.commands.as_mut().unwrap().routes[0].input
        else {
            panic!()
        };
        options.swap(0, 1);
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        let Some(ModuleCommandInput::Typed { options }) =
            &mut bad.commands.as_mut().unwrap().routes[0].input
        else {
            panic!()
        };
        options.push(options[0].clone());
        assert!(validate_manifest(&bad).is_err());
        let mut bad = manifest.clone();
        let Some(ModuleCommandInput::Typed { options }) =
            &mut bad.commands.as_mut().unwrap().routes[0].input
        else {
            panic!()
        };
        options[1].value_type = ModuleCommandOptionType::Integer {
            min_value: 1,
            max_value: 9_007_199_254_740_992,
        };
        bad.operations[0].input_schema["properties"]["limit"]["maximum"] =
            json!(9_007_199_254_740_992_u64);
        assert!(validate_manifest(&bad).is_err());
        let mut choices = manifest;
        let Some(ModuleCommandInput::Typed { options }) =
            &mut choices.commands.as_mut().unwrap().routes[0].input
        else {
            panic!()
        };
        let ModuleCommandOptionType::String {
            choices: values, ..
        } = &mut options[0].value_type
        else {
            panic!()
        };
        *values = vec!["Pebble".into(), "Astro".into()];
        choices.operations[0].input_schema["properties"]["name"]["enum"] =
            json!(["Pebble", "Astro"]);
        validate_manifest(&choices).unwrap();
        choices.operations[0].input_schema["properties"]["name"]["enum"] = json!(["Pebble"]);
        assert!(validate_manifest(&choices).is_err());
    }

    #[test]
    fn v2_package_install_preserves_versioned_descriptors_and_digest() {
        let (temp, mut package) = fixture();
        package.manifest = member_manifest();
        write(&temp, &package);
        let store = ArtifactStore::new(temp.0.join("store")).unwrap();
        let installed = store.install(&temp.0.join("source"), true).unwrap();
        assert_eq!(installed.package.manifest, package.manifest);
        store.verify(&installed).unwrap();
        assert_eq!(
            store.install(&temp.0.join("source"), true).unwrap(),
            installed
        );
    }
}
