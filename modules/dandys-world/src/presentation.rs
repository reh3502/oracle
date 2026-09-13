//! Bounded cards with complete facts, evidence, and actionable navigation.
use dandys_world_core::{
    model::{EvidenceState, Source},
    query::{AnswerBlock, QueryRequest, QueryResponse},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize)]
pub struct Reply {
    pub text: String,
    pub card: Card,
    pub citations: Vec<Reference>,
    pub buttons: Vec<Action>,
    pub choices: Vec<Choice>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub title: String,
    pub description: String,
    pub fields: Vec<Field>,
    pub footer: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub inline: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Reference {
    pub label: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Action {
    pub label: String,
    pub route: String,
    pub options: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Prompt>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Prompt {
    pub label: String,
    pub option: String,
    pub placeholder: String,
    pub max_length: u16,
}
#[derive(Clone, Debug, Serialize)]
pub struct Choice {
    pub label: String,
    pub description: String,
    pub route: String,
    pub options: Value,
}
fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}
// Presentation-only projection. The catalog and its source evidence stay untouched.
fn display_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !((c.is_control() && *c != '\n')
                || matches!(c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' |
        '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200d}' | '\u{feff}'))
        })
        .collect()
}
fn link_like(token: &str) -> bool {
    token.contains("://")
        || token.contains("www.")
        || token.contains('@')
        || token.split('.').collect::<Vec<_>>().windows(2).any(|p| {
            p[0].chars().last().is_some_and(char::is_alphanumeric)
                && p[1].chars().next().is_some_and(char::is_alphabetic)
        })
}
fn rendered_cost(c: char, links: bool, markdown: bool) -> usize {
    c.len_utf16()
        + usize::from(
            c == '@'
                || (links && matches!(c, '.' | ':' | '/'))
                || (markdown
                    && matches!(
                        c,
                        '\\' | '*' | '_' | '~' | '`' | '[' | ']' | '(' | ')' | '#' | '|'
                    )),
        )
}
// Mirrors the host's display expansion, without inserting its security escapes.
fn rendered_len(s: &str, markdown: bool) -> usize {
    s.split_inclusive(char::is_whitespace)
        .map(|token| {
            let links = link_like(token);
            token
                .chars()
                .map(|c| rendered_cost(c, links, markdown))
                .sum::<usize>()
        })
        .sum()
}
fn short(s: &str, max: usize) -> String {
    let clean = display_text(s);
    let mut n = 0;
    clean
        .chars()
        .take_while(|c| {
            // Conservative link cost also covers tokens cut at the label boundary.
            n += rendered_cost(*c, true, true);
            n <= max
        })
        .collect()
}
fn action(label: &str, route: &str, options: Value) -> Action {
    Action {
        label: label.into(),
        route: route.into(),
        options,
        prompt: None,
    }
}
fn prompt_action(
    label: &str,
    route: &str,
    options: Value,
    question: &str,
    option: &str,
    placeholder: &str,
) -> Action {
    Action {
        prompt: Some(Prompt {
            label: question.into(),
            option: option.into(),
            placeholder: placeholder.into(),
            max_length: 100,
        }),
        ..action(label, route, options)
    }
}
fn ask() -> Action {
    Action {
        prompt: Some(Prompt {
            label: "What would you like to know?".into(),
            option: "question".into(),
            placeholder: "What does Poppy do?".into(),
            max_length: 200,
        }),
        ..action("Ask a question", "ask", json!({}))
    }
}
fn base(title: &str, description: &str) -> Reply {
    Reply {
        text: String::new(),
        card: Card {
            title: short(title, 256),
            description: display_text(description),
            fields: vec![],
            footer: "Dandy’s World Wiki contributors • CC BY-SA 3.0".into(),
        },
        citations: vec![],
        buttons: vec![],
        choices: vec![],
    }
}
fn finish(mut r: Reply) -> Reply {
    r.citations
        .sort_by_key(|s| s.label.starts_with("Template:"));
    for (index, source) in r
        .citations
        .iter_mut()
        .filter(|s| s.label.starts_with("Template:"))
        .enumerate()
    {
        source.label = format!("Reference {}", index + 1);
    }
    r.buttons.truncate(4);
    r.buttons.push(ask());
    r.text = format!(
        "{}\n{}{}\n{}",
        r.card.title,
        r.card.description,
        r.card
            .fields
            .iter()
            .map(|f| format!("\n\n{}\n{}", f.name, f.value))
            .collect::<String>(),
        r.card.footer
    );
    if utf16(&r.text) > 1800 {
        r.text = format!(
            "{}\nOpen the card to read the complete details and wiki sources.",
            r.card.title
        );
    }
    r
}
fn fits(r: &Reply) -> bool {
    let source_len = r
        .citations
        .iter()
        .map(|s| rendered_len(&s.label, true) + 120)
        .sum::<usize>();
    r.citations.len() <= 5
        && r.citations
            .iter()
            .all(|s| (1..=9_007_199_254_740_991).contains(&s.revision))
        && rendered_len(&r.card.title, true) <= 256
        && rendered_len(&r.card.description, true) <= 4096
        && r.card.fields.len() + usize::from(!r.citations.is_empty()) <= 20
        && source_len <= 1024
        && r.card.fields.iter().all(|f| {
            !f.name.trim().is_empty()
                && !f.value.trim().is_empty()
                && rendered_len(&f.name, true) <= 256
                && rendered_len(&f.value, true) <= 1024
        })
        && rendered_len(&r.card.title, true)
            + rendered_len(&r.card.description, true)
            + rendered_len(&r.card.footer, false)
            + r.card
                .fields
                .iter()
                .map(|f| rendered_len(&f.name, true) + rendered_len(&f.value, true))
                .sum::<usize>()
            + source_len
            + if r.citations.is_empty() {
                0
            } else {
                utf16("Wiki sources")
            }
            < 5800
}
fn refs(r: &mut Reply, sources: &[Source]) {
    for s in sources {
        if !r.citations.iter().any(|x| x.revision == s.revision_id) {
            r.citations.push(Reference {
                label: short(&s.title, 100),
                revision: s.revision_id,
            });
        }
    }
}
fn plain_warning(s: &str) -> String {
    if s.starts_with("Cached source ") || s.contains("milliseconds since Unix epoch") {
        "This uses saved wiki information that has not been checked in over a day.".into()
    } else if s.starts_with("More fields are available") {
        String::new()
    } else if s.starts_with("More matches exist") {
        "More matches are available. Use Search by name to be more specific.".into()
    } else if s.starts_with("Entity availability is Historical") {
        "This describes past or older game content.".into()
    } else if s.starts_with("Entity availability is ") {
        "This character or item’s current availability needs checking.".into()
    } else if s.starts_with("Evidence state:") {
        String::new()
    } else if s.contains("seven days") {
        "This has not been checked for over seven days, so I can’t confirm it. Check the wiki source below.".into()
    } else if s.contains("missing or invalid source provenance") {
        "The wiki evidence for this detail needs checking.".into()
    } else {
        s.into()
    }
}
fn warnings(lines: &[String]) -> String {
    lines
        .iter()
        .map(|s| display_text(&plain_warning(s)))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n")
}
fn value_is_written(text: &str, value: &Value, unit: Option<&str>) -> bool {
    let numeric = value.to_string();
    let value_present = if value.is_number() {
        text.split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
            .any(|word| word.trim_end_matches('.') == numeric)
    } else {
        text.split(|c: char| !c.is_alphanumeric())
            .any(|word| word.eq_ignore_ascii_case(&numeric))
    };
    value_present && unit.is_none_or(|u| text.to_lowercase().contains(&u.to_lowercase()))
}
fn block_text(b: &AnswerBlock) -> String {
    let suppressed =
        b.value.is_null() && b.text == "The requested fact is not verified for a current answer.";
    let canonical_health = b.key == "health"
        && b.unit.as_deref() == Some("hearts")
        && b.value.as_u64().is_some_and(|n| (1..=10).contains(&n))
        && b.text
            == format!(
                "Maximum starting health: {} hearts. Main Heart decoration is not counted.",
                b.value
            );
    let mut s = if suppressed {
        match b.state {
            EvidenceState::Conflicting => {
                "The wiki sources disagree about this detail. Check the sources below.".into()
            }
            _ => "I can’t confirm this detail from the saved wiki information yet.".into(),
        }
    } else if canonical_health {
        format!(
            "{} {} starting hearts\nThe decorative Main Heart does not add health.",
            "❤️".repeat(b.value.as_u64().unwrap() as usize),
            b.value
        )
    } else {
        b.text.clone()
    };
    if !suppressed {
        if b.state == EvidenceState::Historical {
            s = format!("Past event or older information\n{s}");
        }
        if !canonical_health
            && b.key == "health"
            && b.unit.as_deref() == Some("hearts")
            && b.value.as_u64().is_some_and(|n| (1..=10).contains(&n))
        {
            let n = b.value.as_u64().unwrap();
            s = format!("{} {n} hearts\n{s}", "❤️".repeat(n as usize));
        } else if !canonical_health
            && (b.value.is_number() || b.value.is_boolean())
            && !value_is_written(&s, &b.value, b.unit.as_deref())
        {
            s = format!(
                "{}{}\n{s}",
                b.value,
                b.unit.as_ref().map(|u| format!(" {u}")).unwrap_or_default()
            );
        }
        for c in &b.conditions {
            if canonical_health && c == "maximum starting health as shown by normal Heart slots" {
                continue;
            }
            if !s.to_lowercase().contains(&c.to_lowercase()) {
                s.push_str(&format!("\nApplies when: {c}"));
            }
        }
    }
    let w = warnings(&b.warnings);
    if !w.is_empty() {
        s.push_str(&format!("\n\n{w}"));
    }
    s
}
fn readable_block(b: &AnswerBlock) -> (String, String) {
    let mut name = friendly_field(&b.key);
    let mut text = block_text(b);
    if b.key.starts_with("ability_") && b.state == EvidenceState::Supported {
        if let Some((first, rest)) = text.split_once('\n')
            && first.len() <= 60
            && (rest.starts_with("(Active)") || rest.starts_with("(Passive)"))
        {
            name = first.to_owned();
            text = rest.trim().to_owned();
        }
        text = text.replace("drastically decreasing Stealth to a value of", "setting Stealth to")
            .replace("alerting any Twisteds nearby to his location", "letting nearby Twisteds know where he is")
            .replace("This Toon can sniff out items, causing them to be highlighted when in the Toon's vicinity.", "Highlights items near this Toon.")
            .replace("Has a cooldown of", "Cooldown:");
    }
    while text.contains("\n\n") {
        text = text.replace("\n\n", "\n");
    }
    (name, text)
}
fn field_parts(name: String, value: String, inline: bool) -> Vec<Field> {
    // Every chunk of a fact is accepted or rejected together by the card budget.
    let name = display_text(&name);
    let value = display_text(&value);
    let mut parts = Vec::new();
    let mut head = String::new();
    let mut used = 0;
    for token in value.split_inclusive(char::is_whitespace) {
        let links = link_like(token);
        for c in token.chars() {
            let cost = rendered_cost(c, links, true);
            if used + cost > 1000 {
                parts.push(Field {
                    name: if parts.is_empty() {
                        name.clone()
                    } else {
                        format!("{} (continued)", short(&name, 240))
                    },
                    value: std::mem::take(&mut head),
                    inline,
                });
                used = 0;
            }
            head.push(c);
            used += cost;
        }
    }
    if !head.is_empty() {
        parts.push(Field {
            name: if parts.is_empty() {
                name
            } else {
                format!("{} (continued)", short(&name, 240))
            },
            value: head,
            inline,
        });
    }
    parts
}
fn options(req: &QueryRequest) -> (String, Value) {
    let mut v = serde_json::to_value(req).unwrap();
    let route = v
        .as_object_mut()
        .unwrap()
        .remove("op")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    v.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    (route, v)
}
fn pages(
    r: &mut Reply,
    req: &QueryRequest,
    response: &QueryResponse,
    consumed: usize,
    total: usize,
) {
    if let QueryRequest::Lookup { offset, .. } | QueryRequest::Sources { offset, .. } = req {
        let (route, opts) = options(req);
        if let Some(next) = if consumed < total {
            Some(offset + consumed)
        } else {
            response.next_offset
        } {
            let mut o = opts;
            o["offset"] = json!(next);
            r.buttons.push(action("Next", &route, o));
        }
    }
}
fn friendly_field(key: &str) -> String {
    match key {
        "ability_1" => "First ability".into(),
        "ability_2" => "Second ability".into(),
        "effect_or_ability" => "What it does".into(),
        "unlock_requirements" | "requirements" => "How to unlock".into(),
        _ => {
            let text = key.replace('_', " ");
            let mut chars = text.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    }
}
fn detail_navigation(r: &mut Reply, response: &QueryResponse) {
    if response.candidates.len() == 1 {
        let c = &response.candidates[0];
        r.buttons
            .push(action("Overview", "lookup", json!({"name":c.id})));
        r.buttons
            .push(action("Wiki sources", "sources", json!({"name":c.id})));
    }
}
pub fn render(request: &QueryRequest, response: &QueryResponse) -> Reply {
    let req = response.navigation_request.as_ref().unwrap_or(request);
    if matches!(req, QueryRequest::Status {}) {
        let mut r = base("Dandy’s World guide", &response.message);
        let mut browse = action("Find a character or item", "search", json!({}));
        browse.prompt = Some(Prompt {
            label: "What are you looking for?".into(),
            option: "query".into(),
            placeholder: "Pebble, Bone, or Research".into(),
            max_length: 100,
        });
        r.buttons.push(browse);
        return finish(r);
    }

    if matches!(req, QueryRequest::Sources { .. }) && !response.sources.is_empty() {
        let mut r = base(
            "Wiki sources",
            "These links open the wiki versions used for this answer.",
        );
        let mut count = 0;
        for s in &response.sources {
            let mut next = r.clone();
            refs(&mut next, std::slice::from_ref(s));
            if !fits(&next) {
                break;
            }
            r = next;
            count += 1;
        }
        if count == 0 {
            r.card.description =
                "The source link cannot be displayed safely. Please ask the bot owner to check it."
                    .into();
        } else {
            pages(&mut r, req, response, count, response.sources.len());
        }
        return finish(r);
    }
    if response.answer_blocks.is_empty() {
        let (title, desc) = match response.status.as_str() {
            "needs_clarification" => ("Which one did you mean?", "Choose a match below."),
            "not_found" if !response.candidates.is_empty() => (
                "Did you mean one of these?",
                "I couldn’t find an exact match. Choose a result below.",
            ),
            "not_found" => (
                "I couldn’t find that yet",
                "Try a character, item, or game topic, such as Pebble or Research.",
            ),
            "unsupported_query" => (
                "Let’s try another way",
                "Try “What does Poppy do?” or “How does research work?” You can also search for a name.",
            ),
            _ => ("Search results", "Choose a result to learn more."),
        };
        let mut r = base(title, desc);
        if let QueryRequest::Compare { .. } = req {
            r.card.title = if response.selection_option.is_none() {
                "Let’s adjust this comparison".into()
            } else {
                "Which one did you mean?".into()
            };
            if response.selection_option.is_none() {
                r.card.description = "Choose two of the same type, such as two Toons, and a detail they share. Different situations may not be comparable.".into();
                for (label, option) in [
                    ("Change first name", "left"),
                    ("Change second name", "right"),
                    ("Choose a detail", "field"),
                ] {
                    let (route, mut opts) = options(req);
                    opts.as_object_mut().unwrap().remove(option);
                    r.buttons.push(prompt_action(
                        label,
                        &route,
                        opts,
                        label,
                        option,
                        if option == "field" {
                            "health or speed"
                        } else {
                            "Pebble"
                        },
                    ));
                }
            }
        } else if let QueryRequest::Lookup { name, .. } = req
            && response.candidates.is_empty()
            && response.message.contains("field matches")
        {
            r.card.title = "That detail isn’t in the guide yet".into();
            r.card.description =
                "Try the overview for other details, or open the wiki sources.".into();
            r.buttons
                .push(action("Overview", "lookup", json!({"name": name})));
            r.buttons
                .push(action("Wiki sources", "sources", json!({"name": name})));
        }
        let note = warnings(&response.warnings);
        if !note.is_empty() && rendered_len(&note, true) < 2000 {
            r.card.description.push_str(&format!("\n\n{note}"));
        }

        for c in &response.candidates {
            if matches!(req, QueryRequest::Compare { .. }) && response.selection_option.is_none() {
                continue;
            }
            let (route, mut o) = match req {
                QueryRequest::Lookup { .. }
                | QueryRequest::Sources { .. }
                | QueryRequest::Compare { .. } => options(req),
                _ => ("lookup".into(), json!({})),
            };
            let selection = response.selection_option.as_deref().unwrap_or("name");
            o[selection] = json!(c.id);
            if route != "compare" {
                o["kind"] = serde_json::to_value(c.kind).unwrap();
                o.as_object_mut().unwrap().remove("offset");
            }
            r.choices.push(Choice {
                label: short(&c.name, 80),
                description: serde_json::to_value(c.kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .into(),
                route,
                options: o,
            });
        }
        let mut search = action("Search by name", "search", json!({}));
        search.prompt = Some(Prompt {
            label: "Who or what are you looking for?".into(),
            option: "query".into(),
            placeholder: "Pebble".into(),
            max_length: 100,
        });
        r.buttons.push(search);
        return finish(r);
    }
    let title = if matches!(req, QueryRequest::Compare { .. }) {
        "Side by side".into()
    } else {
        response
            .candidates
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Here’s what the wiki says".into())
    };
    let overview = matches!(req, QueryRequest::Lookup { field: None, .. });
    let mut r = base(&title, &warnings(&response.warnings));
    if overview {
        r.card.description = "Pick a detail below to learn more.".into();
        let mut notes = response.warnings.clone();
        notes.extend(
            response
                .answer_blocks
                .iter()
                .flat_map(|b| b.warnings.clone()),
        );
        let note = warnings(&notes);
        if !note.is_empty() {
            r.card.description.push_str(&format!("\n{note}"));
        }
    }
    let group_size = if matches!(req, QueryRequest::Compare { .. }) {
        2
    } else {
        1
    };
    let mut consumed = 0;
    for group in response.answer_blocks.chunks(group_size) {
        if overview && r.card.fields.len() >= 3 {
            break;
        }
        if overview
            && group.iter().any(|b| {
                b.state != EvidenceState::Supported
                    || b.text == "The requested fact is not verified for a current answer."
                    || rendered_len(&block_text(b), true) > 350
                    || matches!(b.key.as_str(), "designation" | "gender")
            })
        {
            consumed += group.len();
            continue;
        }
        let mut candidate = r.clone();
        let ids: BTreeSet<_> = group.iter().flat_map(|b| b.source_ids.iter()).collect();
        let sources: Vec<_> = response
            .sources
            .iter()
            .filter(|s| ids.contains(&s.id))
            .cloned()
            .collect();
        for b in group {
            let (label, text) = readable_block(b);
            let name = if group_size == 2 {
                format!(
                    "{} · {label}",
                    response
                        .candidates
                        .iter()
                        .find(|c| c.id == b.entity_id)
                        .map(|c| c.name.as_str())
                        .unwrap_or("Character")
                )
            } else {
                label
            };
            candidate
                .card
                .fields
                .extend(field_parts(name, text, group_size == 2));
        }
        refs(&mut candidate, &sources);
        if ids.len() != sources.len() || !fits(&candidate) {
            if consumed == 0 {
                r.card.description="This detail is too long to show completely here. Open the wiki source below to read it.".into();
                if let Some(s) = sources
                    .iter()
                    .find(|s| !s.title.starts_with("Template:"))
                    .or(sources.first())
                {
                    refs(&mut r, std::slice::from_ref(s));
                }
                if !fits(&r) {
                    r.citations.clear();
                    r.card.description="The source link cannot be displayed safely. Please ask the bot owner to check it.".into();
                }
                consumed = group.len();
            }
            break;
        }
        r = candidate;
        consumed += group.len();
    }
    pages(
        &mut r,
        req,
        response,
        consumed,
        response.answer_blocks.len(),
    );
    detail_navigation(&mut r, response);
    if overview {
        r.buttons
            .retain(|b| b.label != "Overview" && b.label != "Next");
        if let Some(c) = response.candidates.first() {
            r.buttons.insert(
                0,
                action(
                    "More details",
                    "lookup",
                    json!({"name": c.id, "field": "all details"}),
                ),
            );
        }
        if r.card.fields.is_empty() {
            r.card.description =
                "Choose a detail below to read about this. Some details may still need checking."
                    .into();
        }
    }
    let mut keys = BTreeSet::new();
    for block in &response.answer_blocks {
        if !keys.insert(block.key.clone()) || r.choices.len() == 25 {
            continue;
        }
        let (route, mut opts) = if matches!(req, QueryRequest::Compare { .. }) {
            options(req)
        } else {
            ("lookup".into(), json!({"name": block.entity_id}))
        };
        opts["field"] = json!(block.key);
        opts.as_object_mut().unwrap().remove("offset");
        r.choices.push(Choice {
            label: short(&readable_block(block).0, 80),
            description: "Read this detail".into(),
            route,
            options: opts,
        });
    }

    if matches!(req, QueryRequest::Compare { .. })
        || (r.card.fields.is_empty() && matches!(req, QueryRequest::Lookup { .. }))
    {
        let (route, mut opts) = options(req);
        opts.as_object_mut().unwrap().remove("field");
        opts.as_object_mut().unwrap().remove("offset");
        let mut a = action("Choose a detail", &route, opts);
        a.prompt = Some(Prompt {
            label: "What would you like to see?".into(),
            option: "field".into(),
            placeholder: "health, speed, or abilities".into(),
            max_length: 100,
        });
        r.buttons.push(a);
    }
    finish(r)
}
