//! Deterministic, offline retrieval. A request retains one immutable catalog.
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const DAY: u64 = 86_400_000;
fn default_limit() -> usize {
    10
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryRequest {
    Search {
        query: String,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Lookup {
        name: String,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        field: Option<String>,
        #[serde(default)]
        offset: usize,
    },
    Compare {
        left: String,
        right: String,
        #[serde(default)]
        field: Option<String>,
    },
    Ask {
        question: String,
    },
    Sources {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        offset: usize,
    },
    Status {},
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct QueryError(pub String);
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub kind: Kind,
}
#[derive(Clone, Debug, Serialize)]
pub struct AnswerBlock {
    pub entity_id: String,
    pub fact_ids: Vec<String>,
    pub key: String,
    pub text: String,
    pub value: serde_json::Value,
    pub unit: Option<String>,
    pub conditions: Vec<String>,
    pub state: EvidenceState,
    pub citations: Vec<Citation>,
    pub source_ids: Vec<String>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct QueryResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EntityImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation_request: Option<QueryRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_option: Option<String>,
    pub snapshot_id: String,
    pub status: String,
    pub message: String,
    pub candidates: Vec<Candidate>,
    pub answer_blocks: Vec<AnswerBlock>,
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
    pub next_offset: Option<usize>,
}
pub struct QueryEngine {
    snapshot_id: String,
    data: Arc<CatalogData>,
    names: BTreeMap<String, Vec<usize>>,
    sources: BTreeMap<String, usize>,
    lexical: BTreeMap<String, BTreeSet<usize>>,
}
fn normalize(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}
fn check(s: &str, max: usize) -> Result<(), QueryError> {
    if s.trim().is_empty() || s.chars().count() > max || s.chars().any(char::is_control) {
        Err(QueryError(format!(
            "Input must contain 1..={max} characters and no control characters"
        )))
    } else {
        Ok(())
    }
}
fn candidate(e: &Entity) -> Candidate {
    Candidate {
        id: e.id.clone(),
        name: e.name.clone(),
        kind: e.kind,
    }
}
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.chars().collect();
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j] + 1).min(old + 1).min(prev + usize::from(x != *y));
            prev = old;
        }
    }
    row[b.len()]
}
impl QueryEngine {
    pub fn new(snapshot_id: String, data: Arc<CatalogData>) -> Self {
        let mut names: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, e) in data.entities.iter().enumerate() {
            for n in std::iter::once(&e.name).chain(e.aliases.iter()) {
                let ids = names.entry(normalize(n)).or_default();
                if !ids.contains(&i) {
                    ids.push(i);
                }
            }
        }
        let mut lexical: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for (i, entity) in data.entities.iter().enumerate() {
            for text in std::iter::once(entity.name.as_str()).chain(
                entity
                    .facts
                    .iter()
                    .flat_map(|f| [f.key.as_str(), f.text.as_str()]),
            ) {
                for token in text
                    .split(|c: char| !c.is_alphanumeric())
                    .map(str::to_lowercase)
                    .filter(|t| t.len() > 1)
                {
                    lexical.entry(token).or_default().insert(i);
                }
            }
        }
        let sources = data
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();
        Self {
            snapshot_id,
            data,
            names,
            sources,
            lexical,
        }
    }
    fn response(&self, status: &str, message: &str) -> QueryResponse {
        QueryResponse {
            image: None,
            navigation_request: None,
            selection_option: None,
            snapshot_id: self.snapshot_id.clone(),
            status: status.into(),
            message: message.into(),
            candidates: vec![],
            answer_blocks: vec![],
            sources: vec![],
            warnings: vec![],
            next_offset: None,
        }
    }
    fn matches(&self, name: &str, kind: Option<Kind>) -> Vec<&Entity> {
        // Explicit IDs disambiguate; display names and aliases have equal priority.
        if let Some(e) = self
            .data
            .entities
            .iter()
            .find(|e| e.id == name && kind.is_none_or(|k| e.kind == k))
        {
            return vec![e];
        }
        let mut result: Vec<_> = self
            .names
            .get(&normalize(name))
            .into_iter()
            .flatten()
            .map(|i| &self.data.entities[*i])
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        result
    }
    fn suggestions(&self, name: &str, kind: Option<Kind>, limit: usize) -> Vec<Candidate> {
        let q = normalize(name);
        let tokens: Vec<_> = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut ranked: Vec<_> = self
            .data
            .entities
            .iter()
            .enumerate()
            .filter(|(_, e)| kind.is_none_or(|k| e.kind == k))
            .filter_map(|(index, e)| {
                let rank = std::iter::once(&e.name)
                    .chain(e.aliases.iter())
                    .map(|n| {
                        let n = normalize(n);
                        if n == q {
                            0
                        } else if n.contains(&q) || q.contains(&n) {
                            1
                        } else if q.len() > 2 && distance(&q, &n) <= 2 {
                            3
                        } else {
                            4
                        }
                    })
                    .min()
                    .unwrap_or(4);
                let lexical = !tokens.is_empty()
                    && tokens
                        .iter()
                        .all(|t| self.lexical.get(t).is_some_and(|ids| ids.contains(&index)));
                let rank = if lexical { rank.min(2) } else { rank };
                (rank < 4).then_some((rank, e))
            })
            .collect();
        ranked.sort_by(|(r, a), (s, b)| r.cmp(s).then(a.id.cmp(&b.id)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, e)| candidate(e))
            .collect()
    }
    fn resolve(&self, name: &str, kind: Option<Kind>) -> Result<&Entity, Box<QueryResponse>> {
        let es = self.matches(name, kind);
        if es.len() == 1 {
            return Ok(es[0]);
        }
        let mut r = self.response(
            if es.is_empty() {
                "not_found"
            } else {
                "needs_clarification"
            },
            if es.is_empty() {
                "No exact wiki match. Select a candidate explicitly, if appropriate."
            } else {
                "This name identifies more than one entity. Select an ID or kind."
            },
        );
        r.candidates = if es.is_empty() {
            self.suggestions(name, kind, 10)
        } else {
            es.into_iter().take(10).map(candidate).collect()
        };
        Err(Box::new(r))
    }
    fn field_matches(f: &Fact, field: &str) -> bool {
        if field == "all details" {
            return true;
        }
        let k = normalize(&f.key);
        let p = normalize(field);
        if k == p {
            return true;
        }
        let is_ability = f.key.split(|c: char| !c.is_alphanumeric()).any(|word| {
            word.eq_ignore_ascii_case("ability") || word.eq_ignore_ascii_case("abilities")
        });
        match p.as_str() {
            "stats" | "statistics" => [
                "stat",
                "health",
                "speed",
                "movementspeed",
                "attentionspan",
                "detectionrange",
                "stamina",
                "stealth",
                "skillcheck",
                "extraction",
            ]
            .iter()
            .any(|p| k.starts_with(p)),
            "speed" => k == "movementspeed",
            "ability" | "abilities" => is_ability,
            "effectorability" => k == "effect" || is_ability,
            "unlock" | "requirements" => {
                k.contains("unlock") || k.contains("requirement") || k.contains("obtainment")
            }
            "research" => k.contains("research"),
            "blackout" => k.contains("blackout"),
            _ => false,
        }
    }
    fn add_fact(&self, r: &mut QueryResponse, e: &Entity, f: &Fact, now: u64) {
        let mut warnings = e.warnings.clone();
        let mut state = f.state;
        if e.availability != EvidenceState::Supported {
            warnings.push(format!("Entity availability is {:?}.", e.availability));
            if state == EvidenceState::Supported {
                state = e.availability;
            }
        }
        let mut ids = BTreeSet::new();
        let mut invalid = f.citations.is_empty();
        let mut expired = false;
        for c in &f.citations {
            if let Some(index) = self.sources.get(&c.source_id) {
                let s = &self.data.sources[*index];
                ids.insert(s.id.clone());
                if s.validated_at_ms > now {
                    invalid = true;
                    warnings.push("Source validation time is in the future.".into());
                }
                let age = now.saturating_sub(s.validated_at_ms);
                if age > DAY {
                    warnings.push(format!(
                        "Cached source {}; last checked at {} milliseconds since Unix epoch.",
                        s.title, s.validated_at_ms
                    ));
                }
                if age > 7 * DAY {
                    expired = true;
                }
            } else {
                invalid = true;
            }
        }
        if invalid {
            state = EvidenceState::Unverified;
            warnings.push("Fact has missing or invalid source provenance.".into());
        }
        // Only expressly historical material can be shown after the refusal window.
        // Unknown field keys are conservatively treated as mutable.
        let refuse = expired && state != EvidenceState::Historical;
        if refuse {
            warnings.push("Cannot verify this cached fact after seven days; consult the linked wiki revision.".into());
            r.status = "stale".into();
        }
        if state == EvidenceState::Conflicting && r.status != "stale" {
            r.status = "source_conflict".into();
        }
        if state != EvidenceState::Supported {
            warnings.push(format!(
                "Evidence state: {state:?}; this is not an unqualified current fact."
            ));
        }
        let suppress = refuse
            || invalid
            || matches!(
                state,
                EvidenceState::Unknown | EvidenceState::Conflicting | EvidenceState::Unverified
            );
        r.answer_blocks.push(AnswerBlock {
            entity_id: e.id.clone(),
            fact_ids: vec![f.id.clone()],
            key: f.key.clone(),
            text: if suppress {
                "The requested fact is not verified for a current answer.".into()
            } else {
                f.text.clone()
            },
            value: if suppress {
                serde_json::Value::Null
            } else {
                f.value.clone()
            },
            unit: f.unit.clone(),
            conditions: f.conditions.clone(),
            state,
            citations: f.citations.clone(),
            source_ids: ids.into_iter().collect(),
            warnings,
        });
    }
    fn finish(&self, mut r: QueryResponse) -> QueryResponse {
        if r.status == "answered"
            && !r.answer_blocks.is_empty()
            && r.answer_blocks
                .iter()
                .all(|b| matches!(b.state, EvidenceState::Unknown | EvidenceState::Unverified))
        {
            r.status = "unavailable".into();
            r.message = "The requested information is not verified in this snapshot.".into();
        }
        let ids: BTreeSet<_> = r
            .answer_blocks
            .iter()
            .flat_map(|b| b.source_ids.iter())
            .collect();
        r.sources = ids
            .into_iter()
            .filter_map(|id| self.sources.get(id).map(|i| self.data.sources[*i].clone()))
            .collect();
        r
    }
    fn lookup(
        &self,
        name: &str,
        kind: Option<Kind>,
        field: Option<&str>,
        offset: usize,
        now: u64,
    ) -> QueryResponse {
        let e = match self.resolve(name, kind) {
            Ok(e) => e,
            Err(mut r) => {
                r.navigation_request = Some(QueryRequest::Lookup {
                    name: name.into(),
                    kind,
                    field: field.map(str::to_owned),
                    offset,
                });
                r.selection_option = Some("name".into());
                return *r;
            }
        };
        let mut facts: Vec<_> = e
            .facts
            .iter()
            .filter(|f| field.is_none_or(|x| Self::field_matches(f, x)))
            .collect();
        // Overview pages lead with useful gameplay facts; direct field requests
        // keep their existing ordering and complete evidence.
        let priority = |f: &Fact| {
            if field.is_some() {
                return 0;
            }
            if f.state != EvidenceState::Supported {
                return 20;
            }
            match f.key.as_str() {
                "health" => 0,
                "ability_1" | "ability_2" | "effect_or_ability" => 1,
                "description" | "effect" | "overview" => 2,
                "requirements" | "unlock_requirements" => 3,
                "speed" | "movement_speed" | "stamina" => 4,
                "gender" | "designation" => 15,
                _ => 10,
            }
        };
        facts.sort_by(|a, b| {
            priority(a)
                .cmp(&priority(b))
                .then(a.key.cmp(&b.key))
                .then(a.id.cmp(&b.id))
        });
        if facts.is_empty() {
            return self.response(
                "not_found",
                "No supported catalog field matches this request; use the source article.",
            );
        }
        let mut r = self.response("answered", &e.name);
        r.candidates.push(candidate(e));
        r.image = self
            .data
            .images
            .get(&e.id)
            .filter(|image| {
                image.validated_at_ms <= now && now.saturating_sub(image.validated_at_ms) <= 7 * DAY
            })
            .cloned();
        r.warnings = e.warnings.clone();
        if offset >= facts.len() {
            r.status = "not_found".into();
            r.message = "No fields at this pagination offset.".into();
            return r;
        }
        for f in facts.iter().skip(offset).take(10) {
            self.add_fact(&mut r, e, f, now);
        }
        if offset.saturating_add(10) < facts.len() {
            r.next_offset = Some(offset + 10);
            r.warnings.push("More fields are available. Continue with next_offset or select a field; each returned fact retains all conditions.".into());
        }
        self.finish(r)
    }
    pub fn execute(&self, request: QueryRequest, now_ms: u64) -> Result<QueryResponse, QueryError> {
        let r = match request {
            QueryRequest::Lookup {
                name,
                kind,
                field,
                offset,
            } => {
                check(&name, 100)?;
                if let Some(f) = &field {
                    check(f, 100)?;
                }
                self.lookup(&name, kind, field.as_deref(), offset, now_ms)
            }
            QueryRequest::Search { query, kind, limit } => {
                check(&query, 100)?;
                if !(1..=10).contains(&limit) {
                    return Err(QueryError("Result limit must be 1..=10".into()));
                }
                let mut r = self.response(
                    "answered",
                    "Matching catalog entries; select an exact ID for facts.",
                );
                r.candidates = self.suggestions(&query, kind, limit + 1);
                if r.candidates.len() > limit {
                    r.candidates.truncate(limit);
                    r.warnings
                        .push("More matches exist; narrow the query or select a kind.".into());
                }
                if r.candidates.is_empty() {
                    r.status = "not_found".into();
                }
                r
            }
            QueryRequest::Compare { left, right, field } => {
                check(&left, 100)?;
                check(&right, 100)?;
                if let Some(f) = &field {
                    check(f, 100)?;
                }
                let a = match self.resolve(&left, None) {
                    Ok(e) => e,
                    Err(mut r) => {
                        r.navigation_request = Some(QueryRequest::Compare {
                            left: left.clone(),
                            right: right.clone(),
                            field: field.clone(),
                        });
                        r.selection_option = Some("left".into());
                        return Ok(*r);
                    }
                };
                let b = match self.resolve(&right, None) {
                    Ok(e) => e,
                    Err(mut r) => {
                        r.navigation_request = Some(QueryRequest::Compare {
                            left: a.id.clone(),
                            right: right.clone(),
                            field: field.clone(),
                        });
                        r.selection_option = Some("right".into());
                        return Ok(*r);
                    }
                };
                let mut r = self.response(
                    "answered",
                    "Sourced values shown side by side; no best-entity recommendation is implied.",
                );
                if a.kind != b.kind {
                    return Ok(self.response(
                        "unsupported_query",
                        "Comparison requires entities of the same kind.",
                    ));
                }
                r.candidates = vec![candidate(a), candidate(b)];
                let mut count = 0;
                for f in &a.facts {
                    if field.as_deref().is_some_and(|k| !Self::field_matches(f, k)) {
                        continue;
                    }
                    let matches: Vec<_> = b.facts.iter().filter(|g| g.key == f.key).collect();
                    if matches.is_empty() {
                        r.warnings
                            .push(format!("{} has no {} field.", b.name, f.key));
                    }
                    for g in matches {
                        let mut x = f.conditions.clone();
                        let mut y = g.conditions.clone();
                        x.sort();
                        y.sort();
                        if f.unit != g.unit || x != y {
                            r.warnings.push(format!(
                                "{} cannot be compared: units or conditions differ.",
                                f.key
                            ));
                            continue;
                        }
                        if count == 5 {
                            r.warnings.push(
                                "Additional comparisons omitted; select a specific field.".into(),
                            );
                            continue;
                        }
                        self.add_fact(&mut r, a, f, now_ms);
                        self.add_fact(&mut r, b, g, now_ms);
                        count += 1;
                    }
                }
                if count == 0 {
                    r.status = "unsupported_query".into();
                    r.message = "No compatible fields are available for this comparison.".into();
                }
                self.finish(r)
            }
            QueryRequest::Ask { question } => {
                check(&question, 500)?;
                self.ask(&question, now_ms)
            }
            QueryRequest::Sources { name, kind, offset } => {
                let mut r =
                    self.response("answered", "Wiki source revision and validation metadata.");
                let ids: Option<BTreeSet<_>> = if let Some(name) = name {
                    check(&name, 100)?;
                    let e = match self.resolve(&name, kind) {
                        Ok(e) => e,
                        Err(r) => return Ok(*r),
                    };
                    Some(
                        e.facts
                            .iter()
                            .flat_map(|f| f.citations.iter())
                            .chain(e.relationships.iter().flat_map(|r| r.citations.iter()))
                            .map(|c| c.source_id.as_str())
                            .collect(),
                    )
                } else {
                    None
                };
                let mut sources: Vec<_> = self
                    .data
                    .sources
                    .iter()
                    .filter(|s| ids.as_ref().is_none_or(|ids| ids.contains(s.id.as_str())))
                    .collect();
                sources.sort_by(|a, b| a.id.cmp(&b.id));
                r.sources = sources
                    .iter()
                    .skip(offset)
                    .take(10)
                    .map(|s| (*s).clone())
                    .collect();
                if offset.saturating_add(10) < sources.len() {
                    r.next_offset = Some(offset + 10);
                }
                if r.sources.is_empty() {
                    r.status = "not_found".into();
                }
                r
            }
            QueryRequest::Status {} => {
                let mut r = self.response(
                    if self.data.entities.is_empty() {
                        "unavailable"
                    } else {
                        "answered"
                    },
                    "Offline wiki catalog; no network refresh is performed by this query engine.",
                );
                if let Some(oldest) = self.data.sources.iter().map(|s| s.validated_at_ms).min()
                    && now_ms.saturating_sub(oldest) > DAY
                {
                    r.warnings.push(format!("Catalog includes cached sources last checked at {oldest} milliseconds since Unix epoch."));
                }
                r
            }
        };
        Ok(r)
    }
    fn ask(&self, question: &str, now: u64) -> QueryResponse {
        // A deliberately closed grammar consumes the entire question. No partial
        // match can silently drop a second request, modifier, or instruction.
        let q = question
            .trim()
            .trim_end_matches('?')
            .trim()
            .replace('’', "'")
            .to_lowercase();
        let unsupported = || {
            self.response("unsupported_query","This question is outside the supported question patterns. Use an exact lookup, field, search, or comparison.")
        };
        if q.contains(';') || q.contains('\n') || q.contains("http") {
            return unsupported();
        }
        let mut kind = None;
        let mut field = None;
        let mut name = None;
        for prefix in [
            "what happens during ",
            "what does ",
            "what are ",
            "what is ",
            "who is ",
            "tell me about ",
            "explain ",
            "how does ",
            "how do ",
        ] {
            if let Some(rest) = q.strip_prefix(prefix) {
                name = Some(rest);
                break;
            }
        }
        if let Some(rest) = q
            .strip_prefix("how do i unlock ")
            .or_else(|| q.strip_prefix("how to unlock "))
            .or_else(|| q.strip_prefix("how do i get "))
        {
            name = Some(rest);
            field = Some("unlock");
        }
        let Some(mut name) = name else {
            return unsupported();
        };
        if q.starts_with("what happens during ") {
            for article in ["a ", "an ", "the "] {
                if let Some(rest) = name.strip_prefix(article) {
                    name = rest;
                    break;
                }
            }
            kind = Some(Kind::Mechanic);
        }
        if q.starts_with("what does ") {
            let Some(n) = name.strip_suffix(" do") else {
                return unsupported();
            };
            name = n;
            field = Some("effect_or_ability");
        }
        if (q.starts_with("how does ") || q.starts_with("how do ")) && field.is_none() {
            let Some(n) = name.strip_suffix(" work") else {
                return unsupported();
            };
            name = n;
        }
        for (prefix, f) in [
            ("the stats of ", "stats"),
            ("the stats for ", "stats"),
            ("the abilities of ", "abilities"),
            ("the ability of ", "abilities"),
            ("the speed of ", "speed"),
            ("the unlock requirements for ", "unlock"),
            ("the requirements for ", "unlock"),
        ] {
            if let Some(rest) = name.strip_prefix(prefix) {
                name = rest;
                field = Some(f);
                break;
            }
        }
        for (prefix, k) in [
            ("toon ", Kind::Toon),
            ("twisted ", Kind::Twisted),
            ("floor ", Kind::Floor),
            ("mechanic ", Kind::Mechanic),
            ("item ", Kind::Item),
            ("trinket ", Kind::Trinket),
        ] {
            if let Some(rest) = name.strip_prefix(prefix) {
                name = rest;
                kind = Some(k);
                break;
            }
        }
        for (suffix, f) in [
            ("'s stats", "stats"),
            ("'s speed", "speed"),
            ("'s health", "health"),
            ("'s statistics", "stats"),
            ("'s abilities", "abilities"),
            ("'s ability", "abilities"),
            ("'s unlock requirements", "unlock"),
            ("'s research", "research"),
            ("'s blackout behavior", "blackout"),
        ] {
            if let Some(rest) = name.strip_suffix(suffix) {
                name = rest;
                field = Some(f);
                break;
            }
        }
        if name.chars().count() > 100 || name.is_empty() {
            return unsupported();
        }
        if kind.is_none() {
            let matches = self.matches(name, None);
            if field == Some("unlock") {
                let eligible: Vec<_> = matches
                    .iter()
                    .filter(|e| matches!(e.kind, Kind::Toon | Kind::Npc | Kind::Trinket))
                    .collect();
                if eligible.len() == 1 {
                    kind = Some(eligible[0].kind);
                }
            } else if (q.starts_with("how does ") || q.starts_with("how do "))
                && matches.iter().any(|e| e.kind == Kind::Mechanic)
            {
                kind = Some(Kind::Mechanic);
            }
        }
        // Conjunctions may be part of a sourced proper name, but never a
        // second request that is dropped during matching.
        if (q.contains(" and ") || q.contains(" or ")) && self.matches(name, kind).is_empty() {
            return unsupported();
        }
        // Only resolve the entire remaining entity span, never an embedded name.
        {
            let mut r = self.lookup(name, kind, field, 0, now);
            r.navigation_request = Some(QueryRequest::Lookup {
                name: name.into(),
                kind,
                field: field.map(str::to_owned),
                offset: 0,
            });
            r
        }
    }
}
