//! Explicit opt-in; synthetic read-only tool, at most three paid model requests.
use oracle_gemini_contract::*;
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn main() {
    if let Err(error) = run() {
        eprintln!("P5 live probe did not pass: {error}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("ORACLE_GEMINI_LIVE").as_deref() != Ok("I_AUTHORIZE_3_REQUESTS") {
        return Err(
            "set ORACLE_GEMINI_LIVE=I_AUTHORIZE_3_REQUESTS to authorize this bounded probe".into(),
        );
    }
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| "GEMINI_API_KEY is absent")?;
    if key.trim().is_empty() {
        return Err("GEMINI_API_KEY is empty".into());
    }
    let profile_name =
        std::env::var("ORACLE_GEMINI_PROFILE").unwrap_or_else(|_| "stable-v1-3.7".into());
    let selected = profile(&profile_name).ok_or("unknown explicit Gemini profile")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let prompt = "Call oracle_probe exactly twice, sequentially in separate turns. First use round=1, prior=none. Read its receipt and then use round=2, prior equal to that exact receipt. After the second result, summarize completion briefly. Do not call any other function.";
    let mut session = Session::new(selected.model, prompt, vec![test_tool()])?;
    let mut receipts = Vec::<String>::new();
    let mut records = Vec::new();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    for round in 0..3 {
        let request = session.request(if round < 2 { "any" } else { "none" })?;
        let response = client
            .post(selected.endpoint)
            .header("x-goog-api-key", &key)
            .header("Content-Type", "application/json")
            .body(request.clone())
            .send()
            .map_err(|_| "HTTP request failed (details suppressed to protect credentials)")?;
        let status = response.status();
        let mut body = String::new();
        response
            .take(2_097_153)
            .read_to_string(&mut body)
            .map_err(|_| "response read failed")?;
        if body.len() > 2_097_152 {
            return Err("response exceeds 2 MiB probe bound".into());
        }
        if !status.is_success() {
            let summary = safe_http_error(status.as_u16(), &body, &[&key]);
            let failure = json!({"gate":"P5 live two tool rounds","passed":false,
                "endpoint":selected.endpoint,"configured_model":selected.model,"api_version":selected.api_version,
                "request_number":round+1,"profile":selected.name,"source_sha256":source_hashes(),"openapi_sha256":digest(selected.openapi.as_bytes()),
                "request_sha256":digest(request.as_bytes()),"diagnostic":summary,
                "recorded_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()});
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("live-failures.jsonl");
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            serde_json::to_writer(&mut file, &failure)?;
            file.write_all(b"\n")?;
            file.sync_data()?;
            return Err(format!(
                "profile {} request rejected; no automatic retry or profile fallback: {}",
                selected.name, summary
            )
            .into());
        }
        let turn = parse_turn(&body)?;
        let returned_model = turn
            .model
            .clone()
            .ok_or("response omitted model identity")?;
        if returned_model.trim_start_matches("models/") != selected.model {
            return Err("returned model differs from configured profile".into());
        }
        let value: Value = serde_json::from_str(&body)?;
        let signatures = value["steps"]
            .as_array()
            .map(|s| s.iter().filter(|s| s.get("signature").is_some()).count())
            .unwrap_or(0);
        records.push(json!({"request_sha256":digest(request.as_bytes()),"response_sha256":digest(body.as_bytes()),"returned_model":returned_model,"status":value["status"],"signature_steps":signatures,"usage":turn.usage}));
        let stop = session.accept(turn)?;
        if round < 2 {
            if stop != Stop::RequiresAction || session.calls().len() != 1 {
                return Err("expected one function call in this tool round".into());
            }
            let call = &session.calls()[0];
            let expected_prior = receipts.last().map(String::as_str).unwrap_or("none");
            if call.arguments["round"] != json!(round + 1)
                || call.arguments["prior"] != expected_prior
            {
                return Err("tool arguments did not preserve round/receipt dependency".into());
            }
            let receipt = digest(format!("{nonce}:{}", round + 1).as_bytes());
            let id = call.id.clone();
            receipts.push(receipt.clone());
            session.results(vec![(
                id,
                json!({"receipt":receipt,"round":round+1}),
                false,
            )])?;
            // Assert raw provider steps, including opaque signatures, occur byte-for-byte in replay.
            let replay = session.request("any")?;
            #[derive(serde::Deserialize)]
            struct RawSteps {
                steps: Vec<Box<serde_json::value::RawValue>>,
            }
            for step in serde_json::from_str::<RawSteps>(&body)?.steps {
                if !replay.contains(step.get()) {
                    return Err("lossy native step replay".into());
                }
            }
        } else if stop != Stop::Completed || receipts.len() != 2 {
            return Err("no verified two-round completion".into());
        }
    }
    if !records
        .iter()
        .take(2)
        .any(|record| record["signature_steps"].as_u64().is_some_and(|n| n > 0))
    {
        return Err("two rounds completed but no opaque signatures observed; signature gate remains unverified".into());
    }
    let report = json!({"gate":"P5 live two tool rounds","passed":true,"endpoint":selected.endpoint,"api_version":selected.api_version,"configured_model":selected.model,
        "source_sha256":source_hashes(),"profile":selected.name,"recorded_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"openapi_sha256":digest(selected.openapi.as_bytes()),"rounds":records,"verified_tool_receipts":receipts.len(),"store":false,"note":"No raw signatures, response text, credentials or private data retained."});
    std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/live-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("P5 live two-round contract passed; metadata written to live-report.json");
    Ok(())
}
