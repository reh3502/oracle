//! Bounded human replies. Every displayed fact keeps its complete evidence set.
use dandys_world_core::{
    model::{EvidenceState, SOURCE_ORIGIN, Source},
    query::{AnswerBlock, QueryRequest, QueryResponse},
};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize)]
pub struct Reply {
    pub text: String,
    pub citations: Vec<Reference>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Reference {
    pub label: String,
    pub revision: u64,
}
fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}
fn reference(source: &Source) -> Reference {
    let label = if utf16(&source.title) <= 100 && !source.title.chars().any(char::is_control) {
        format!("Wiki: {}", source.title)
    } else {
        format!("Wiki revision {}", source.revision_id)
    };
    Reference {
        label,
        revision: source.revision_id,
    }
}
fn fits(reply: &Reply) -> bool {
    !reply.text.trim().is_empty() && utf16(&reply.text) <= 1800 && reply.citations.len() <= 5
        && reply.citations.iter().all(|r| (1..=9_007_199_254_740_991).contains(&r.revision))
        // Account for the configured wiki revision URL, Markdown punctuation and separators.
        && utf16(&reply.text) + reply.citations.iter().map(|r| utf16(&r.label) + SOURCE_ORIGIN.len() + 16 + r.revision.to_string().len() + 20).sum::<usize>() <= 1900
}
fn append_refs(reply: &mut Reply, sources: impl Iterator<Item = Source>) {
    for source in sources {
        if !reply
            .citations
            .iter()
            .any(|r| r.revision == source.revision_id)
        {
            reply.citations.push(reference(&source));
        }
    }
}
fn warnings(lines: &[String]) -> String {
    let mut seen = BTreeSet::new();
    lines
        .iter()
        .filter(|s| seen.insert(s.as_str()))
        .map(|s| format!("\nNote: {s}"))
        .collect()
}
fn block_text(block: &AnswerBlock, name: &str) -> String {
    let mut text = format!(
        "\n\n{name} — {}: {}",
        block.key.replace('_', " "),
        block.text
    );
    if matches!(
        block.state,
        EvidenceState::Supported | EvidenceState::Historical
    ) && !block.value.is_null()
    {
        if block.value.is_number() || block.value.is_boolean() {
            text.push_str(&format!("\nValue: {}", block.value));
        }
        if !block.conditions.is_empty() {
            text.push_str(&format!("\nConditions: {}", block.conditions.join("; ")));
        }
        if let Some(unit) = &block.unit {
            text.push_str(&format!("\nUnit: {unit}"));
        }
    }
    text.push_str(&warnings(&block.warnings));
    text
}
fn continuation(
    request: &QueryRequest,
    response: &QueryResponse,
    consumed: usize,
    total: usize,
) -> String {
    match request {
        QueryRequest::Lookup { offset, .. } | QueryRequest::Sources { offset, .. } => {
            let next = if consumed < total {
                Some(offset + consumed)
            } else {
                response.next_offset
            };
            next.map(|n| {
                format!(
                    "\nContinue with /dw {} using the same options and offset:{n}.",
                    if matches!(request, QueryRequest::Sources { .. }) {
                        "sources"
                    } else {
                        "lookup"
                    }
                )
            })
            .unwrap_or_default()
        }
        QueryRequest::Ask { .. } if consumed < total || response.next_offset.is_some() => response
            .candidates
            .first()
            .map(|c| {
                format!(
                    "\nUse /dw lookup name:{} with a specific field for the remaining detail.",
                    c.id
                )
            })
            .unwrap_or_else(|| {
                "\nUse /dw lookup with a specific entity and field for more detail.".into()
            }),
        QueryRequest::Compare { .. } if consumed < total => {
            "\nAdditional comparisons do not fit; use /dw compare with a specific field.".into()
        }
        _ => String::new(),
    }
}
fn fallback(response: &QueryResponse, hint: &str, sources: &[Source]) -> Reply {
    let mut reply = Reply { text: "This result and its complete evidence do not fit in one reply. No game fact is being summarized here; consult the linked wiki revision or narrow the field.".into(), citations: vec![] };
    // A navigation link is not represented as the complete evidence for a claim.
    if let Some(source) = sources
        .iter()
        .find(|s| !s.title.starts_with("Template:"))
        .or_else(|| sources.first())
    {
        reply.citations.push(reference(source));
    } else if let Some(source) = response.sources.first() {
        reply.citations.push(reference(source));
    }
    if reply
        .citations
        .iter()
        .any(|r| !(1..=9_007_199_254_740_991).contains(&r.revision))
    {
        return Reply {
            text: "The source revision cannot be displayed safely. Ask an operator to validate the catalog.".into(),
            citations: vec![],
        };
    }
    reply.text.push_str(hint);
    if !fits(&reply) {
        reply.text = "This result is too long. Consult the linked wiki revision or use /dw lookup with a specific field.".into();
    }
    reply
}
pub fn render(request: &QueryRequest, response: &QueryResponse) -> Reply {
    let header = format!("{}{}", response.message, warnings(&response.warnings));
    if matches!(request, QueryRequest::Sources { .. }) && !response.sources.is_empty() {
        let mut reply = Reply {
            text: "Wiki contributors — CC BY-SA 3.0. Source revisions:".into(),
            citations: vec![],
        };
        let mut consumed = 0;
        for source in &response.sources {
            let mut candidate = reply.clone();
            candidate.text.push_str(&format!(
                "\n{} — revision {}, last checked {} ms since Unix epoch.",
                source.title, source.revision_id, source.validated_at_ms
            ));
            append_refs(&mut candidate, std::iter::once(source.clone()));
            let hint = continuation(request, response, consumed + 1, response.sources.len());
            candidate.text.push_str(&hint);
            if !fits(&candidate) {
                break;
            }
            candidate.text.truncate(candidate.text.len() - hint.len());
            reply = candidate;
            consumed += 1;
        }
        if consumed == 0 {
            return fallback(
                response,
                &continuation(request, response, 1, response.sources.len()),
                &response.sources[..1],
            );
        }
        reply.text.push_str(&continuation(
            request,
            response,
            consumed,
            response.sources.len(),
        ));
        return reply;
    }
    if response.answer_blocks.is_empty() {
        let mut reply = Reply {
            text: header,
            citations: vec![],
        };
        for candidate in &response.candidates {
            reply.text.push_str(&format!(
                "\n{} ({:?}); exact ID: {}",
                candidate.name, candidate.kind, candidate.id
            ));
        }
        if fits(&reply) {
            return reply;
        }
        return Reply { text: "The matches or warnings are too long for one reply. Narrow your search or use an exact entity name and kind.".into(), citations: vec![] };
    }
    let group_size = if matches!(request, QueryRequest::Compare { .. }) {
        2
    } else {
        1
    };
    let mut reply = Reply {
        text: format!("{header}\nWiki contributors — CC BY-SA 3.0."),
        citations: vec![],
    };
    let mut consumed = 0;
    for group in response.answer_blocks.chunks(group_size) {
        let mut candidate = reply.clone();
        let source_ids: BTreeSet<_> = group.iter().flat_map(|b| b.source_ids.iter()).collect();
        let sources: Vec<_> = response
            .sources
            .iter()
            .filter(|s| source_ids.contains(&s.id))
            .cloned()
            .collect();
        for block in group {
            let name = response
                .candidates
                .iter()
                .find(|c| c.id == block.entity_id)
                .map(|c| c.name.as_str())
                .unwrap_or(&block.entity_id);
            candidate.text.push_str(&block_text(block, name));
        }
        append_refs(&mut candidate, sources.clone().into_iter());
        let hint = continuation(
            request,
            response,
            consumed + group.len(),
            response.answer_blocks.len(),
        );
        candidate.text.push_str(&hint);
        if sources.len() != source_ids.len() || !fits(&candidate) {
            if consumed == 0 {
                return fallback(response, &hint, &sources);
            }
            break;
        }
        candidate.text.truncate(candidate.text.len() - hint.len());
        reply = candidate;
        consumed += group.len();
    }
    reply.text.push_str(&continuation(
        request,
        response,
        consumed,
        response.answer_blocks.len(),
    ));
    reply
}
