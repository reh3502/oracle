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
