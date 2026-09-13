//! Host-owned compilation, typed input decoding, and opt-in replies.
pub use crate::published_cards::{
    CardButton, CardChoice, CardPresentation, CardPrompt, render_card, render_card_with_images,
};
use oracle_core::{
    Error, ErrorCode, ModuleCommandInput, ModuleCommandOptionType, ModuleCommandRoute,
    ModulePresentation, Result,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const SAFE_INTEGER: i64 = 9_007_199_254_740_991;
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}

/// Compile one subcommand. Absent input metadata preserves the v1 definition.
pub fn compile_route(route: &ModuleCommandRoute) -> Result<Value> {
    let options = match &route.input {
        None => json_option(route.input_required),
        Some(ModuleCommandInput::Json { required }) => json_option(*required),
        Some(ModuleCommandInput::Typed { options }) => {
            let mut names = BTreeSet::new();
            let mut optional = false;
            if options.len() > 25 {
                return Err(invalid());
            }
            let mut result = Vec::new();
            for option in options {
                if !names.insert(&option.name) || (optional && option.required) {
                    return Err(invalid());
                }
                optional |= !option.required;
                let mut value = json!({"name":option.name,"description":option.description,"required":option.required});
                match &option.value_type {
                    ModuleCommandOptionType::String {
                        min_length,
                        max_length,
                        choices,
                    } => {
                        if min_length > max_length
                            || *max_length == 0
                            || *max_length > 6000
                            || choices.len() > 25
                        {
                            return Err(invalid());
                        }
                        value["type"] = json!(3);
                        value["min_length"] = json!(min_length);
                        value["max_length"] = json!(max_length);
                        if !choices.is_empty() {
                            value["choices"] = json!(
                                choices
                                    .iter()
                                    .map(|c| json!({"name":c,"value":c}))
                                    .collect::<Vec<_>>()
                            );
                        }
                    }
                    ModuleCommandOptionType::Integer {
                        min_value,
                        max_value,
                    } => {
                        if min_value > max_value
                            || *min_value < -SAFE_INTEGER
                            || *max_value > SAFE_INTEGER
                        {
                            return Err(invalid());
                        }
                        value["type"] = json!(4);
                        value["min_value"] = json!(min_value);
                        value["max_value"] = json!(max_value);
                    }
                    ModuleCommandOptionType::Boolean => value["type"] = json!(5),
                }
                result.push(value);
            }
            result
        }
    };
    Ok(json!({"type":1,"name":route.name,"description":route.description,"options":options}))
}
fn json_option(required: bool) -> Vec<Value> {
    vec![
        json!({"type":3,"name":"input","description":"Operation input as JSON","required":required,"max_length":6000}),
    ]
}

