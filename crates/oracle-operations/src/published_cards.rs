//! Bounded, host-owned rich replies. Modules never supply raw Discord payloads.
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
    #[serde(default)]
    image: Option<CardImage>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CardImage {
    url: String,
    revision: u64,
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
    render_card_with_images(route, result, citation_prefix, None)
}
/// Rich replies may include a thumbnail only under an explicit operator policy.
/// The host forwards the validated URL to Discord; it never fetches the image.
pub fn render_card_with_images(
    route: &ModuleCommandRoute,
    result: &Value,
    citation_prefix: Option<&str>,
    image_prefix: Option<&str>,
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
    if let Some(image) = &reply.image {
        oracle_core::validate_module_image_url(&image.url, image_prefix.ok_or_else(invalid)?)?;
        if image.revision == 0 || image.revision > SAFE_INTEGER as u64 {
            return Err(invalid());
        }
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
    if !reply.citations.is_empty() || reply.image.is_some() {
        let prefix = citation_prefix.ok_or_else(invalid)?;
        let mut links = Vec::new();
        for citation in reply.citations {
            if citation.revision == 0 || citation.revision > SAFE_INTEGER as u64 {
                return Err(invalid());
            }
            let label = rich(&citation.label, 120, false)?;
            links.push(format!("[{label}]({prefix}{})", citation.revision));
        }
        if let Some(image) = &reply.image {
            links.push(format!("[Image]({prefix}{})", image.revision));
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
    if let Some(image) = reply.image {
        embed["thumbnail"] = json!({"url":image.url});
    }
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
    const IMAGE_PREFIX: &str = "https://cdn.example.test/wiki/images/";
    fn with_image(url: &str) -> Value {
        let mut value = reply();
        value["reply"]["image"] = json!({"url":url,"revision":456});
        value
    }
    fn image_render(value: &Value) -> Result<Option<CardPresentation>> {
        render_card_with_images(
            &route(),
            value,
            Some("https://example.org/index.php?oldid="),
            Some(IMAGE_PREFIX),
        )
    }
    #[test]
    fn approved_images_are_preserved_and_credited_in_the_same_source_field() {
        for path in [
            "a/a1/Pebble.png",
            "a/a1/Pebble.PNG/revision/latest?cb=20240806022953",
            "a/a1/Pebble%20Render%C3%A9.png/revision/latest/scale-to-width-down/256?cb=20240806022953",
            "5/56/Get_To_Where%3F.png/revision/latest/scale-to-width-down/256?cb=20260515191029",
        ] {
            let url = format!("{IMAGE_PREFIX}{path}");
            let mut value = with_image(&url);
            value["reply"]["citations"] = json!([{"label":"Pebble","revision":123}]);
            let rendered = image_render(&value).unwrap().unwrap();
            assert_eq!(rendered.embed["thumbnail"]["url"], url);
            let fields = rendered.embed["fields"].as_array().unwrap();
            assert_eq!(fields.len(), 1);
            let text = fields[0]["value"].as_str().unwrap();
            assert!(text.contains("[Pebble](https://example.org/index.php?oldid=123)"));
            assert!(text.contains("[Image](https://example.org/index.php?oldid=456)"));
            assert!(!text.contains(&url));
        }
        let old = render(&reply()).unwrap().unwrap();
        let current = image_render(&reply()).unwrap().unwrap();
        assert_eq!(old.embed, current.embed);
        assert!(old.embed.get("thumbnail").is_none());
    }
    #[test]
    fn images_require_operator_permission_and_safe_numeric_attribution() {
        let mut value = with_image(&format!("{IMAGE_PREFIX}a.png"));
        assert!(render(&value).is_err());
        assert!(render_card_with_images(&route(), &value, None, Some(IMAGE_PREFIX)).is_err());
        for revision in [json!(0), json!(-1), json!(9007199254740992u64), json!(true)] {
            value["reply"]["image"]["revision"] = revision;
            assert!(image_render(&value).is_err());
        }
        value["reply"]["image"]["revision"] = json!(456);
        value["reply"]["image"]["source_url"] = json!("https://evil.test");
        assert!(image_render(&value).is_err());
    }
    #[test]
    fn hostile_image_paths_and_prefixes_fail_without_url_disclosure() {
        for prefix in [
            "http://cdn.example.test/wiki/",
            "https://cdn.example.test/",
            "https://cdn.example.test/wiki",
            "https://user@cdn.example.test/wiki/",
            "https://cdn.example.test:443/wiki/",
            "https://cdn.example.test/wiki/../",
            "https://cdn.example.test/wiki/%2e/",
            "https://cdn.example.test/wiki/?x=/",
            "https://cdn.example.test/wiki/#/",
        ] {
            assert!(
                oracle_core::validate_module_image_prefix(prefix).is_err(),
                "{prefix}"
            );
        }
        for path in [
            "../a.png",
            "%2e%2e/a.png",
            "%252e%252e/a.png",
            "x%2fa.png",
            "x%5ca.png",
            "a.png%00",
            "a.png%0a",
            "a%E2%80%AE.png",
            "a.png?url=https://evil.test",
            "a.png?cb=1&cb=2",
            "a.png?cb=",
            "a.png?cb=abc",
            "a.png#fragment",
            "a.svg",
            "a.html",
            "a.png/redirect/evil",
            "a.png/revision/latest/scale-to-width-down/99999",
            "a.png//revision/latest",
            "a.png/revision/latest?cb=123#evil",
            "a%GG.png",
            "a%FF.png",
        ] {
            let url = format!("{IMAGE_PREFIX}{path}");
            let error = image_render(&with_image(&url)).unwrap_err();
            assert!(!format!("{error:?}").contains(&url));
        }
        for url in [
            "https://evil.test/a.png",
            "https://cdn.example.test.evil.test/wiki/images/a.png",
            "https://cdn.example.test/wiki/images-other/a.png",
            "https://cdn.example.test/wiki/images@evil.test/a.png",
        ] {
            assert!(image_render(&with_image(url)).is_err());
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrivateCardPresentation {
    pub embed: Value,
    pub controls: oracle_core::PrivateCardV2,
}
/// Protocol 1.2 controls have their own strict decoder; CardV1 stays unchanged.
pub fn render_private_card(result: &Value) -> Result<PrivateCardPresentation> {
    let mut controls: oracle_core::PrivateCardV2 =
        serde_json::from_value(result.get("reply").cloned().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if controls.buttons.len() > 15 || controls.choices.len() > 25 || controls.card.fields.len() > 20
    {
        return Err(invalid());
    }
    if controls
        .card
        .fields
        .iter()
        .map(|f| f.members.len())
        .sum::<usize>()
        > 9
    {
        return Err(invalid());
    }
    for f in &controls.card.fields {
        for member in &f.members {
            checked(&member.prefix, 128, true)?;
            checked(&member.suffix, 128, true)?;
        }
    }
    let title = rich(&controls.card.title, 256, false)?;
    let description = rich(&controls.card.description, 4096, true)?;
    let footer = label(&controls.card.footer, 512, true)?;
    let mut total = len(&title) + len(&description) + len(&footer);
    let mut fields = Vec::new();
    for f in &controls.card.fields {
        let name = rich(&f.name, 256, false)?;
        let value = rich(&f.value, 1024, !f.members.is_empty())?;
        total += len(&name) + len(&value);
        fields.push(json!({"name":name,"value":value,"inline":f.inline}));
    }
    if total > 6000 {
        return Err(invalid());
    }
    controls.select_placeholder = label(&controls.select_placeholder, 150, true)?;
    for b in &mut controls.buttons {
        b.label = label(&b.label, 80, false)?;
        private_action(&b.operation, &b.input)?;
        if let Some(p) = &mut b.prompt {
            p.label = label(&p.label, 45, false)?;
            p.placeholder = label(&p.placeholder, 100, true)?;
            if !name(&p.option)
                || !(1..=200).contains(&p.max_length)
                || b.input.contains_key(&p.option)
            {
                return Err(invalid());
            }
            // Prompt fields obey the same flat-data and actor-authority rules as pinned inputs.
            private_action(
                &b.operation,
                &Map::from_iter([(p.option.clone(), json!("value"))]),
            )?;
            if let Some(select) = &mut p.select {
                select.label = label(&select.label, 45, false)?;
                if !name(&select.option)
                    || select.option == p.option
                    || b.input.contains_key(&select.option)
                    || !(1..=25).contains(&select.choices.len())
                {
                    return Err(invalid());
                }
                let mut seen = std::collections::HashSet::new();
                for choice in &mut select.choices {
                    choice.label = label(&choice.label, 100, false)?;
                    checked(&choice.value, 100, false)?;
                    if !seen.insert(choice.value.clone()) {
                        return Err(invalid());
                    }
                    private_action(
                        &b.operation,
                        &Map::from_iter([(select.option.clone(), json!(choice.value))]),
                    )?;
                }
            }
        }
    }
    for c in &mut controls.choices {
        c.label = label(&c.label, 80, false)?;
        c.description = label(&c.description, 100, true)?;
        private_action(&c.operation, &c.input)?;
    }
    let embed = json!({"title":title,"description":description,"footer":{"text":footer},"fields":fields,"color":0x9678D3});
    Ok(PrivateCardPresentation { embed, controls })
}
fn private_action(operation: &str, input: &Map<String, Value>) -> Result<()> {
    // Actions are flat typed data. Member authority only comes from the host envelope.
    action(operation, input)?;
    if input.keys().any(|key| {
        matches!(
            key.as_str(),
            "actor"
                | "actor_id"
                | "user_id"
                | "guild_id"
                | "channel_id"
                | "interaction_id"
                | "permissions"
                | "roles"
                | "member"
        )
    }) {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod private_tests {
    use super::*;
    fn card() -> Value {
        json!({"reply":{"card":{"title":"Run","description":"Choose toons"},"choices":[],"buttons":[{"label":"Count","operation":"run_ui","input":{"id":"ABCD1234","expected_revision":1},"prompt":{"option":"count","label":"Places","max_length":1}}]}})
    }
    #[test]
    fn v2_has_separate_strict_bounds_and_no_actor_authority() {
        assert!(render_private_card(&card()).is_ok());
        for key in [
            "actor",
            "actor_id",
            "user_id",
            "guild_id",
            "channel_id",
            "interaction_id",
            "permissions",
            "roles",
            "member",
        ] {
            let mut v = card();
            v["reply"]["buttons"][0]["input"][key] = json!("forged");
            assert!(render_private_card(&v).is_err());
        }
        let mut v = card();
        v["reply"]["buttons"][0]["input"]["nested"] = json!({"actor":"forged"});
        assert!(render_private_card(&v).is_err());
        let mut v = card();
        let b = v["reply"]["buttons"][0].clone();
        v["reply"]["buttons"] = json!(vec![b; 16]);
        assert!(render_private_card(&v).is_err());
        let mut v = card();
        v["reply"]["buttons"][0]["input"]["count"] = json!("2");
        assert!(render_private_card(&v).is_err());
    }
    #[test]
    fn modal_selects_are_bounded_unique_and_cannot_supply_authority() {
        let mut v = card();
        v["reply"]["buttons"][0]["prompt"]["select"] = json!({"option":"toon","label":"Toon","choices":[{"label":"Pebble","value":"toon:pebble"}]});
        assert!(render_private_card(&v).is_ok());
        for key in ["count", "id", "actor_id", "permissions", "member"] {
            let mut bad = v.clone();
            bad["reply"]["buttons"][0]["prompt"]["select"]["option"] = json!(key);
            assert!(render_private_card(&bad).is_err(), "{key}");
        }
        for size in [0, 2, 26] {
            let mut bad = v.clone();
            bad["reply"]["buttons"][0]["prompt"]["select"]["choices"] =
                json!(vec![json!({"label":"Pebble","value":"toon:pebble"}); size]);
            assert!(render_private_card(&bad).is_err());
        }
        v["reply"]["buttons"][0]["prompt"]["option"] = json!("actor_id");
        assert!(render_private_card(&v).is_err());
    }
    #[test]
    fn v2_neutralizes_mentions_and_rejects_raw_discord_fields() {
        let mut v = card();
        v["reply"]["card"]["description"] = json!("@everyone <@123> https://evil.test");
        let text = render_private_card(&v).unwrap().embed["description"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!text.contains("@everyone"));
        assert!(!text.contains("<@123>"));
        v["reply"]["components"] = json!([]);
        assert!(render_private_card(&v).is_err());
    }
}

/// Resolve typed member references using names supplied by the host, then run all
/// ordinary rendering bounds and sanitization again. Missing members expose no ID.
pub fn resolve_private_card_names(
    card: &mut PrivateCardPresentation,
    names: &std::collections::BTreeMap<oracle_core::UserId, String>,
) -> Result<()> {
    for (index, field) in card.controls.card.fields.iter_mut().enumerate() {
        let mut value = card.embed["fields"][index]["value"]
            .as_str()
            .ok_or_else(invalid)?
            .to_owned();
        for member in field.members.drain(..) {
            let name = names
                .get(&member.user_id)
                .map(String::as_str)
                .unwrap_or("Member unavailable");
            let safe_name = rich(name, 256, false).unwrap_or_else(|_| "Member unavailable".into());
            if !value.is_empty() {
                value.push('\n');
            }
            value.push_str(&rich(&member.prefix, 256, true)?);
            value.push_str(&safe_name);
            value.push_str(&rich(&member.suffix, 256, true)?);
        }
        if len(&value) > 1024 {
            return Err(invalid());
        }
        card.embed["fields"][index]["value"] = json!(value);
    }
    for choice in &mut card.controls.choices {
        if let Some(id) = choice.member_id.take() {
            let name = names
                .get(&id)
                .map(String::as_str)
                .unwrap_or("Member unavailable");
            choice.label = label(name, 80, false).unwrap_or_else(|_| "Member unavailable".into());
        }
    }
    let mut total = [
        card.embed["title"].as_str(),
        card.embed["description"].as_str(),
        card.embed["footer"]["text"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(len)
    .sum::<usize>();
    for field in card.embed["fields"].as_array().ok_or_else(invalid)? {
        total += len(field["name"].as_str().ok_or_else(invalid)?)
            + len(field["value"].as_str().ok_or_else(invalid)?);
    }
    if total > 6000 {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod member_name_tests {
    use super::*;
    #[test]
    fn typed_names_are_inert_and_missing_members_never_expose_ids() {
        let value = json!({"reply":{"card":{"title":"Run","description":"Players","fields":[{"name":"Roster","value":"","inline":false,"members":[{"user_id":"123","suffix":" — Poppy"},{"user_id":"456"}]}]},"choices":[{"label":"Player","member_id":"123","description":"Remove player","operation":"run_ui","input":{"target":"123"}}]}});
        let mut card = render_private_card(&value).unwrap();
        resolve_private_card_names(
            &mut card,
            &std::collections::BTreeMap::from([(
                "123".parse().unwrap(),
                "@everyone **name**".into(),
            )]),
        )
        .unwrap();
        let body = card.embed.to_string();
        assert!(!body.contains("123"));
        assert!(!body.contains("456"));
        assert!(!body.contains("@everyone"));
        assert!(body.contains("Member unavailable"));
        assert!(body.contains("Poppy"));
        assert!(!card.controls.choices[0].label.contains("@everyone"));
    }
    #[test]
    fn member_expansion_cannot_exceed_embed_field_budget() {
        let value = json!({"reply":{"card":{"title":"Run","description":"Players","fields":[{"name":"Roster","value":"x".repeat(1024),"inline":false,"members":[{"user_id":"123"}]}]}}});
        let mut card = render_private_card(&value).unwrap();
        assert!(resolve_private_card_names(&mut card, &Default::default()).is_err());
    }
}
