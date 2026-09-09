//! Deterministic selection from a host-authorized immutable catalog snapshot.
//! Filtering authority belongs to the host port before constructing this value.
use crate::provider::ToolDefinition;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MAX_SELECTED_TOOLS: usize = 12;
pub const MAX_SELECTED_SCHEMA_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub definition: ToolDefinition,
    pub tags: Vec<String>,
    /// Host-created binding includes operation version and any module identity/epoch.
    pub binding: String,
    /// Discovery and receipt inspection are pinned by host code, never module prose.
    pub pinned: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedTool {
    pub definition: ToolDefinition,
    pub binding: String,
    pub descriptor_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub revision: u64,
    pub tools: Vec<SelectedTool>,
}
pub struct Catalog {
    revision: u64,
    entries: Vec<Entry>,
}
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("invalid or duplicate catalog entry")]
    Invalid,
    #[error("catalog or selected schemas exceed limits")]
    TooLarge,
    #[error("selected tool identity is stale or unavailable")]
    Stale,
}
fn digest(entry: &Entry) -> Result<String, CatalogError> {
    let bytes = serde_json::to_vec(entry).map_err(|_| CatalogError::Invalid)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}
impl Catalog {
    pub fn new(revision: u64, mut entries: Vec<Entry>) -> Result<Self, CatalogError> {
        if revision == 0 || entries.len() > 4096 {
            return Err(CatalogError::TooLarge);
        }
        let mut names = BTreeSet::new();
        for entry in &entries {
            let name = &entry.definition.name;
            if name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || !names.insert(name)
                || entry.binding.is_empty()
                || entry.binding.len() > 512
                || entry.definition.description.len() > 2048
                || entry.tags.len() > 16
                || entry.tags.iter().any(|tag| tag.len() > 64)
                || !entry.definition.parameters.is_object()
            {
                return Err(CatalogError::Invalid);
            }
        }
        if serde_json::to_vec(&entries)
            .map_err(|_| CatalogError::Invalid)?
            .len()
            > 4 * 1024 * 1024
            || entries.iter().filter(|entry| entry.pinned).count() > MAX_SELECTED_TOOLS
        {
            return Err(CatalogError::TooLarge);
        }
        entries.sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
        Ok(Self { revision, entries })
    }

    pub fn select(&self, query: &str, limit: usize) -> Result<Selection, CatalogError> {
        if query.len() > 4096
            || limit == 0
            || limit > MAX_SELECTED_TOOLS
            || self.entries.iter().filter(|entry| entry.pinned).count() > limit
        {
            return Err(CatalogError::Invalid);
        }
        let query = words(query);
        let mut ranked: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let names = words(&entry.definition.name);
                let tags = words(&entry.tags.join(" "));
                let description = words(&entry.definition.description);
                let score = query.intersection(&names).count() * 4
                    + query.intersection(&tags).count() * 3
                    + query.intersection(&description).count();
                (entry.pinned || score > 0).then_some((entry, score))
            })
            .collect();
        ranked.sort_by(|(a, sa), (b, sb)| {
            b.pinned
                .cmp(&a.pinned)
                .then(sb.cmp(sa))
                .then(a.definition.name.cmp(&b.definition.name))
        });
        let mut tools = Vec::new();
        let mut bytes = 0;
        for (entry, _) in ranked.into_iter().take(limit) {
            bytes += serde_json::to_vec(&entry.definition)
                .map_err(|_| CatalogError::Invalid)?
                .len();
            if bytes > MAX_SELECTED_SCHEMA_BYTES {
                return Err(CatalogError::TooLarge);
            }
            tools.push(SelectedTool {
                definition: entry.definition.clone(),
                binding: entry.binding.clone(),
                descriptor_hash: digest(entry)?,
            });
        }
        Ok(Selection {
            revision: self.revision,
            tools,
        })
    }

    /// Revalidate every tool before a batch is admitted. A changed registry always
    /// forces rediscovery; an old alias must never silently invoke a new generation.
    pub fn validate(&self, selection: &Selection) -> Result<(), CatalogError> {
        if selection.revision != self.revision || selection.tools.len() > MAX_SELECTED_TOOLS {
            return Err(CatalogError::Stale);
        }
        let mut names = BTreeSet::new();
        for tool in &selection.tools {
            if !names.insert(&tool.definition.name) {
                return Err(CatalogError::Stale);
            }
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.definition.name == tool.definition.name)
                .ok_or(CatalogError::Stale)?;
            if entry.binding != tool.binding
                || digest(entry)? != tool.descriptor_hash
                || serde_json::to_value(&entry.definition).map_err(|_| CatalogError::Invalid)?
                    != serde_json::to_value(&tool.definition).map_err(|_| CatalogError::Invalid)?
            {
                return Err(CatalogError::Stale);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn entry(name: &str, description: &str, pinned: bool) -> Entry {
        Entry {
            definition: ToolDefinition {
                name: name.into(),
                description: description.into(),
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
            tags: vec![],
            binding: "generation:1/epoch:1".into(),
            pinned,
        }
    }
    #[test]
    fn irrelevant_modules_do_not_consume_schema_context() {
        let mut entries = vec![
            entry("core_tools_search_v1", "Discover operations", true),
            entry("core_operation_get_v1", "Read receipts", true),
            entry(
                "logging_config_plan_v1",
                "Configure activity logging moderate preset",
                false,
            ),
        ];
        for i in 0..100 {
            entries.push(entry(
                &format!("unrelated_{i}"),
                "Weather sports finance",
                false,
            ));
        }
        let catalog = Catalog::new(1, entries).unwrap();
        let selected = catalog.select("moderate activity logging", 12).unwrap();
        assert_eq!(selected.tools.len(), 3);
        assert!(
            selected
                .tools
                .iter()
                .any(|t| t.definition.name == "logging_config_plan_v1")
        );
        catalog.validate(&selected).unwrap();
    }
    #[test]
    fn changed_epoch_schema_or_registry_requires_rediscovery() {
        let original = entry("logging_config_plan_v1", "Logging", false);
        let selected = Catalog::new(1, vec![original.clone()])
            .unwrap()
            .select("logging", 12)
            .unwrap();
        assert_eq!(
            Catalog::new(2, vec![original.clone()])
                .unwrap()
                .validate(&selected),
            Err(CatalogError::Stale)
        );
        let mut changed = original;
        changed.binding = "generation:2/epoch:1".into();
        assert_eq!(
            Catalog::new(1, vec![changed]).unwrap().validate(&selected),
            Err(CatalogError::Stale)
        );
    }
    #[test]
    fn selection_ties_are_stable_and_duplicates_are_rejected() {
        let a = entry("a_config", "Configure logging", false);
        let b = entry("b_config", "Configure logging", false);
        let catalog = Catalog::new(1, vec![b, a.clone()]).unwrap();
        assert_eq!(
            catalog.select("logging", 1).unwrap().tools[0]
                .definition
                .name,
            "a_config"
        );
        assert!(matches!(
            Catalog::new(1, vec![a.clone(), a]),
            Err(CatalogError::Invalid)
        ));
    }
    #[test]
    fn schema_limits_reject_instead_of_truncating_contracts() {
        let mut tool = entry("large_config", "Configuration", true);
        tool.definition.parameters["description"] = json!("x".repeat(MAX_SELECTED_SCHEMA_BYTES));
        assert!(matches!(
            Catalog::new(1, vec![tool]).unwrap().select("config", 12),
            Err(CatalogError::TooLarge)
        ));
    }
}
