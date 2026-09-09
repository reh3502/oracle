use oracle_gemini_contract::*;
use serde_json::{Value, json};
fn session() -> Session {
    Session::new(MODEL, "Run two test rounds", vec![test_tool()]).unwrap()
}
fn first() -> Turn {
    parse_turn(include_str!("../fixtures/round1.json")).unwrap()
}
#[test]
fn two_rounds_preserve_native_steps_bytes_and_signatures() {
    let mut s = session();
    s.accept(first()).unwrap();
    assert_eq!(s.request("any").unwrap_err(), Error::Pending);
    s.results(vec![(
        "call-1".into(),
        json!({"receipt":"receipt-1"}),
        false,
    )])
    .unwrap();
    let second = s.request("any").unwrap();
    assert!(second.contains(
        r#"{"type":"thought", "signature":"AAEC/w==", "future_metadata":{"number":1.2300e+20}}"#
    ));
    let v: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v["input"][3]["call_id"], "call-1");
    assert_eq!(v["input"][3]["result"]["receipt"], "receipt-1");
    assert_eq!(v["store"], false);
    assert_eq!(v["stream"], false);
    assert_eq!(v["tools"], json!([test_tool()]));
    assert_eq!(v["system_instruction"], SYSTEM);
    assert!(v.get("previous_interaction_id").is_none());
    s.accept(parse_turn(include_str!("../fixtures/round2.json")).unwrap())
        .unwrap();
    s.results(vec![(
        "call-2".into(),
        json!({"receipt":"receipt-2"}),
        false,
    )])
    .unwrap();
    let third = s.request("none").unwrap();
    assert!(third.contains("AAEC/w=="));
    assert!(third.contains("//8AAg=="));
    assert_eq!(
        s.accept(parse_turn(include_str!("../fixtures/completed.json")).unwrap())
            .unwrap(),
        Stop::Completed
    );
    assert_eq!(s.request("none").unwrap_err(), Error::Terminal);
}
#[test]
fn result_batch_is_atomic_and_identity_checked() {
    let mut s = session();
    s.accept(first()).unwrap();
    assert_eq!(
        s.results(vec![("foreign-id".into(), json!({}), false)]),
        Err(Error::Pending)
    );
    assert_eq!(s.calls().len(), 1);
    s.results(vec![("call-1".into(), json!({}), true)]).unwrap();
    let v: Value = serde_json::from_str(&s.request("any").unwrap()).unwrap();
    assert_eq!(v["input"][3]["is_error"], true);
    assert_eq!(s.accept(first()).unwrap_err(), Error::Call);
}
#[test]
fn malformed_or_unsupported_schema_rejected() {
    for schema in [
        json!({"type":"object","properties":{},"required":["missing"]}),
        json!({"type":"string","pattern":".*"}),
        json!({"type":"banana"}),
        json!({"type":"array"}),
        json!({"type":"object","properties":{},"additionalProperties":true}),
        json!({"type":"integer","enum":["1"]}),
    ] {
        assert_eq!(check_schema(&schema), Err(Error::Schema));
    }
}
#[test]
fn malformed_arguments_unknown_tool_duplicate_ids_reject_entire_round() {
    for (mutation, expected) in [
        (json!({"round":"1","prior":"none"}), Error::Arguments),
        (
            json!({"round":1,"prior":"none","guild":"foreign"}),
            Error::Arguments,
        ),
    ] {
        let mut v: Value = serde_json::from_str(include_str!("../fixtures/round1.json")).unwrap();
        v["steps"][1]["arguments"] = mutation;
        let mut s = session();
        assert_eq!(
            s.accept(parse_turn(&v.to_string()).unwrap()).unwrap_err(),
            expected
        );
        assert!(s.calls().is_empty());
        assert!(s.request("any").is_ok());
    }
    let mut v: Value = serde_json::from_str(include_str!("../fixtures/round1.json")).unwrap();
    v["steps"][1]["name"] = json!("unknown");
    assert_eq!(
        session()
            .accept(parse_turn(&v.to_string()).unwrap())
            .unwrap_err(),
        Error::Call
    );
    v["steps"][1]["name"] = json!("oracle_probe");
    let duplicate = v["steps"][1].clone();
    v["steps"].as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        session()
            .accept(parse_turn(&v.to_string()).unwrap())
            .unwrap_err(),
        Error::Call
    );
}
#[test]
fn refusal_prose_is_not_a_completion_receipt_and_missing_usage_is_unknown() {
    let t = parse_turn(include_str!("../fixtures/refusal-prose.json")).unwrap();
    assert_eq!(t.stop, Stop::Completed);
    assert!(t.calls.is_empty());
    assert_eq!(t.usage, None);
    // The published contract has no refusal discriminator. Never guess one from
    // English prose, and never treat provider completion as host goal completion.
    let mut s = session();
    assert_eq!(s.accept(t).unwrap(), Stop::Completed);
    assert!(s.calls().is_empty());
    assert_eq!(
        s.results(vec![("invented-receipt".into(), json!({}), false)]),
        Err(Error::Pending)
    );
    assert_eq!(s.request("none").unwrap_err(), Error::Terminal);
}
#[test]
fn incomplete_failed_cancelled_never_release_tool_calls() {
    let incomplete = include_str!("../fixtures/incomplete.json");
    for status in ["incomplete", "failed", "cancelled"] {
        let t = parse_turn(&incomplete.replace("incomplete", status)).unwrap();
        assert!(t.calls.is_empty());
        let mut s = session();
        s.accept(t).unwrap();
        assert_eq!(s.request("any").unwrap_err(), Error::Terminal);
    }
}
#[test]
fn wire_errors_fail_closed() {
    for raw in [
        "{",
        r#"{"status":"new_status"}"#,
        r#"{"status":"requires_action","steps":[]}"#,
        r#"{"status":"requires_action","steps":[{"type":"function_call","id":"x","name":"oracle_probe","arguments":"{}"}]}"#,
    ] {
        assert!(matches!(parse_turn(raw), Err(Error::Protocol)));
    }
}
#[test]
fn downloaded_contract_exposes_exact_pinned_seams() {
    let v: Value = serde_json::from_str(OPENAPI).unwrap();
    assert_eq!(
        digest(OPENAPI.as_bytes()),
        "3c25941e544ff0d96125faf65983b36152f91e0e6e2c4dc88b051fd687ca3f52"
    );
    assert_eq!(v["info"]["version"], "v1");
    assert!(v["paths"].get("/{api_version}/interactions").is_some());
    let schemas = &v["components"]["schemas"];
    assert_eq!(
        schemas["ThoughtStep"]["properties"]["signature"]["type"],
        "string"
    );
    assert!(
        schemas["ModelOption"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("models/{MODEL}")))
    );
    assert!(
        schemas["FunctionResultStep"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("call_id"))
    );
}

