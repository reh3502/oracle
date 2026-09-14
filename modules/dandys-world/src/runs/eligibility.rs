//! Build a run's pinned roster from a complete, attributed normalized catalog.
//! Wiki purchase availability is not evidence about a member's Toon ownership.
use super::domain::{EligibilitySnapshot, Error};
use crate::{
    model::{CatalogData, Entity, EvidenceState, Kind, Source},
    refresh_review::parse_timestamp,
    snapshot,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SOURCE_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const REQUIRED: [(&str, u64); 7] = [
    ("Toons", 151),
    ("Dandy's World (Game)", 152),
    ("Template:RegularToons", 6643),
    ("Template:MCToons", 31108),
    ("Template:EventToons", 8886),
    ("Template:UnobtainableToons", 17070),
    ("Template:ToonAmount", 1545),
];

/// `snapshot_id` comes from Snapshot::from_bytes, not operation input. Source
/// observation times remain the actual observations; reading never renews them.
pub fn from_catalog(
    snapshot_id: &str,
    data: &CatalogData,
    now: u64,
) -> Result<EligibilitySnapshot, Error> {
    snapshot::validate(data).map_err(|_| Error::CatalogUnavailable)?;
    let crawl_start = parse_timestamp(&data.crawl_started_at).ok_or(Error::CatalogUnavailable)?;
    let crawl_end = parse_timestamp(&data.crawl_completed_at).ok_or(Error::CatalogUnavailable)?;
    if crawl_start > crawl_end || crawl_end > now {
        return Err(Error::CatalogUnavailable);
    }
    if snapshot_id.len() != 64
        || !snapshot_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::CatalogUnavailable);
    }
    let mut source_revisions = BTreeMap::new();
    let mut oldest = u64::MAX;
    for (title, page_id) in REQUIRED {
        let source = required_source(data, title, page_id)?;
        observe(source, now, &mut oldest)?;
        source_revisions.insert(title.to_owned(), source.revision_id);
    }
    let roster_entity = data
        .entities
        .iter()
        .find(|e| e.id == "page:151")
        .ok_or(Error::CatalogUnavailable)?;
    if roster_entity.name != "Toons" || !metadata_supported(roster_entity) {
        return Err(Error::CatalogUnavailable);
    }
    let game = data
        .entities
        .iter()
        .find(|e| e.id == "page:152")
        .ok_or(Error::CatalogUnavailable)?;
    if game.name != "Dandy's World (Game)" || !metadata_supported(game) {
        return Err(Error::CatalogUnavailable);
    }
    // Four complete template definitions occur in the roster fact's citations.
    // Require byte-for-byte source hashes before interpreting the reviewed simple
    // ToonBox format; a source header alone cannot prove roster completeness.
    let mut allowed_names = BTreeSet::new();
    for (title, page_id) in &REQUIRED[2..5] {
        let source = required_source(data, title, *page_id)?;
        allowed_names.extend(template_names(roster_entity, source)?);
    }
    let excluded = template_names(
        roster_entity,
        required_source(data, REQUIRED[5].0, REQUIRED[5].1)?,
    )?;
    if !allowed_names.is_disjoint(&excluded) || allowed_names.is_empty() || allowed_names.len() > 80
    {
        return Err(Error::CatalogUnavailable);
    }
    let mut actual_names = BTreeSet::new();
    let mut toons = BTreeMap::new();
    for entity in data.entities.iter().filter(|e| e.kind == Kind::Toon) {
        if !metadata_supported(entity)
            || excluded.contains(&entity.name)
            || matches!(entity.name.as_str(), "Dandy" | "Dyle")
        {
            continue;
        }
        if !actual_names.insert(entity.name.clone()) {
            return Err(Error::CatalogUnavailable);
        }
        let source = data
            .sources
            .iter()
            .find(|s| s.id == entity.id && s.title == entity.name)
            .ok_or(Error::CatalogUnavailable)?;
        if source.id != format!("page:{}", source.page_id) {
            return Err(Error::CatalogUnavailable);
        }
        observe(source, now, &mut oldest)?;
        toons.insert(entity.id.clone(), entity.name.clone());
    }
    if actual_names != allowed_names {
        return Err(Error::CatalogUnavailable);
    }
    let eligibility = EligibilitySnapshot {
        source_hash: snapshot_id.to_owned(),
        source_revisions,
        toons,
        observed_at: oldest,
        fresh_until: oldest
            .checked_add(MAX_SOURCE_AGE_MS)
            .ok_or(Error::CatalogUnavailable)?,
        disputed: false,
    };
    eligibility
        .require_fresh(now)
        .map_err(|_| Error::CatalogUnavailable)?;
    Ok(eligibility)
}

