//! First-release review gate. A transport must independently attest every source
//! validation; this pure comparator cannot establish that an HTTP request happened.
use crate::{
    model::{CatalogData, EvidenceState},
    snapshot::{Snapshot, validate},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Eligible,
    ReviewRequired,
    Rejected,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    InvalidActive,
    InvalidCandidate,
    InvalidTimestamp,
    FutureTimestamp,
    ValidationRegressed,
    RevisionRegressed,
    RevisionContentMismatch,
    SourceIdentityMismatch,
    EntityIdentityMismatch,
    DiscoveryLoss,
    CategoryLoss,
    ParserQualityRegressed,
    SourceChanged,
    SourceDeleted,
    EntityAdded,
    EntityDeleted,
    FactsChanged,
    CatalogChanged,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReviewReason {
    pub code: ReasonCode,
    pub count: u64,
    pub examples: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CandidateReview {
    pub active_digest: String,
    pub candidate_digest: String,
    pub status: ReviewStatus,
    pub reasons: Vec<ReviewReason>,
}
/// This is an operator decision, not a credential. The caller must authenticate
/// the approver and retain the decision; module or wiki input cannot create it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewApproval {
    pub active_digest: String,
    pub candidate_digest: String,
}

struct Findings {
    rejected: bool,
    reasons: BTreeMap<ReasonCode, ReviewReason>,
}
impl Findings {
    fn add(&mut self, code: ReasonCode, rejected: bool, id: &str) {
        self.rejected |= rejected;
        let reason = self.reasons.entry(code).or_insert_with(|| ReviewReason {
            code,
            count: 0,
            examples: vec![],
        });
        reason.count = reason.count.saturating_add(1);
        if reason.examples.len() < 8 {
            let example: String = id.chars().filter(|c| !c.is_control()).take(120).collect();
            if !example.is_empty() && !reason.examples.contains(&example) {
                reason.examples.push(example);
            }
        }
    }
    fn finish(self, active_digest: String, candidate_digest: String) -> CandidateReview {
        CandidateReview {
            active_digest,
            candidate_digest,
            status: if self.rejected {
                ReviewStatus::Rejected
            } else if self.reasons.is_empty() {
                ReviewStatus::Eligible
            } else {
                ReviewStatus::ReviewRequired
            },
            reasons: self.reasons.into_values().collect(),
        }
    }
}
fn digest_shape(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Review exact candidate bytes against a serving snapshot loaded through
/// `Snapshot::from_bytes` or `Store::load`. The original byte digests bind review
/// even when whitespace or semantically irrelevant ordering changes.
pub fn review_candidate(active: &Snapshot, candidate_bytes: &[u8], now_ms: u64) -> CandidateReview {
    let candidate_digest = format!("{:x}", Sha256::digest(candidate_bytes));
    let active_digest = if digest_shape(&active.id) {
        active.id.clone()
    } else {
        String::new()
    };
    let mut findings = Findings {
        rejected: false,
        reasons: BTreeMap::new(),
    };
    if !digest_shape(&active.id) || validate(&active.data).is_err() {
        findings.add(ReasonCode::InvalidActive, true, "");
        return findings.finish(active_digest, candidate_digest);
    }
    let Ok(candidate) = Snapshot::from_bytes(candidate_bytes) else {
        findings.add(ReasonCode::InvalidCandidate, true, "");
        return findings.finish(active_digest, candidate_digest);
    };
    let old = &active.data;
    let new = &candidate.data;
    let old_interval = interval(old, now_ms, &mut findings);
    let new_interval = interval(new, now_ms, &mut findings);
    if let (Some((old_start, old_end)), Some((new_start, new_end))) = (old_interval, new_interval)
        && (new_start < old_start || new_end < old_end)
    {
        findings.add(ReasonCode::ValidationRegressed, true, "crawl");
    }
    let old_sources: BTreeMap<_, _> = old.sources.iter().map(|s| (s.id.as_str(), s)).collect();
    let old_pages: BTreeMap<_, _> = old
        .sources
        .iter()
        .map(|s| (s.page_id, s.id.as_str()))
        .collect();
    let new_sources: BTreeSet<_> = new.sources.iter().map(|s| s.id.as_str()).collect();
    let mut titles = BTreeSet::new();
    for source in &new.sources {
        if source.id != format!("page:{}", source.page_id)
            || source.url != canonical_url(&source.title)
            || !titles.insert(&source.title)
            || old_pages
                .get(&source.page_id)
                .is_some_and(|id| *id != source.id)
        {
            findings.add(ReasonCode::SourceIdentityMismatch, true, &source.id);
        }
        let revision_time = parse_timestamp(&source.revision_timestamp);
        if revision_time.is_none() || revision_time.is_some_and(|t| t > source.validated_at_ms) {
            findings.add(ReasonCode::InvalidTimestamp, true, &source.id);
        }
        if source.validated_at_ms > now_ms {
            findings.add(ReasonCode::FutureTimestamp, true, &source.id);
        }
        match old_sources.get(source.id.as_str()) {
            None => {
                findings.add(ReasonCode::SourceChanged, false, &source.id);
                check_validation_interval(
                    source.validated_at_ms,
                    new_interval,
                    &source.id,
                    &mut findings,
                );
            }
            Some(previous) => {
                if source.page_id != previous.page_id {
                    findings.add(ReasonCode::SourceIdentityMismatch, true, &source.id);
                }
                if source.validated_at_ms < previous.validated_at_ms {
                    findings.add(ReasonCode::ValidationRegressed, true, &source.id);
                }
                if source.validated_at_ms > previous.validated_at_ms {
                    check_validation_interval(
                        source.validated_at_ms,
                        new_interval,
                        &source.id,
                        &mut findings,
                    );
                }
                let old_revision_time = parse_timestamp(&previous.revision_timestamp);
                if old_revision_time.is_none()
                    || old_revision_time.is_some_and(|t| t > previous.validated_at_ms)
                {
                    findings.add(ReasonCode::InvalidActive, true, &source.id);
                }
                if source.revision_id < previous.revision_id
                    || revision_time
                        .zip(old_revision_time)
                        .is_some_and(|(n, o)| n < o)
                {
                    findings.add(ReasonCode::RevisionRegressed, true, &source.id);
                }
                if source.revision_id == previous.revision_id
                    && (source.content_sha256 != previous.content_sha256
                        || source.revision_timestamp != previous.revision_timestamp)
                {
                    findings.add(ReasonCode::RevisionContentMismatch, true, &source.id);
                }
                if source.title != previous.title || source.url != previous.url {
                    findings.add(ReasonCode::SourceChanged, false, &source.id);
                }
                if source.revision_id != previous.revision_id
                    || source.content_sha256 != previous.content_sha256
                {
                    findings.add(ReasonCode::SourceChanged, false, &source.id);
                }
            }
        }
    }
    let removed: Vec<_> = old_sources
        .keys()
        .filter(|id| !new_sources.contains(**id))
        .collect();
    // First-release hard stop: losing more than one fifth of the discovered
    // source set is major coverage loss, even if replacement pages hide the net
    // count change. Smaller complete-discovery removals still require review.
    let major_loss = removed.len().saturating_mul(5) > old.sources.len();
    for id in &removed {
        findings.add(
            if major_loss {
                ReasonCode::DiscoveryLoss
            } else {
                ReasonCode::SourceDeleted
            },
            major_loss,
            id,
        );
    }
    let new_entities: BTreeSet<_> = new.entities.iter().map(|e| e.id.as_str()).collect();
    let mut kind_counts = BTreeMap::new();
    let mut removed_by_kind = BTreeMap::new();
    for entity in &old.entities {
        *kind_counts.entry(entity.kind).or_insert(0usize) += 1;
        if !new_entities.contains(entity.id.as_str()) {
            *removed_by_kind.entry(entity.kind).or_insert(0usize) += 1;
        }
    }
    for (kind, removed) in removed_by_kind {
        if removed.saturating_mul(5) > kind_counts[&kind] {
            let kind = serde_json::to_value(kind).expect("kind is serializable");
            findings.add(ReasonCode::CategoryLoss, true, kind.as_str().unwrap());
        }
    }
    if new.coverage.unresolved_redirects.len() > old.coverage.unresolved_redirects.len()
        || new
            .coverage
            .warnings
            .iter()
            .any(|w| !old.coverage.warnings.contains(w))
    {
        findings.add(ReasonCode::ParserQualityRegressed, true, "coverage");
    }
    let old_entities: BTreeMap<_, _> = old.entities.iter().map(|e| (e.id.as_str(), e)).collect();
    let old_fact_owners: BTreeMap<_, _> = old
        .entities
        .iter()
        .flat_map(|e| e.facts.iter().map(move |f| (f.id.as_str(), e.id.as_str())))
        .collect();
    for entity in &new.entities {
        for fact in &entity.facts {
            if old_fact_owners
                .get(fact.id.as_str())
                .is_some_and(|owner| *owner != entity.id)
            {
                findings.add(ReasonCode::EntityIdentityMismatch, true, &fact.id);
            }
        }
        let Some(previous) = old_entities.get(entity.id.as_str()) else {
            findings.add(ReasonCode::EntityAdded, false, &entity.id);
            continue;
        };
        if entity.kind != previous.kind {
            findings.add(ReasonCode::EntityIdentityMismatch, true, &entity.id);
        }
        if entity
            .warnings
            .iter()
            .any(|w| !previous.warnings.contains(w))
        {
            findings.add(ReasonCode::ParserQualityRegressed, true, &entity.id);
        }
        let old_facts: BTreeMap<_, _> = previous.facts.iter().map(|f| (f.id.as_str(), f)).collect();
        let new_facts: BTreeSet<_> = entity.facts.iter().map(|f| f.id.as_str()).collect();
        for fact in &entity.facts {
            if let Some(before) = old_facts.get(fact.id.as_str()) {
                if before.state == EvidenceState::Supported
                    && matches!(
                        fact.state,
                        EvidenceState::Unknown | EvidenceState::Unverified
                    )
                {
                    findings.add(ReasonCode::ParserQualityRegressed, true, &fact.id);
                }
                if serde_json::to_value(before).ok() != serde_json::to_value(fact).ok() {
                    findings.add(ReasonCode::FactsChanged, false, &fact.id);
                }
            } else {
                findings.add(ReasonCode::FactsChanged, false, &fact.id);
            }
        }
        for id in old_facts.keys() {
            if !new_facts.contains(id) {
                findings.add(ReasonCode::ParserQualityRegressed, true, id);
            }
        }
    }
    for id in old_entities.keys() {
        if !new_entities.contains(id) {
            findings.add(ReasonCode::EntityDeleted, false, id);
        }
    }
    for (id, image) in &new.images {
        if image.validated_at_ms > now_ms {
            findings.add(ReasonCode::FutureTimestamp, true, id);
        }
        if let Some(previous) = old.images.get(id) {
            if image.validated_at_ms < previous.validated_at_ms {
                findings.add(ReasonCode::ValidationRegressed, true, id);
            }
            if image.file_page_id == previous.file_page_id && image.revision < previous.revision {
                findings.add(ReasonCode::RevisionRegressed, true, id);
            }
        }
        if old
            .images
            .get(id)
            .is_none_or(|previous| image.validated_at_ms > previous.validated_at_ms)
            && let Some((start, end)) = new_interval
            && !(start..=end).contains(&image.validated_at_ms)
        {
            findings.add(ReasonCode::InvalidTimestamp, true, id);
        }
    }
    if semantic_value(old) != semantic_value(new) {
        findings.add(ReasonCode::CatalogChanged, false, "");
    }
    findings.finish(active_digest, candidate_digest)
}

/// Recompute eligibility from authoritative bytes on every attempted publication.
/// An approval is usable only for exactly the active/candidate pair reviewed; no
/// approval overrides malformed provenance or the hard rejection gates.
pub fn publication_allowed(
    active: &Snapshot,
    candidate_bytes: &[u8],
    now_ms: u64,
    approval: Option<&ReviewApproval>,
) -> bool {
    let review = review_candidate(active, candidate_bytes, now_ms);
    match review.status {
        ReviewStatus::Eligible => true,
        ReviewStatus::Rejected => false,
        ReviewStatus::ReviewRequired => approval.is_some_and(|a| {
            digest_shape(&a.active_digest)
                && digest_shape(&a.candidate_digest)
                && a.active_digest == review.active_digest
                && a.candidate_digest == review.candidate_digest
        }),
    }
}
fn canonical_url(title: &str) -> String {
    let mut url = format!("{}/wiki/", crate::model::SOURCE_ORIGIN);
    for byte in title.replace(' ', "_").bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut url, "%{byte:02X}").expect("writing to string");
        }
    }
    url
}
fn semantic_value(data: &CatalogData) -> Value {
    let mut value = serde_json::to_value(data).expect("catalog contains JSON values only");
    let object = value.as_object_mut().unwrap();
    object.remove("crawl_started_at");
    object.remove("crawl_completed_at");
    if let Some(images) = object.get_mut("images").and_then(Value::as_object_mut) {
        for image in images.values_mut() {
            image.as_object_mut().unwrap().remove("validated_at_ms");
        }
    }
    let sources = object["sources"].as_array_mut().unwrap();
    for source in sources.iter_mut() {
        source.as_object_mut().unwrap().remove("validated_at_ms");
    }
    sources.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    object["entities"]
        .as_array_mut()
        .unwrap()
        .sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    value
}
fn check_validation_interval(
    validated: u64,
    interval: Option<(u64, u64)>,
    id: &str,
    findings: &mut Findings,
) {
    if interval.is_none_or(|(start, end)| validated < start || validated > end) {
        findings.add(ReasonCode::InvalidTimestamp, true, id);
    }
}
fn interval(data: &CatalogData, now: u64, findings: &mut Findings) -> Option<(u64, u64)> {
    let pair =
        parse_timestamp(&data.crawl_started_at).zip(parse_timestamp(&data.crawl_completed_at));
    match pair {
        Some((start, end)) if start <= end && end <= now => Some((start, end)),
        Some((_, end)) if end > now => {
            findings.add(ReasonCode::FutureTimestamp, true, "crawl");
            None
        }
        _ => {
            findings.add(ReasonCode::InvalidTimestamp, true, "crawl");
            None
        }
    }
}
// The API/importer uses UTC RFC3339, with either Z or +00:00 and up to nine
// fractional digits. Validate the calendar rather than lexically ordering strings.
pub(crate) fn parse_timestamp(value: &str) -> Option<u64> {
    let value = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"))?;
    if !value.is_ascii()
        || value.len() < 19
        || &value[4..5] != "-"
        || &value[7..8] != "-"
        || &value[10..11] != "T"
        || &value[13..14] != ":"
        || &value[16..17] != ":"
    {
        return None;
    }
    let number = |start, end| {
        let digits = value.get(start..end)?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<i64>().ok()
    };
    let (mut year, month, day, hour, minute, second) = (
        number(0, 4)?,
        number(5, 7)?,
        number(8, 10)?,
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
    );
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > days[(month - 1) as usize] {
        return None;
    }
    let fractional = if value.len() == 19 {
        0
    } else {
        let fraction = value.get(19..)?.strip_prefix('.')?;
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let mut ms = 0;
        for n in 0..3 {
            ms = ms * 10 + u64::from(fraction.as_bytes().get(n).copied().unwrap_or(b'0') - b'0');
        }
        ms
    };
    year -= i64::from(month <= 2);
    let era = year / 400;
    let yoe = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    u64::try_from(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000)
        .ok()?
        .checked_add(fractional)
}
