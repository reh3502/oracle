//! Portable offline catalog contract shared with the import-time normalizer.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const SOURCE_ORIGIN: &str = "https://dandys-world-robloxhorror.fandom.com";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogData {
    pub schema_version: u32,
    pub adapter_version: String,
    pub source_origin: String,
    pub crawl_started_at: String,
    pub crawl_completed_at: String,
    pub sources: Vec<Source>,
    pub entities: Vec<Entity>,
    pub coverage: Coverage,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, EntityImage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub page_id: u64,
    pub title: String,
    pub url: String,
    pub revision_id: u64,
    pub revision_timestamp: String,
    pub validated_at_ms: u64,
    pub content_sha256: String,
    pub license: String,
    pub license_url: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Toon,
    Twisted,
    Npc,
    Floor,
    Machine,
    Mechanic,
    Trinket,
    Item,
    Event,
    Topic,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    Supported,
    Unknown,
    Conflicting,
    Historical,
    Unverified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub source_id: String,
    pub section: String,
    /// Original source fragment, never interpreted as instructions or executed.
    pub quote: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub id: String,
    pub key: String,
    pub text: String,
    pub value: serde_json::Value,
    pub unit: Option<String>,
    /// Conditions are required for comparison compatibility; no silent modifiers.
    pub conditions: Vec<String>,
    pub state: EvidenceState,
    pub citations: Vec<Citation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub relation: String,
    pub target_id: String,
    pub citations: Vec<Citation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub aliases: Vec<String>,
    pub availability: EvidenceState,
    pub warnings: Vec<String>,
    pub facts: Vec<Fact>,
    pub relationships: Vec<Relationship>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageEntry {
    pub page_id: u64,
    pub title: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub discovered_pages: usize,
    pub imported_pages: usize,
    pub namespace_counts: BTreeMap<String, usize>,
    pub nonredirect_articles: usize,
    pub redirects: usize,
    pub entities_by_kind: BTreeMap<String, usize>,
    pub excluded: Vec<CoverageEntry>,
    pub unresolved_redirects: Vec<CoverageEntry>,
    pub warnings: Vec<String>,
}

/// File provenance is distinct from the article's text license.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityImage {
    pub url: String,
    pub file_title: String,
    pub file_page_id: u64,
    pub revision: u64,
    pub sha1: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub validated_at_ms: u64,
    pub article_revision: u64,
}
