//! Strict legacy wire shapes. Keep v1 fields unchanged.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleManifestV1 {
    #[serde(deserialize_with = "version_one")]
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
    pub operations: Vec<ModuleOperationV1>,
    #[serde(default)]
    pub collections: Vec<ModuleCollection>,
    #[serde(default)]
    pub migrations: Vec<ModuleMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<ModuleConfiguration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<GuildEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<ModuleCommandsV1>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleOperationV1 {
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCommandsV1 {
    pub namespace: String,
    pub description: String,
    pub routes: Vec<ModuleCommandRouteV1>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCommandRouteV1 {
    pub name: String,
    pub description: String,
    pub operation: String,
    /// One fixed JSON string option named input. If omitted, the host uses {}.
    pub input_required: bool,
}
fn version_one<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(d)?;
    if version != 1 {
        return Err(serde::de::Error::custom("expected manifest version 1"));
    }
    Ok(version)
}
impl From<ModuleManifestV1> for ModuleManifest {
    fn from(v: ModuleManifestV1) -> Self {
        Self {
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
            runtime: None,
            operations: v
                .operations
                .into_iter()
                .map(|o| ModuleOperation {
                    ai: o.ai,
                    name: o.name,
                    description: o.description,
                    input_schema: o.input_schema,
                    output_schema: o.output_schema,
                    timeout_ms: o.timeout_ms,
                    capabilities: o.capabilities,
                    audience: ModuleAudience::Operator,
                })
                .collect(),
            commands: v.commands.map(|c| ModuleCommands {
                namespace: c.namespace,
                description: c.description,
                routes: c
                    .routes
                    .into_iter()
                    .map(|r| ModuleCommandRoute {
                        name: r.name,
                        description: r.description,
                        operation: r.operation,
                        input_required: r.input_required,
                        input: None,
                        presentation: None,
                    })
                    .collect(),
            }),
        }
    }
}
