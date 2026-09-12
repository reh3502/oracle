//! Versioned module wire and persistence contracts. Native execution is operator-trusted.
use crate::{GuildId, ModuleId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModuleManifest {
    pub manifest_version: u32,
    pub id: ModuleId,
    pub version: String,
    pub target: String,
    pub protocol_major: u32,
    pub protocol_minor_min: u32,
    pub host_api: String,
    pub data_version: u32,
    pub readable_data_versions: Vec<u32>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub required_intents: Vec<String>,
    #[serde(default)]
    pub provides: Vec<ProvidedContract>,
    #[serde(default)]
    pub consumes: Vec<ConsumedContract>,
    pub operations: Vec<ModuleOperation>,
    #[serde(default)]
    pub collections: Vec<ModuleCollection>,
    #[serde(default)]
    pub migrations: Vec<ModuleMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<ModuleConfiguration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<GuildEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<ModuleCommands>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ModuleRuntimeRequirements>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleOperation {
    #[serde(default, skip_serializing_if = "ModuleAudience::is_operator")]
    pub audience: ModuleAudience,
    /// Optional reviewed projection; absence keeps the operation out of model tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<ModuleAiOperation>,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub timeout_ms: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
/// This metadata cannot grant capabilities or bypass host policy. Native artifacts
/// remain operator-trusted; arbitrary mutations have no AI projection in v1.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleAiOperation {
    pub kind: ModuleAiOperationKind,
    /// Boolean postcondition in the typed result, checked alongside host receipts.
    pub success_pointer: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleAiOperationKind {
    Inspection,
    Verification,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCollection {
    pub name: String,
    pub schema: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleMigration {
    pub from: u32,
    pub to: u32,
    pub operation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProvidedContract {
    pub name: String,
    pub version: String,
    pub operation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedContract {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub optional: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModulePackage {
    pub manifest: ModuleManifest,
    pub entrypoint: String,
    pub files: BTreeMap<String, String>,
    pub source_revision: String,
    pub toolchain: String,
    pub license: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InstalledModule {
    pub digest: String,
    pub package: ModulePackage,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DesiredModule {
    pub module: ModuleId,
    pub digest: String,
    pub loaded: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DesiredActivation {
    pub module: ModuleId,
    pub guild: GuildId,
    pub active: bool,
    pub grants: Vec<String>,
    pub bindings: BTreeMap<String, ModuleId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleDocument {
    pub collection: String,
    pub key: String,
    pub value: Value,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentWrite {
    pub collection: String,
    pub key: String,
    pub expected_revision: Option<u64>,
    pub value: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationProgress {
    pub module: ModuleId,
    pub guild: GuildId,
    pub data_version: u32,
    pub target_version: Option<u32>,
    pub artifact_digest: Option<String>,
    pub cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationPage {
    pub documents: Vec<ModuleDocument>,
    pub next_cursor: Option<String>,
}

/// Presets are versioned partial objects merged over current values. The resulting
/// complete object must satisfy the module schema and host policy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleConfiguration {
    pub schema_version: u32,
    pub schema: Value,
    pub presets: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfiguration {
    pub revision: u64,
    pub values: Value,
}

/// Host-normalized metadata only. No message body, attachment, audit reason, or
/// arbitrary Gateway payload crosses this contract.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum GuildEventKind {
    ModerationAudit,
    ChannelChanged,
    RoleAccessChanged,
    MemberRolesChanged,
    Ban,
    Unban,
    MemberJoined,
    MemberLeft,
    Maintenance,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuildEventOrigin {
    External,
    Oracle,
    /// The Gateway observation does not identify the actor.
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuildEvent {
    pub id: String,
    pub kind: GuildEventKind,
    pub occurred_at_ms: u64,
    pub origin: GuildEventOrigin,
    pub subject_id: Option<String>,
    pub actor_id: Option<String>,
    pub related_id: Option<String>,
}

/// Explicit human command opt-in. The host owns the final Discord namespace and
/// resolves these routes to the same versioned operations used by other ingress.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCommands {
    pub namespace: String,
    pub description: String,
    pub routes: Vec<ModuleCommandRoute>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCommandRoute {
    pub name: String,
    pub description: String,
    pub operation: String,
    /// One fixed JSON string option named input. If omitted, the host uses {}.
    #[serde(default)]
    pub input_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ModuleCommandInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ModulePresentation>,
}

/// Host observations for the exact module invocation; no recursive module RPC is used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleHostHealth {
    pub module: ModuleId,
    pub guild: GuildId,
    pub session: String,
    pub generation: u64,
    pub epoch: u64,
    pub observed_at_ms: u64,
    pub configuration: HostConfigurationHealth,
    pub subscriptions: HostSubscriptionHealth,
    pub destination: HostDestinationHealth,
    pub queue: Option<HostEventQueueHealth>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostConfigurationHealth {
    pub stored: Option<EffectiveConfiguration>,
    pub receipt_state: Option<String>,
    pub verified: bool,
    pub error: Option<crate::ErrorCode>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSubscriptionHealth {
    pub declared: Vec<GuildEventKind>,
    pub effective: Vec<GuildEventKind>,
    pub missing_intents: Vec<String>,
    pub ready: bool,
    pub error: Option<crate::ErrorCode>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostDestinationHealth {
    pub id: Option<String>,
    pub verified: bool,
    pub error: Option<crate::ErrorCode>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostEventQueueHealth {
    pub accepting: bool,
    pub queued: usize,
    pub delivered: usize,
    pub dropped: usize,
    pub last_error: Option<crate::ErrorCode>,
}

/// Version 2 authority is a host declaration, never caller input.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleAudience {
    #[default]
    Operator,
    MemberRead,
}
impl ModuleAudience {
    pub fn is_operator(&self) -> bool {
        *self == Self::Operator
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleRuntimeRequirements {
    pub data_directory_required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleCommandInput {
    Json { required: bool },
    Typed { options: Vec<ModuleCommandOption> },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModuleCommandOption {
    pub name: String,
    pub description: String,
    pub required: bool,
    #[serde(flatten)]
    pub value_type: ModuleCommandOptionType,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleCommandOptionType {
    String {
        min_length: u32,
        max_length: u32,
        #[serde(default)]
        choices: Vec<String>,
    },
    Integer {
        min_value: i64,
        max_value: i64,
    },
    Boolean,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulePresentation {
    PlainTextV1 { pointer: String },
}

mod legacy;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleManifestV2 {
    #[serde(deserialize_with = "version_two")]
    pub manifest_version: u32,
    pub id: ModuleId,
    pub version: String,
    pub target: String,
    pub protocol_major: u32,
    pub protocol_minor_min: u32,
    pub host_api: String,
    pub data_version: u32,
    pub readable_data_versions: Vec<u32>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub required_intents: Vec<String>,
    #[serde(default)]
    pub provides: Vec<ProvidedContract>,
    #[serde(default)]
    pub consumes: Vec<ConsumedContract>,
    pub operations: Vec<ModuleOperation>,
    #[serde(default)]
    pub collections: Vec<ModuleCollection>,
    #[serde(default)]
    pub migrations: Vec<ModuleMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<ModuleConfiguration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<GuildEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<ModuleCommands>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ModuleRuntimeRequirements>,
}
fn version_two<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(d)?;
    if version != 2 {
        return Err(serde::de::Error::custom("expected manifest version 2"));
    }
    Ok(version)
}
impl<'de> Deserialize<'de> for ModuleManifest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            V1(legacy::ModuleManifestV1),
            V2(ModuleManifestV2),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::V1(v) => v.into(),
            Wire::V2(v) => Self {
                manifest_version: v.manifest_version,
                id: v.id,
                version: v.version,
                target: v.target,
                protocol_major: v.protocol_major,
                protocol_minor_min: v.protocol_minor_min,
                host_api: v.host_api,
                data_version: v.data_version,
                readable_data_versions: v.readable_data_versions,
                capabilities: v.capabilities,
                required_intents: v.required_intents,
                provides: v.provides,
                consumes: v.consumes,
                collections: v.collections,
                migrations: v.migrations,
                configuration: v.configuration,
                subscriptions: v.subscriptions,
                operations: v.operations,
                commands: v.commands,
                runtime: v.runtime,
            },
        })
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use serde_json::json;
    fn legacy() -> Value {
        serde_json::from_str(include_str!(
            "../../../examples/modules/counter/manifest.json"
        ))
        .unwrap()
    }
    #[test]
    fn v1_remains_strict_and_serialization_has_no_new_fields() {
        let value = legacy();
        let manifest: ModuleManifest = serde_json::from_value(value.clone()).unwrap();
        let encoded = serde_json::to_value(&manifest).unwrap();
        assert!(encoded.get("runtime").is_none());
        assert!(
            encoded["operations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o.get("audience").is_none())
        );
        assert_eq!(
            serde_json::from_value::<ModuleManifest>(encoded).unwrap(),
            manifest
        );
        for field in ["audience", "unexpected"] {
            let mut bad = value.clone();
            bad["operations"][0][field] = json!("operator");
            assert!(serde_json::from_value::<ModuleManifest>(bad).is_err());
        }
        let mut bad = value.clone();
        bad["runtime"] = json!({"data_directory_required":false});
        assert!(serde_json::from_value::<ModuleManifest>(bad).is_err());
        let mut bad = value;
        bad["manifest_version"] = json!(3);
        assert!(serde_json::from_value::<ModuleManifest>(bad).is_err());
    }
    #[test]
    fn v1_canonical_bytes_preserve_package_digests() {
        for source in [
            include_str!("../../../examples/modules/counter/manifest.json"),
            include_str!("../../../examples/modules/configuration-probe/manifest-events.json"),
        ] {
            let frozen: legacy::ModuleManifestV1 = serde_json::from_str(source).unwrap();
            let normalized: ModuleManifest = serde_json::from_str(source).unwrap();
            assert_eq!(
                serde_json::to_vec(&normalized).unwrap(),
                serde_json::to_vec(&frozen).unwrap()
            );
        }
    }
    #[test]
    fn v1_rejects_v2_command_fields_and_duplicate_known_fields() {
        let mut value = legacy();
        value["commands"] = json!({"namespace":"counter","description":"Counter","routes":[{"name":"read","description":"Read","operation":"read","input_required":false,"input":{"kind":"typed","options":[]}}]});
        assert!(serde_json::from_value::<ModuleManifest>(value).is_err());
        let json = serde_json::to_string(&legacy()).unwrap();
        let duplicate = json.replacen("{", "{\"manifest_version\":1,", 1);
        assert!(serde_json::from_str::<ModuleManifest>(&duplicate).is_err());
    }
    #[test]
    fn v2_typed_options_and_runtime_round_trip_and_remain_strict() {
        let mut value = legacy();
        value["manifest_version"] = json!(2);
        value["protocol_minor_min"] = json!(1);
        value["host_api"] = json!("^1.1");
        value["runtime"] = json!({"data_directory_required":true});
        value["operations"][0]["audience"] = json!("member_read");
        value["commands"] = json!({"namespace":"dw","description":"Game queries","routes":[{"name":"lookup","description":"Find an entity","operation":"lookup","input":{"kind":"typed","options":[{"name":"name","description":"Entity name","required":true,"type":"string","min_length":1,"max_length":100,"choices":[]}]},"presentation":{"kind":"plain_text_v1","pointer":"/reply"}}]});
        let manifest: ModuleManifest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(manifest.operations[0].audience, ModuleAudience::MemberRead);
        assert_eq!(
            serde_json::from_value::<ModuleManifest>(serde_json::to_value(&manifest).unwrap())
                .unwrap(),
            manifest
        );
        value["commands"]["routes"][0]["input"]["options"][0]["surprise"] = json!(true);
        assert!(serde_json::from_value::<ModuleManifest>(value).is_err());
    }
}
