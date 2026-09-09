//! Versioned module wire and persistence contracts. Native execution is operator-trusted.
use crate::{GuildId, ModuleId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleOperation {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub timeout_ms: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
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
