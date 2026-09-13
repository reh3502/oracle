//! Bounded, host-owned rich replies. Modules never supply Discord payloads or URLs.
use oracle_core::{Error, ErrorCode, ModuleCommandRoute, ModulePresentation, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const SAFE_INTEGER: i64 = 9_007_199_254_740_991;
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}

#[derive(Clone, Debug)]
pub struct CardPresentation {
    pub embed: Value,
    pub buttons: Vec<CardButton>,
    pub choices: Vec<CardChoice>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardPrompt {
    pub label: String,
    pub option: String,
    pub placeholder: String,
    pub max_length: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardButton {
    pub label: String,
    pub route: String,
    pub options: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<CardPrompt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardChoice {
    pub label: String,
    pub description: String,
    pub route: String,
    pub options: Map<String, Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    text: String,
    card: Card,
    citations: Vec<Citation>,
    buttons: Vec<CardButton>,
    choices: Vec<CardChoice>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Card {
    title: String,
    description: String,
    fields: Vec<Field>,
    footer: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Field {
    name: String,
    value: String,
    inline: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Citation {
    label: String,
    revision: u64,
}
fn len(s: &str) -> usize {
    s.encode_utf16().count()
}
fn checked(s: &str, max: usize, empty: bool) -> Result<()> {
    if len(s) > max || (!empty && s.trim().is_empty()) || s.chars().any(|c| {
        (c.is_control() && c != '\n') || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200d}' | '\u{feff}')
    }) { return Err(invalid()); }
    Ok(())
}
/// Preserve sentences and numbers; neutralize only markup, mentions and link-like
/// tokens. Invisible separators inserted here cannot be supplied by a module.
fn rich(s: &str, max: usize, empty: bool) -> Result<String> {
    sanitize(s, max, empty, true)
}
fn label(s: &str, max: usize, empty: bool) -> Result<String> {
    sanitize(s, max, empty, false)
}
fn sanitize(s: &str, max: usize, empty: bool, markdown: bool) -> Result<String> {
    checked(s, max, empty)?;
    let mut out = String::new();
    for token in s.split_inclusive(char::is_whitespace) {
        let domain_like = token
            .split('.')
            .collect::<Vec<_>>()
            .windows(2)
            .any(|parts| {
                parts[0].chars().last().is_some_and(char::is_alphanumeric)
                    && parts[1].chars().next().is_some_and(char::is_alphabetic)
            });
        let link_like =
            token.contains("://") || token.contains("www.") || token.contains('@') || domain_like;
        for c in token.chars() {
            match c {
                '@' => out.push_str("@\u{200b}"),
                '<' => out.push('‹'),
                '>' => out.push('›'),
                '.' | ':' | '/' if link_like => {
                    out.push(c);
                    out.push('\u{200b}');
                }
                '\\' | '*' | '_' | '~' | '`' | '[' | ']' | '(' | ')' | '#' | '|' if markdown => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    if len(&out) > max {
        return Err(invalid());
    }
    Ok(out)
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
fn action(route: &str, options: &Map<String, Value>) -> Result<()> {
    if !name(route)
        || options.len() > 25
        || serde_json::to_vec(options).map_err(|_| invalid())?.len() > 8192
    {
        return Err(invalid());
    }
    for (key, value) in options {
        if !name(key) {
            return Err(invalid());
        }
        match value {
            Value::String(s) => checked(s, 6000, true)?,
            Value::Bool(_) => {}
            Value::Number(n)
                if n.as_i64()
                    .is_some_and(|n| (-SAFE_INTEGER..=SAFE_INTEGER).contains(&n)) => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}
/// None means the caller must retain its existing presentation path.
pub fn render_card(
    route: &ModuleCommandRoute,
    result: &Value,
    citation_prefix: Option<&str>,
) -> Result<Option<CardPresentation>> {
    let Some(ModulePresentation::CardV1 { pointer }) = &route.presentation else {
        return Ok(None);
    };
    if pointer != "/reply" {
        return Err(invalid());
    }
    let mut reply: Reply =
        serde_json::from_value(result.pointer(pointer).cloned().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    checked(&reply.text, 2000, false)?;
    if reply.card.fields.len() > 20
        || reply.citations.len() > 5
        || reply.buttons.len() > 5
        || reply.choices.len() > 25
    {
        return Err(invalid());
    }
    if let Some(prefix) = citation_prefix {
        super::published::validate_citation_prefix(prefix)?;
    }
    let title = rich(&reply.card.title, 256, false)?;
    let description = rich(&reply.card.description, 4096, true)?;
    let footer = label(&reply.card.footer, 512, true)?;
    let mut total = len(&title) + len(&description) + len(&footer);
    let mut fields = Vec::new();
    for field in reply.card.fields {
        let name = rich(&field.name, 256, false)?;
        let value = rich(&field.value, 1024, false)?;
        total += len(&name) + len(&value);
        fields.push(json!({"name":name,"value":value,"inline":field.inline}));
    }
    if !reply.citations.is_empty() {
        let prefix = citation_prefix.ok_or_else(invalid)?;
        let mut links = Vec::new();
        for citation in reply.citations {
            if citation.revision == 0 || citation.revision > SAFE_INTEGER as u64 {
                return Err(invalid());
            }
            let label = rich(&citation.label, 120, false)?;
            links.push(format!("[{label}]({prefix}{})", citation.revision));
        }
        let value = links.join(" · ");
        if len(&value) > 1024 || fields.len() >= 20 {
            return Err(invalid());
        }
        total += len("Wiki sources") + len(&value);
        fields.push(json!({"name":"Wiki sources","value":value,"inline":false}));
    }
    if total > 6000 {
        return Err(invalid());
    }
    for button in &mut reply.buttons {
        button.label = label(&button.label, 80, false)?;
        action(&button.route, &button.options)?;
        if let Some(prompt) = &mut button.prompt {
            prompt.label = label(&prompt.label, 45, false)?;
            prompt.placeholder = label(&prompt.placeholder, 100, true)?;
            if !name(&prompt.option)
                || !(1..=200).contains(&prompt.max_length)
                || button.options.contains_key(&prompt.option)
            {
                return Err(invalid());
            }
        }
    }
    for choice in &mut reply.choices {
        choice.label = label(&choice.label, 80, false)?;
        choice.description = label(&choice.description, 100, true)?;
        action(&choice.route, &choice.options)?;
    }
    let mut embed = json!({"title":title,"color":0x9678D3,"fields":fields});
    if !description.is_empty() {
        embed["description"] = json!(description);
    }
    if !footer.is_empty() {
        embed["footer"] = json!({"text":footer});
    }
    Ok(Some(CardPresentation {
        embed,
        buttons: reply.buttons,
        choices: reply.choices,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route() -> ModuleCommandRoute {
        serde_json::from_value(json!({"name":"lookup","description":"Lookup","operation":"lookup","presentation":{"kind":"card_v1","pointer":"/reply"}})).unwrap()
    }
    fn reply() -> Value {
        json!({"reply":{"text":"Pebble has 2 hearts.","card":{"title":"Pebble","description":"Health: 2 hearts. Speed: 1.5.","fields":[],"footer":"Wiki contributors · CC BY-SA 3.0"},"citations":[],"buttons":[],"choices":[]}})
    }
    fn render(v: &Value) -> Result<Option<CardPresentation>> {
        render_card(&route(), v, Some("https://example.org/index.php?oldid="))
    }
    #[test]
    fn sentences_keep_normal_punctuation() {
        assert_eq!(
            render(&reply()).unwrap().unwrap().embed["description"],
            "Health: 2 hearts. Speed: 1.5."
        );
    }
    #[test]
    fn hostile_content_is_inert_and_links_are_host_owned() {
        let mut r = reply();
        r["reply"]["card"]["description"] =
            json!("@everyone <@123> **bold** [click](https://evil.example) evil.example");
        let p = render(&r).unwrap().unwrap();
        let text = p.embed["description"].as_str().unwrap();
        assert!(!text.contains("@everyone"));
        assert!(!text.contains("<@"));
        assert!(!text.contains("https://evil.example"));
        assert!(!text.contains("**bold**"));
        assert!(!text.contains("evil.example"));
        r["reply"]["citations"] = json!([{"label":"Pebble","revision":123}]);
        assert!(
            render(&r)
                .unwrap()
                .unwrap()
                .embed
                .to_string()
                .contains("https://example.org/index.php?oldid=123")
        );
        assert!(render_card(&route(), &r, None).is_err());
        r["reply"]["citations"][0]["url"] = json!("https://evil.example");
        assert!(render(&r).is_err());
    }
    #[test]
    fn rejects_controls_bidi_and_bad_references() {
        for s in [
            "bad\ttext",
            "bad\u{202e}text",
            "bad\u{2066}text",
            "bad\u{0000}text",
            "bad\u{200b}text",
        ] {
            let mut r = reply();
            r["reply"]["card"]["description"] = json!(s);
            assert!(render(&r).is_err());
        }
        for n in [0u64, 9_007_199_254_740_992] {
            let mut r = reply();
            r["reply"]["citations"] = json!([{"label":"Wiki","revision":n}]);
            assert!(render(&r).is_err());
        }
    }
    #[test]
    fn utf16_and_aggregate_limits_fail_without_truncating() {
        let mut r = reply();
        r["reply"]["card"]["title"] = json!("😀".repeat(128));
        assert!(render(&r).is_ok());
        r["reply"]["card"]["title"] = json!("😀".repeat(129));
        assert!(render(&r).is_err());
        let mut r = reply();
        r["reply"]["card"]["description"] = json!("x".repeat(4096));
        r["reply"]["card"]["fields"] = json!([{"name":"One","value":"x".repeat(1024),"inline":false},{"name":"Two","value":"x".repeat(1024),"inline":false}]);
        assert!(render(&r).is_err());
    }
    #[test]
    fn actions_are_typed_bounded_and_modal_is_explicit() {
        let mut r = reply();
        r["reply"]["buttons"] = json!([{"label":"Ask again","route":"ask","options":{"kind":"toon","count":9007199254740991i64,"history":false},"prompt":{"label":"Your question","option":"question","placeholder":"How fast is Pebble?","max_length":200}}]);
        assert!(render(&r).is_ok());
        r["reply"]["buttons"][0]["options"]["count"] = json!(9007199254740992i64);
        assert!(render(&r).is_err());
        r["reply"]["buttons"][0]["options"]["count"] = json!(1.0);
        assert!(render(&r).is_err());
        r["reply"]["buttons"][0]["options"]["count"] = json!({"nested":true});
        assert!(render(&r).is_err());
        r["reply"]["buttons"][0]["options"]["count"] = json!(1);
        r["reply"]["buttons"][0]["prompt"]["max_length"] = json!(201);
        assert!(render(&r).is_err());
        r["reply"]["buttons"][0]["prompt"]["max_length"] = json!(200);
        r["reply"]["buttons"][0]["options"]["question"] = json!("hidden");
        assert!(render(&r).is_err());
    }
    #[test]
    fn collection_and_serialized_action_limits_are_enforced() {
        let mut r = reply();
        r["reply"]["buttons"] = json!(vec![
            json!({"label":"Pick","route":"lookup","options":{}});
            6
        ]);
        assert!(render(&r).is_err());
        r["reply"]["buttons"] = json!([]);
        r["reply"]["choices"] = json!(vec![
            json!({"label":"Pick","description":"A toon","route":"lookup","options":{}});
            26
        ]);
        assert!(render(&r).is_err());
        r["reply"]["choices"] = json!([]);
        r["reply"]["citations"] = json!(vec![json!({"label":"Wiki","revision":1}); 6]);
        assert!(render(&r).is_err());
        r["reply"]["citations"] = json!([]);
        r["reply"]["buttons"] = json!([{"label":"Pick","route":"lookup","options":{"one":"x".repeat(5000),"two":"x".repeat(5000)}}]);
        assert!(render(&r).is_err());
        r["reply"]["buttons"][0]["options"] = json!({});
        r["reply"]["buttons"][0]["route"] = json!("/admin");
        assert!(render(&r).is_err());
    }
    #[test]
    fn plain_component_labels_do_not_display_markdown_escapes() {
        let mut r = reply();
        r["reply"]["choices"] = json!([{"label":"Dandy (Toon)","description":"Pick Dandy's toon.","route":"lookup","options":{"name":"Dandy"}}]);
        let p = render(&r).unwrap().unwrap();
        assert_eq!(p.choices[0].label, "Dandy (Toon)");
        assert_eq!(p.choices[0].description, "Pick Dandy's toon.");
    }
}