/// Decode already authenticated Discord option values without coercion.
/// Duplicate wire options must be rejected before constructing this map.
pub fn decode_input(route: &ModuleCommandRoute, options: &Map<String, Value>) -> Result<Value> {
    compile_route(route)?;
    match &route.input {
        Some(ModuleCommandInput::Typed {
            options: descriptors,
        }) => {
            if options
                .keys()
                .any(|name| !descriptors.iter().any(|d| &d.name == name))
            {
                return Err(invalid());
            }
            for descriptor in descriptors {
                let Some(value) = options.get(&descriptor.name) else {
                    if descriptor.required {
                        return Err(invalid());
                    }
                    continue;
                };
                let valid = match &descriptor.value_type {
                    ModuleCommandOptionType::String {
                        min_length,
                        max_length,
                        choices,
                    } => value.as_str().is_some_and(|text| {
                        let length = text.chars().count();
                        length >= *min_length as usize
                            && length <= *max_length as usize
                            && (choices.is_empty() || choices.iter().any(|c| c == text))
                    }),
                    ModuleCommandOptionType::Integer {
                        min_value,
                        max_value,
                    } => value.as_i64().is_some_and(|n| {
                        n >= *min_value
                            && n <= *max_value
                            && (-SAFE_INTEGER..=SAFE_INTEGER).contains(&n)
                    }),
                    ModuleCommandOptionType::Boolean => value.is_boolean(),
                };
                if !valid {
                    return Err(invalid());
                }
            }
            Ok(Value::Object(options.clone()))
        }
        mode => {
            let required = match mode {
                Some(ModuleCommandInput::Json { required }) => *required,
                _ => route.input_required,
            };
            if options.len() > 1 || options.keys().any(|k| k != "input") {
                return Err(invalid());
            }
            match options.get("input") {
                None if !required => Ok(json!({})),
                Some(Value::String(text)) if text.chars().count() <= 6000 => {
                    serde_json::from_str(text).map_err(|_| invalid())
                }
                _ => Err(invalid()),
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    text: String,
    citations: Vec<Citation>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Citation {
    label: String,
    revision: u64,
}

/// Only an operator-approved HTTPS prefix can construct outgoing citation URLs.
/// Keep this grammar deliberately smaller than a general URL: no credentials,
/// fragment, encoded delimiters, or Markdown delimiters are accepted.
pub fn validate_citation_prefix(prefix: &str) -> Result<()> {
    if prefix.len() > 1024
        || !prefix.ends_with("oldid=")
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~:/?&=".contains(&b))
    {
        return Err(invalid());
    }
    let rest = prefix.strip_prefix("https://").ok_or_else(invalid)?;
    let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
    if authority.is_empty()
        || authority.contains(':')
        || authority.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(invalid());
    }
    let (_, query) = path.split_once('?').ok_or_else(invalid)?;
    if path.contains("//")
        || query.contains('?')
        || query.split('&').filter(|p| p.starts_with("oldid=")).count() != 1
        || query.split('&').next_back() != Some("oldid=")
    {
        return Err(invalid());
    }
    Ok(())
}
fn safe_text(text: &str) -> Result<String> {
    if text.trim().is_empty()
        || text.chars().any(|c| {
            (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return Err(invalid());
    }
    // Plain text supplied by a module never introduces active Markdown, mentions,
    // or auto-linked domains. Fullwidth punctuation remains readable and inert.
    Ok(text
        .chars()
        .map(|c| match c {
            '@' => '＠',
            '.' => '．',
            ':' => '：',
            '<' => '‹',
            '>' => '›',
            '[' => '［',
            ']' => '］',
            '(' => '（',
            ')' => '）',
            '*' => '＊',
            '_' => '＿',
            '`' => '｀',
            '~' => '～',
            '\\' => '＼',
            '#' => '＃',
            '|' => '｜',
            _ => c,
        })
        .collect())
}

/// Return None for legacy routes so callers retain their exact JSON/attachment path.
/// Transport still owns ephemeral delivery, embed suppression, and allowed_mentions.
pub fn render_presentation(
    route: &ModuleCommandRoute,
    result: &Value,
    citation_prefix: Option<&str>,
) -> Result<Option<String>> {
    let Some(ModulePresentation::PlainTextV1 { pointer }) = &route.presentation else {
        return Ok(None);
    };
    if pointer != "/reply" {
        return Err(invalid());
    }
    let reply: Reply =
        serde_json::from_value(result.pointer(pointer).cloned().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if reply.citations.len() > 5 {
        return Err(invalid());
    }
    if let Some(prefix) = citation_prefix {
        validate_citation_prefix(prefix)?;
    }
    let mut rendered = safe_text(&reply.text)?;
    for citation in reply.citations {
        if citation.revision == 0
            || citation.revision > SAFE_INTEGER as u64
            || citation.label.chars().count() > 120
            || citation.label.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let label = safe_text(&citation.label)?;
        let prefix = citation_prefix.ok_or_else(invalid)?;
        rendered.push_str(&format!("\n[{label}]({prefix}{})", citation.revision));
    }
    if rendered.encode_utf16().count() > 2000 {
        return Err(invalid());
    }
    Ok(Some(rendered))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route(input: Value, presentation: Value) -> ModuleCommandRoute {
        serde_json::from_value(json!({"name":"lookup","description":"Look up an entity","operation":"lookup","input":input,"presentation":presentation})).unwrap()
    }
    fn typed() -> ModuleCommandRoute {
        route(
            json!({"kind":"typed","options":[
        {"name":"name","description":"Name","required":true,"type":"string","min_length":1,"max_length":8,"choices":["Pebble","😀"]},
        {"name":"count","description":"Count","required":false,"type":"integer","min_value":0,"max_value":2},
        {"name":"history","description":"History","required":false,"type":"boolean"}]}),
            Value::Null,
        )
    }
    fn plain() -> ModuleCommandRoute {
        route(
            Value::Null,
            json!({"kind":"plain_text_v1","pointer":"/reply"}),
        )
    }
    const PREFIX: &str = "https://example.org/wiki/index.php?oldid=";
    fn render(text: &str, citations: Value) -> Result<Option<String>> {
        render_presentation(
            &plain(),
            &json!({"reply":{"text":text,"citations":citations}}),
            Some(PREFIX),
        )
    }
    #[test]
    fn legacy_definition_and_json_are_unchanged() {
        let r = route(Value::Null, Value::Null);
        assert_eq!(
            compile_route(&r).unwrap(),
            json!({"type":1,"name":"lookup","description":"Look up an entity","options":[{"type":3,"name":"input","description":"Operation input as JSON","required":false,"max_length":6000}]})
        );
        assert_eq!(decode_input(&r, &Map::new()).unwrap(), json!({}));
        assert_eq!(
            decode_input(&r, json!({"input":"[1,true]"}).as_object().unwrap()).unwrap(),
            json!([1, true])
        );
        assert_eq!(
            render_presentation(&r, &json!({"x":1}), None).unwrap(),
            None
        );
        for value in [
            json!({"other":"{}"}),
            json!({"input":{}}),
            json!({"input":"{"}),
        ] {
            assert!(decode_input(&r, value.as_object().unwrap()).is_err());
        }
        let required = route(json!({"kind":"json","required":true}), Value::Null);
        assert!(decode_input(&required, &Map::new()).is_err());
    }
    #[test]
    fn typed_compiler_and_decoder_preserve_values() {
        let r = typed();
        let compiled = compile_route(&r).unwrap();
        assert_eq!(
            compiled["options"][0]["choices"][0],
            json!({"name":"Pebble","value":"Pebble"})
        );
        assert_eq!(compiled["options"][1]["type"], 4);
        assert_eq!(compiled["options"][2]["type"], 5);
        for value in [
            json!({"name":"Pebble","count":2,"history":false}),
            json!({"name":"😀"}),
        ] {
            assert_eq!(decode_input(&r, value.as_object().unwrap()).unwrap(), value);
        }
        for value in [
            json!({}),
            json!({"name":"Other"}),
            json!({"name":"Pebble","unknown":1}),
            json!({"name":"Pebble","count":2.0}),
            json!({"name":"Pebble","count":"2"}),
            json!({"name":"Pebble","count":3}),
            json!({"name":"Pebble","count":9007199254740992u64}),
            json!({"name":"Pebble","history":0}),
            json!({"name":null}),
        ] {
            assert!(
                decode_input(&r, value.as_object().unwrap()).is_err(),
                "{value}"
            );
        }
    }
    #[test]
    fn descriptor_bounds_apply_without_choices_and_use_strict_integers() {
        let r = route(
            json!({"kind":"typed","options":[
                {"name":"text","description":"Text","required":true,"type":"string","min_length":2,"max_length":3},
                {"name":"number","description":"Number","required":true,"type":"integer","min_value":-9007199254740991i64,"max_value":9007199254740991i64}
            ]}),
            Value::Null,
        );
        for value in [
            json!({"text":"😀a","number":9007199254740991i64}),
            json!({"text":"abc","number":-9007199254740991i64}),
        ] {
            assert_eq!(decode_input(&r, value.as_object().unwrap()).unwrap(), value);
        }
        for value in [
            json!({"text":"a","number":1}),
            json!({"text":"abcd","number":1}),
            json!({"text":"ab","number":-9007199254740992i64}),
            json!({"text":"ab","number":1e0}),
        ] {
            assert!(decode_input(&r, value.as_object().unwrap()).is_err());
        }
    }
    #[test]
    fn plain_text_is_inert_and_citations_are_host_constructed() {
        let text = render(
            "@everyone <@123> [go](https://evil.example) **bold**",
            json!([{"label":"[bad] @here","revision":123}]),
        )
        .unwrap()
        .unwrap();
        assert!(!text.contains("@everyone"));
        assert!(!text.contains("https://evil.example"));
        assert!(text.ends_with("[［bad］ ＠here](https://example.org/wiki/index.php?oldid=123)"));
        assert!(
            render_presentation(
                &plain(),
                &json!({"reply":{"text":"ok","citations":[],"channel":"123"}}),
                Some(PREFIX)
            )
            .is_err()
        );
    }
    #[test]
    fn complete_output_limit_uses_utf16_and_never_truncates() {
        assert!(render(&"😀".repeat(1000), json!([])).is_ok());
        assert!(render(&"😀".repeat(1001), json!([])).is_err());
        assert!(render(&"x".repeat(2000), json!([{"label":"Wiki","revision":1}])).is_err());
    }
    #[test]
    fn malformed_replies_and_prefixes_fail_closed() {
        for prefix in [
            "http://example.org/?oldid=",
            "https://example.org@evil.org/?oldid=",
            "https://example.org/?oldid=1&oldid=",
            "https://example.org/)?oldid=",
            "https://example.org/%0a?oldid=",
            "https://example.org/?oldid=#",
            "https://example.org/?notoldid=",
            "https:///x?oldid=",
        ] {
            assert!(validate_citation_prefix(prefix).is_err(), "{prefix}");
        }
        assert!(
            validate_citation_prefix("https://example.org/index.php?title=Page&oldid=").is_ok()
        );
        for text in ["", "   ", "x\0", "x\u{202e}"] {
            assert!(render(text, json!([])).is_err());
        }
        for citations in [
            json!([{"label":"a","revision":0}]),
            json!([{"label":"a","revision":1.0}]),
            json!([{"label":"a","revision":9007199254740992u64}]),
            json!([{"label":"a","revision":1,"url":"https://evil.org"}]),
            json!([{"label":"x".repeat(121),"revision":1}]),
            json!([{"label":"a\nb","revision":1}]),
            json!(vec![json!({"label":"a","revision":1}); 6]),
        ] {
            assert!(render("ok", citations).is_err());
        }
        assert!(
            render_presentation(
                &plain(),
                &json!({"reply":{"text":"ok","citations":[{"label":"a","revision":1}]}}),
                None
            )
            .is_err()
        );
    }
}