#[test]
fn live_binary_rejects_missing_authorization_or_key_before_network() {
    for authorized in [false, true] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_live"));
        command
            .env_remove("GEMINI_API_KEY")
            .env_remove("ORACLE_GEMINI_LIVE");
        if authorized {
            command.env("ORACLE_GEMINI_LIVE", "I_AUTHORIZE_3_REQUESTS");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(message.contains(if authorized {
            "GEMINI_API_KEY is absent"
        } else {
            "authorize this bounded probe"
        }));
    }
}

#[test]
fn quota_diagnostics_preserve_only_safe_structured_fields() {
    let body=json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"private prose token SECRET123","details":[
        {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier","quotaValue":"0","description":"private prose","quotaDimensions":{"project":"private-project"}}]},
        {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"58.25s"}
    ]}}).to_string();
    let safe = safe_http_error(429, &body, &["SECRET123"]);
    assert_eq!(safe["provider_status"], "RESOURCE_EXHAUSTED");
    assert_eq!(safe["retry_after_seconds"], 58.25);
    assert_eq!(safe["quotas"][0]["limit_value"], 0);
    assert_eq!(
        safe["quotas"][0]["metric"],
        "generativelanguage.googleapis.com/generate_content_free_tier_requests"
    );
    for forbidden in [
        "SECRET123",
        "private prose",
        "private-project",
        "\"message\":",
        "quotaDimensions",
    ] {
        assert!(!safe.to_string().contains(forbidden));
    }
}
#[test]
fn error_diagnostics_suppress_echoed_key_bad_shapes_and_unknown_content() {
    let body=json!({"error":{"status":"SECRET123","message":"SECRET123","details":[
        {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/SECRET123","quotaId":"SECRET123","quotaValue":"SECRET123"},{"quotaMetric":"https://evil.test/?key=SECRET123","quotaId":"prose with spaces","quotaValue":"-1"}]},
        {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"SECRET123s"},
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quota_metric":"generativelanguage.googleapis.com/interactions_requests","quota_limit":"InteractionsPerMinute","quota_limit_value":"12","secret":"SECRET123","consumer":"projects/private"}}
    ]}}).to_string();
    let safe = safe_http_error(429, &body, &["SECRET123"]);
    assert!(safe.get("provider_status").is_none());
    assert!(safe.get("retry_after_seconds").is_none());
    assert_eq!(safe["quotas"].as_array().unwrap().len(), 1);
    assert_eq!(safe["quotas"][0]["limit_value"], 12);
    for forbidden in ["SECRET123", "evil.test", "prose", "projects/private"] {
        assert!(!safe.to_string().contains(forbidden));
    }
    assert!(
        safe_http_error(500, "not JSON SECRET123", &["SECRET123"])
            .get("quotas")
            .is_none()
    );
}

#[test]
fn explicit_beta_profile_and_schema_preserve_two_round_contract() {
    let beta: Value = serde_json::from_str(BETA_OPENAPI).unwrap();
    let stable: Value = serde_json::from_str(OPENAPI).unwrap();
    assert_eq!(
        digest(BETA_OPENAPI.as_bytes()),
        "c3993507e6928c16dca47817038d32a8ef853c1e1c049f154d101c8b0c9b1d21"
    );
    assert_eq!(beta["info"]["version"], "v1beta");
    assert_eq!(
        beta["components"]["schemas"]["ModelOption"]["x-speakeasy-unknown-values"],
        "allow"
    );
    for seam in [
        "FunctionCallStep",
        "FunctionResultStep",
        "UserInputStep",
        "ModelOutputStep",
    ] {
        for field in ["properties", "required"] {
            assert_eq!(
                beta["components"]["schemas"][seam][field],
                stable["components"]["schemas"][seam][field]
            );
        }
    }
    for selected in [STABLE_PROFILE, BETA_PROFILE] {
        let mut s = Session::new(selected.model, "Run two rounds", vec![test_tool()]).unwrap();
        for (fixture, id, receipt) in [
            (
                include_str!("../fixtures/round1.json"),
                "call-1",
                "receipt-1",
            ),
            (
                include_str!("../fixtures/round2.json"),
                "call-2",
                "receipt-2",
            ),
        ] {
            let request: Value = serde_json::from_str(&s.request("any").unwrap()).unwrap();
            assert_eq!(request["model"], selected.model);
            s.accept(parse_turn(&fixture.replace(MODEL, selected.model)).unwrap())
                .unwrap();
            s.results(vec![(id.into(), json!({"receipt":receipt}), false)])
                .unwrap();
        }
        let replay = s.request("none").unwrap();
        assert!(replay.contains("AAEC/w=="));
        assert!(replay.contains("//8AAg=="));
        assert_eq!(
            s.accept(
                parse_turn(
                    &include_str!("../fixtures/completed.json").replace(MODEL, selected.model)
                )
                .unwrap()
            )
            .unwrap(),
            Stop::Completed
        );
    }
    assert!(profile("silent-fallback").is_none());
    assert_eq!(BETA_PROFILE.api_version, "v1beta");
    assert_eq!(BETA_PROFILE.model, "gemini-3.8-flash");
}
#[test]
fn generic_error_shape_and_category_never_copy_prose() {
    let summary = safe_http_error(
        429,
        r#"{"error":"Rate limit exceeded for secret SECRET123"}"#,
        &["SECRET123"],
    );
    assert_eq!(summary["error_shape"], "string");
    assert_eq!(summary["message_category"], "rate_limit");
    assert!(!summary.to_string().contains("SECRET123"));
    let summary = safe_http_error(
        429,
        r#"{"error":{"code":429,"message":"Quota exhausted for SECRET123"}}"#,
        &["SECRET123"],
    );
    assert_eq!(summary["provider_code"], 429);
    assert_eq!(summary["message_category"], "quota");
    assert!(!summary.to_string().contains("SECRET123"));
}
