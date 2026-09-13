//! Opt-in corpus check against actual module replies rather than hand-built cards.
//! Run with ORACLE_CARD_REPLIES=/absolute/replies.jsonl cargo test -p
//! oracle-operations --test card_corpus -- --ignored --nocapture.
use oracle_core::ModuleCommandRoute;
use oracle_operations::published::render_card_with_images;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusCase {
    case: String,
    result: Value,
}

fn utf16(value: &Value, max: usize, case: &str, field: &str) -> usize {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("{case}: {field} is not text"));
    let length = text.encode_utf16().count();
    assert!(
        length <= max,
        "{case}: {field} uses {length} UTF-16 units; maximum {max}"
    );
    length
}

#[test]
#[ignore = "requires actual module NDJSON replies in ORACLE_CARD_REPLIES"]
fn every_corpus_reply_renders_within_discord_limits() {
    let path = std::env::var_os("ORACLE_CARD_REPLIES")
        .expect("set ORACLE_CARD_REPLIES to the actual module reply NDJSON file");
    let file = File::open(&path).expect("open ORACLE_CARD_REPLIES");
    let route: ModuleCommandRoute = serde_json::from_value(json!({
        "name":"lookup", "description":"Corpus lookup", "operation":"lookup",
        "presentation":{"kind":"card_v1", "pointer":"/reply"}
    }))
    .unwrap();
    let mut case_count = 0;
    let mut failures = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.unwrap_or_else(|e| panic!("line {}: {e}", index + 1));
        let row: CorpusCase = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("line {}: invalid corpus record: {e}", index + 1));
        assert!(
            !row.case.trim().is_empty(),
            "line {}: empty case name",
            index + 1
        );
        case_count += 1;
        let rendered = match render_card_with_images(
            &route,
            &row.result,
            Some("https://dandys-world-robloxhorror.fandom.com/index.php?oldid="),
            Some("https://static.wikia.nocookie.net/dandys-world-robloxhorror/images/"),
        ) {
            Ok(Some(card)) => card,
            other => {
                failures.push(format!("{}: {other:?}", row.case));
                continue;
            }
        };
        let embed = &rendered.embed;
        let mut total = utf16(&embed["title"], 256, &row.case, "title");
        if let Some(description) = embed.get("description") {
            total += utf16(description, 4096, &row.case, "description");
        }
        if let Some(footer) = embed.get("footer") {
            total += utf16(&footer["text"], 512, &row.case, "footer");
        }
        let fields = embed["fields"].as_array().expect("host emits fields array");
        assert!(fields.len() <= 20, "{}: too many embed fields", row.case);
        for field in fields {
            total += utf16(&field["name"], 256, &row.case, "field name");
            total += utf16(&field["value"], 1024, &row.case, "field value");
        }
        assert!(
            total <= 6000,
            "{}: embed uses {total} UTF-16 units",
            row.case
        );
        assert!(
            rendered.buttons.len() <= 5,
            "{}: too many buttons",
            row.case
        );
        assert!(
            rendered.choices.len() <= 25,
            "{}: too many choices",
            row.case
        );
    }
    assert!(
        case_count >= 100,
        "expected at least 100 actual module cases; got {}",
        case_count
    );
    assert!(
        failures.is_empty(),
        "{} of {} cases failed host rendering:\n{}",
        failures.len(),
        case_count,
        failures.join("\n")
    );
    println!(
        "{} actual module replies passed host card rendering and Discord UTF-16 bounds",
        case_count
    );
}