fn required_source<'a>(
    data: &'a CatalogData,
    title: &str,
    page_id: u64,
) -> Result<&'a Source, Error> {
    let mut matching = data.sources.iter().filter(|s| s.title == title);
    let source = matching.next().ok_or(Error::CatalogUnavailable)?;
    if matching.next().is_some()
        || source.page_id != page_id
        || source.id != format!("page:{page_id}")
    {
        return Err(Error::CatalogUnavailable);
    }
    Ok(source)
}

fn observe(source: &Source, now: u64, oldest: &mut u64) -> Result<(), Error> {
    let revision_time =
        parse_timestamp(&source.revision_timestamp).ok_or(Error::CatalogUnavailable)?;
    if revision_time > source.validated_at_ms {
        return Err(Error::CatalogUnavailable);
    }
    if source.validated_at_ms == 0
        || source.validated_at_ms > now
        || now - source.validated_at_ms >= MAX_SOURCE_AGE_MS
    {
        return Err(Error::CatalogUnavailable);
    }
    *oldest = (*oldest).min(source.validated_at_ms);
    Ok(())
}

fn metadata_supported(entity: &Entity) -> bool {
    entity.availability == EvidenceState::Supported
        && !entity.warnings.iter().any(|warning| {
            let warning = warning.to_ascii_lowercase();
            // "limited" means seasonal purchase, and remains eligible.
            let ineligible = [
                "unreleased",
                "scrapped",
                "unobtainable",
                "developer-only",
                "developer only",
            ]
            .iter()
            .any(|flag| warning.contains(flag));
            let eligibility_warning = ["eligib", "playable", "roster", "availability"]
                .iter()
                .any(|field| warning.contains(field));
            let disputed = ["conflict", "disputed", "unverified"]
                .iter()
                .any(|flag| warning.contains(flag));
            ineligible || (eligibility_warning && disputed)
        })
}

fn template_names(entity: &Entity, source: &Source) -> Result<BTreeSet<String>, Error> {
    let mut definitions = BTreeSet::new();
    for fact in &entity.facts {
        for citation in fact.citations.iter().filter(|c| c.source_id == source.id) {
            if fact.state != EvidenceState::Supported || citation.quote.len() > 16 * 1024 {
                return Err(Error::CatalogUnavailable);
            }
            let hash = format!("{:x}", Sha256::digest(citation.quote.as_bytes()));
            if hash != source.content_sha256 {
                return Err(Error::CatalogUnavailable);
            }
            definitions.insert(citation.quote.as_str());
        }
    }
    if definitions.len() != 1 {
        return Err(Error::CatalogUnavailable);
    }
    let mut names = BTreeSet::new();
    let mut remaining = *definitions.first().ok_or(Error::CatalogUnavailable)?;
    while let Some((_, after)) = remaining.split_once("{{") {
        let (body, rest) = after.split_once("}}").ok_or(Error::CatalogUnavailable)?;
        let mut parts = body.split('|');
        if parts.next() != Some("ToonBox") {
            return Err(Error::CatalogUnavailable);
        }
        let name = parts.next().ok_or(Error::CatalogUnavailable)?.trim();
        if name.is_empty()
            || name.chars().count() > 100
            || name.chars().any(|c| c.is_control() || "{}<>[]".contains(c))
            || !names.insert(name.to_owned())
        {
            return Err(Error::CatalogUnavailable);
        }
        for option in parts {
            let (key, value) = option.split_once('=').ok_or(Error::CatalogUnavailable)?;
            if !matches!(key.trim(), "type" | "textsize" | "margin")
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return Err(Error::CatalogUnavailable);
            }
        }
        if names.len() > 80 {
            return Err(Error::CatalogUnavailable);
        }
        remaining = rest;
    }
    if names.is_empty() {
        return Err(Error::CatalogUnavailable);
    }
    Ok(names)
}
