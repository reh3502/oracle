use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const FIRST: &str = include_str!("../../tests/fixtures/gemini/round1.json");
const SECOND: &str = include_str!("../../tests/fixtures/gemini/round2.json");
const COMPLETE: &str = include_str!("../../tests/fixtures/gemini/completed.json");
fn profile() -> ModelProfile {
    ModelProfile {
        id: "stable-test".into(),
        model: "gemini-3.7-flash".into(),
        api_version: "v1".into(),
        max_context_tokens: 1_000_000,
        max_output_tokens: 4096,
    }
}
fn provider() -> GeminiProvider {
    GeminiProvider::new(profile(), "synthetic-key").unwrap()
}
fn request() -> ModelRequest {
    ModelRequest {
        goal: "Run two rounds".into(),
        system_instruction: "Trusted system".into(),
        tools: vec![ToolDefinition {
            name: "oracle_probe".into(),
            description: "Probe".into(),
            parameters: json!({"type":"object","properties":{"round":{"type":"integer","enum":[1,2]},"prior":{"type":"string"}},"required":["round","prior"],"additionalProperties":false}),
        }],
        continuation: None,
        results: vec![],
        max_output_tokens: 1024,
        max_request_bytes: 100_000,
        max_response_bytes: 100_000,
        timeout_ms: 3000,
    }
}
fn decode(
    p: &GeminiProvider,
    prepared: PreparedTurn,
    body: &str,
) -> Result<ModelTurn, ProviderError> {
    let native: NativeRequest = serde_json::from_str(&prepared.body).unwrap();
    let metadata = p
        .check_metadata(
            &prepared.body,
            &native,
            prepared.provider_metadata.as_deref().unwrap(),
        )
        .unwrap();
    let history = p.check_native(&native, &metadata.historical_tools).unwrap();
    p.decode(body.as_bytes(), native, history, metadata.historical_tools)
}
fn result(id: &str) -> ToolResult {
    ToolResult {
        call_id: id.into(),
        value: json!({"receipt":"receipt-1"}),
        is_error: false,
    }
}
fn resume(turn: ModelTurn, results: Vec<ToolResult>) -> ModelRequest {
    let mut r = request();
    r.continuation = Some(turn.continuation);
    r.results = results;
    r
}
fn first(p: &GeminiProvider) -> ModelTurn {
    decode(p, p.prepare(request()).unwrap(), FIRST).unwrap()
}

#[test]
fn profiles_keys_and_thinking_are_explicit() {
    let p = provider();
    assert_eq!(p.profile(), &profile());
    assert_eq!(
        p.endpoint,
        "https://generativelanguage.googleapis.com/v1/interactions"
    );
    assert!(p.key.is_sensitive());
    for (version, model) in [
        ("v2", "gemini-3.7-flash"),
        ("v1beta", "gemini-3.7-flash"),
        ("https://evil.invalid", "gemini-3.8-flash"),
    ] {
        let mut p = profile();
        p.api_version = version.into();
        p.model = model.into();
        assert!(matches!(
            GeminiProvider::new(p, "test"),
            Err(ProviderError::InvalidRequest)
        ));
    }
    for key in ["", "bad\nkey"] {
        assert!(matches!(
            GeminiProvider::new(profile(), key),
            Err(ProviderError::Auth)
        ));
    }
    let mut beta = profile();
    beta.api_version = "v1beta".into();
    beta.model = "gemini-3.8-flash".into();
    let p = GeminiProvider::with_thinking(beta, "test", ThinkingLevel::High).unwrap();
    assert!(p.endpoint.ends_with("/v1beta/interactions"));
    let v: Value = serde_json::from_str(&p.prepare(request()).unwrap().body).unwrap();
    assert_eq!(v["generation_config"]["thinking_level"], "high");
}

#[test]
fn two_rounds_replay_exact_bytes_and_only_one_user_input() {
    let p = provider();
    let mut r = resume(first(&p), vec![result("call-1")]);
    r.goal = "Never append this on continuation".into();
    let prepared = p.prepare(r).unwrap();
    assert!(prepared.body.contains(
        r#"{"type":"thought", "signature":"AAEC/w==", "future_metadata":{"number":1.2300e+20}}"#
    ));
    assert!(!prepared.body.contains("Never append"));
    let v: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(v["store"], false);
    assert_eq!(v["stream"], false);
    assert_eq!(v["system_instruction"], "Trusted system");
    assert_eq!(v["tools"][0]["name"], "oracle_probe");
    assert!(v.get("previous_interaction_id").is_none());
    assert_eq!(v["generation_config"]["thinking_level"], "low");
    assert_eq!(prepared.input_token_reservation, prepared.body.len() as u64);
    assert_eq!(prepared.output_token_reservation, 1024);
    let turn = decode(&p, prepared, SECOND).unwrap();
    assert_eq!(turn.usage, Usage::default());
    let prepared = p.prepare(resume(turn, vec![result("call-2")])).unwrap();
    assert!(prepared.body.contains("//8AAg=="));
    assert!(prepared.body.contains("AAEC/w=="));
    let v: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(
        v["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["type"] == "user_input")
            .count(),
        1
    );
    let turn = decode(&p, prepared, COMPLETE).unwrap();
    assert_eq!(turn.stop, StopReason::Completed);
    assert_eq!(
        turn.visible_text.as_deref(),
        Some("Both test rounds completed.")
    );
    assert!(p.prepare(resume(turn, vec![])).is_err());
}

#[test]
fn complete_results_are_unique_matched_and_ordered() {
    let p = provider();
    let mut v: Value = serde_json::from_str(FIRST).unwrap();
    let mut second = v["steps"][1].clone();
    second["id"] = json!("call-b");
    v["steps"].as_array_mut().unwrap().push(second);
    let turn = || decode(&p, p.prepare(request()).unwrap(), &v.to_string()).unwrap();
    for results in [
        vec![],
        vec![result("call-1")],
        vec![result("call-1"), result("call-1")],
        vec![result("call-1"), result("foreign")],
    ] {
        assert!(p.prepare(resume(turn(), results)).is_err());
    }
    let mut bad = result("call-b");
    bad.value = json!(42);
    assert!(
        p.prepare(resume(turn(), vec![result("call-1"), bad]))
            .is_err()
    );
    let mut b = result("call-b");
    b.is_error = true;
    let prepared = p
        .prepare(resume(turn(), vec![b, result("call-1")]))
        .unwrap();
    let wire: Value = serde_json::from_str(&prepared.body).unwrap();
    let input = wire["input"].as_array().unwrap();
    assert_eq!(input[input.len() - 2]["call_id"], "call-1");
    assert_eq!(input[input.len() - 1]["call_id"], "call-b");
    assert_eq!(input[input.len() - 1]["is_error"], true);
}

#[test]
fn continuation_is_bound_to_full_profile_policy_and_thinking() {
    let p = provider();
    let make = || resume(first(&p), vec![result("call-1")]);
    let mut r = make();
    r.continuation.as_mut().unwrap().profile = "foreign".into();
    assert!(p.prepare(r).is_err());
    let mut r = make();
    r.system_instruction.push('!');
    assert!(p.prepare(r).is_err());
    let mut r = make();
    r.tools[0].description.push('!');
    assert!(p.prepare(r).is_err());
    let mut q = profile();
    q.max_output_tokens = 2048;
    assert!(
        GeminiProvider::new(q, "test")
            .unwrap()
            .prepare(make())
            .is_err()
    );
    assert!(
        GeminiProvider::with_thinking(profile(), "test", ThinkingLevel::High)
            .unwrap()
            .prepare(make())
            .is_err()
    );
    let mut switched = profile();
    switched.model = "gemini-3.8-flash".into();
    switched.api_version = "v1beta".into();
    assert!(
        GeminiProvider::new(switched, "test")
            .unwrap()
            .prepare(make())
            .is_err()
    );
    let mut r = make();
    r.continuation.as_mut().unwrap().opaque = "{".into();
    assert!(p.prepare(r).is_err());
}

#[test]
fn invalid_calls_reject_whole_batch_and_duplicate_ids_across_rounds() {
    let p = provider();
    for mutation in [
        json!({"round":"1","prior":"none"}),
        json!({"round":1,"prior":"none","extra":true}),
        json!({"prior":"none"}),
        json!({"round":3,"prior":"none"}),
        json!([]),
    ] {
        let mut v: Value = serde_json::from_str(FIRST).unwrap();
        let mut invalid = v["steps"][1].clone();
        invalid["id"] = json!("second");
        invalid["arguments"] = mutation;
        v["steps"].as_array_mut().unwrap().push(invalid);
        assert!(matches!(
            decode(&p, p.prepare(request()).unwrap(), &v.to_string()),
            Err(ProviderError::RejectedToolCall { .. })
        ));
    }
    for field in ["id", "name"] {
        let mut v: Value = serde_json::from_str(FIRST).unwrap();
        v["steps"][1][field] = json!("");
        assert!(matches!(
            decode(&p, p.prepare(request()).unwrap(), &v.to_string()),
            Err(ProviderError::RejectedToolCall { .. })
        ));
    }
    let prepared = p
        .prepare(resume(first(&p), vec![result("call-1")]))
        .unwrap();
    assert!(matches!(
        decode(&p, prepared, FIRST),
        Err(ProviderError::RejectedToolCall { .. })
    ));
}

#[test]
fn invalid_proposals_do_not_release_text_or_valid_prefix_and_wire_errors_stay_distinct() {
    let p = provider();
    for bad in [
        json!({"type":"function_call","id":"bad","name":"unadvertised","arguments":{}}),
        json!({"type":"function_call","id":"call-1","name":"oracle_probe","arguments":{"round":1,"prior":"none"}}),
        json!({"type":"function_call","id":"","name":"oracle_probe","arguments":{"round":1,"prior":"none"}}),
    ] {
        let mut wire: Value = serde_json::from_str(FIRST).unwrap();
        wire["steps"].as_array_mut().unwrap().push(json!({"type":"model_output","content":[{"type":"text","text":"Must not escape with invalid proposal"}]}));
        wire["steps"].as_array_mut().unwrap().push(bad);
        // An Err has no ModelTurn, so neither the earlier valid call nor text
        // can reach the coordinator or create a continuation to replay.
        assert!(matches!(
            decode(&p, p.prepare(request()).unwrap(), &wire.to_string()),
            Err(ProviderError::RejectedToolCall { .. })
        ));
    }
    for (key, value) in [
        ("status", json!("unknown")),
        ("model", json!("wrong-model")),
        ("steps", json!([{"type":"unrecognized_wire_step"}])),
    ] {
        let mut wire: Value = serde_json::from_str(FIRST).unwrap();
        wire[key] = value;
        assert!(matches!(
            decode(&p, p.prepare(request()).unwrap(), &wire.to_string()),
            Err(ProviderError::ProtocolMismatch)
        ));
    }
    let mut malformed = request();
    malformed.tools[0].parameters = json!({"type":"object","properties":false});
    assert!(matches!(
        p.prepare(malformed),
        Err(ProviderError::InvalidRequest)
    ));
}

#[test]
fn unsupported_descriptors_fail_closed() {
    let p = provider();
    for schema in [
        json!({"type":"object","properties":{},"required":["missing"]}),
        json!({"type":"object","properties":{},"additionalProperties":true}),
        json!({"type":"object","properties":{"x":{"type":"string","pattern":".*"}}}),
        json!({"type":"object","properties":{"x":{"type":"array"}}}),
        json!({"type":"object","properties":{"x":{"type":"integer","enum":["1"]}}}),
    ] {
        let mut r = request();
        r.tools[0].parameters = schema;
        assert!(p.prepare(r).is_err());
    }
    let mut r = request();
    r.tools.push(r.tools[0].clone());
    assert!(p.prepare(r).is_err());
}

#[test]
fn malformed_and_terminal_replies_never_release_calls() {
    let p = provider();
    for raw in [
        "{",
        r#"{"status":"requires_action","steps":[]}"#,
        r#"{"status":"unknown","steps":[]}"#,
        r#"{"status":"completed"}"#,
    ] {
        assert!(decode(&p, p.prepare(request()).unwrap(), raw).is_err());
    }
    for (status, expected) in [
        ("incomplete", StopReason::Truncated),
        ("failed", StopReason::Failed),
        ("cancelled", StopReason::Cancelled),
        ("completed", StopReason::Completed),
    ] {
        let raw = FIRST.replace("requires_action", status);
        let t = decode(&p, p.prepare(request()).unwrap(), &raw).unwrap();
        assert!(t.calls.is_empty());
        assert_eq!(t.stop, expected);
    }
    let raw = FIRST.replace("\"usage\"", "\"errors\":{},\"usage\"");
    assert!(decode(&p, p.prepare(request()).unwrap(), &raw).is_err());
}

#[test]
fn usage_stays_unknown_and_thought_text_is_never_visible() {
    let p = provider();
    let raw = r#"{"status":"completed","steps":[{"type":"thought","summary":[{"type":"text","text":"PRIVATE"}],"signature":"opaque"},{"type":"model_output","content":[{"type":"text","text":"Visible"}]}],"usage":{"total_input_tokens":9,"total_output_tokens":4,"total_tokens":13,"total_cached_tokens":0,"total_thought_tokens":2}}"#;
    let t = decode(&p, p.prepare(request()).unwrap(), raw).unwrap();
    assert_eq!(t.visible_text.as_deref(), Some("Visible"));
    assert_eq!(
        t.usage,
        Usage {
            input_tokens: Some(9),
            output_tokens: Some(4),
            total_tokens: Some(13),
            cached_tokens: Some(0),
            reasoning_tokens: Some(2)
        }
    );
    let t = first(&p);
    assert_eq!(t.visible_text, None);
    assert_eq!(t.usage.input_tokens, None);
    assert_eq!(t.usage.total_tokens, Some(23));
    let t = decode(
        &p,
        p.prepare(request()).unwrap(),
        include_str!("../../tests/fixtures/gemini/refusal-prose.json"),
    )
    .unwrap();
    assert_eq!(t.stop, StopReason::Completed);
    assert!(t.calls.is_empty());
}

#[test]
fn preparation_bounds_and_caps_are_enforced() {
    let p = provider();
    for cap in [0, 4097] {
        let mut r = request();
        r.max_output_tokens = cap;
        assert!(p.prepare(r).is_err());
    }
    let mut r = request();
    r.max_request_bytes = 1;
    assert!(p.prepare(r).is_err());
    let mut r = request();
    r.max_response_bytes = MAX_RESPONSE_BYTES + 1;
    assert!(p.prepare(r).is_err());
    let mut r = request();
    r.timeout_ms = MAX_TIMEOUT_MS + 1;
    assert!(p.prepare(r).is_err());
    let mut r = request();
    r.max_output_tokens = 32;
    let prepared = p.prepare(r).unwrap();
    assert_eq!(prepared.output_token_reservation, 32);
    let mut q = profile();
    q.max_context_tokens = 4096;
    let p = GeminiProvider::new(q, "test").unwrap();
    let mut r = request();
    r.goal = "界".repeat(2000);
    assert!(matches!(p.prepare(r), Err(ProviderError::ContextExceeded)));
}

// The only arbitrary endpoint seam is private, compiled exclusively in tests.
async fn loopback(
    response: String,
    delay: Duration,
) -> (GeminiProvider, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut p = provider();
    p.endpoint = format!("http://{}/v1/interactions", listener.local_addr().unwrap());
    p.client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        loop {
            let mut b = [0; 4096];
            let n = socket.read(&mut b).await.unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&b[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let len: usize = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if data.len() >= end + 4 + len {
                    break;
                }
            }
        }
        tokio::time::sleep(delay).await;
        let _ = socket.write_all(response.as_bytes()).await;
        drop(socket);
        // A retry or followed redirect would connect again while this listener remains open.
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err()
        );
        String::from_utf8(data).unwrap()
    });
    (p, task)
}
fn http(status: &str, body: &str, headers: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn real_http_posts_native_request_once_and_reads_response() {
    let (p, task) = loopback(http("200 OK", FIRST, ""), Duration::ZERO).await;
    let prepared = p.prepare(request()).unwrap();
    let expected = prepared.body.clone();
    let t = p.send(prepared, &CancellationToken::new()).await.unwrap();
    assert_eq!(t.calls[0].id, "call-1");
    let wire = task.await.unwrap();
    assert!(wire.starts_with("POST /v1/interactions HTTP/1.1\r\n"));
    assert!(
        wire.to_ascii_lowercase()
            .contains("x-goog-api-key: synthetic-key\r\n")
    );
    assert!(wire.ends_with(&expected));
}

#[tokio::test]
async fn http_errors_are_typed_redacted_and_never_retried() {
    for (status, headers, expected) in [
        ("401 Unauthorized", "", ProviderError::Auth),
        ("403 Forbidden", "", ProviderError::Auth),
        (
            "429 Too Many Requests",
            "Retry-After: 2\r\n",
            ProviderError::RateLimited {
                retry_after_ms: Some(2000),
            },
        ),
        (
            "503 Unavailable",
            "",
            ProviderError::HttpTransient { status: 503 },
        ),
        (
            "408 Request Timeout",
            "",
            ProviderError::HttpTransient { status: 408 },
        ),
        (
            "500 Internal Server Error",
            "",
            ProviderError::HttpTransient { status: 500 },
        ),
        ("400 Bad Request", "", ProviderError::InvalidRequest),
        (
            "307 Temporary Redirect",
            "Location: http://127.0.0.1:1/forbidden\r\n",
            ProviderError::ProtocolMismatch,
        ),
    ] {
        let (p, task) =
            loopback(http(status, "SECRET private body", headers), Duration::ZERO).await;
        let error = p
            .send(p.prepare(request()).unwrap(), &CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert!(
            !format!(
                "{error:?} {error} {}",
                serde_json::to_string(&error).unwrap()
            )
            .contains("SECRET")
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn disconnected_transport_is_redacted_and_never_retried() {
    let (p, task) = loopback(String::new(), Duration::ZERO).await;
    let error = p
        .send(p.prepare(request()).unwrap(), &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error, ProviderError::Transport);
    assert_eq!(error.to_string(), "provider transport failed");
    assert_eq!(serde_json::to_string(&error).unwrap(), "\"transport\"");
    task.await.unwrap();
}

#[tokio::test]
async fn declared_chunked_and_truncated_bodies_are_bounded() {
    for response in [
        http("200 OK", &"x".repeat(300), ""),
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n12c\r\n{}\r\n0\r\n\r\n",
            "x".repeat(300)
        ),
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".into(),
        http("200 OK", "{", ""),
    ] {
        let (p, task) = loopback(response, Duration::ZERO).await;
        let mut r = request();
        r.max_response_bytes = 200;
        assert!(matches!(
            p.send(p.prepare(r).unwrap(), &CancellationToken::new())
                .await,
            Err(ProviderError::ProtocolMismatch)
        ));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_timeout_and_forged_preparation_are_fail_closed() {
    let p = provider();
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        p.send(p.prepare(request()).unwrap(), &token).await,
        Err(ProviderError::Cancelled)
    ));
    let mut prepared = p.prepare(request()).unwrap();
    prepared.input_token_reservation = 0;
    assert!(matches!(
        p.send(prepared, &CancellationToken::new()).await,
        Err(ProviderError::InvalidRequest)
    ));
    for cancel in [true, false] {
        let (p, task) = loopback(http("200 OK", COMPLETE, ""), Duration::from_millis(150)).await;
        let token = CancellationToken::new();
        let trigger = token.clone();
        let mut r = request();
        if !cancel {
            r.timeout_ms = 40;
        }
        let signal = tokio::spawn(async move {
            if cancel {
                tokio::time::sleep(Duration::from_millis(40)).await;
                trigger.cancel();
            }
        });
        let e = p.send(p.prepare(r).unwrap(), &token).await.err().unwrap();
        assert_eq!(
            e,
            if cancel {
                ProviderError::Cancelled
            } else {
                ProviderError::Timeout
            }
        );
        signal.await.unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn stalled_body_reads_obey_cancellation_and_total_deadline() {
    for cancelled in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut p = provider();
        p.endpoint = format!("http://{}/v1/interactions", listener.local_addr().unwrap());
        p.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let (headers_sent, headers_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n")
                .await
                .unwrap();
            let _ = headers_sent.send(());
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let token = CancellationToken::new();
        let trigger = token.clone();
        let signal = tokio::spawn(async move {
            headers_received.await.unwrap();
            if cancelled {
                trigger.cancel();
            }
        });
        let mut r = request();
        r.timeout_ms = 100;
        let error = p.send(p.prepare(r).unwrap(), &token).await.err().unwrap();
        assert_eq!(
            error,
            if cancelled {
                ProviderError::Cancelled
            } else {
                ProviderError::Timeout
            }
        );
        signal.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_prepared_turns_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut p = provider();
    p.endpoint = format!("http://{}/v1/interactions", listener.local_addr().unwrap());
    p.client = Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    for field in ["store", "model", "stream", "previous_interaction_id"] {
        let mut prepared = p.prepare(request()).unwrap();
        let mut body: Value = serde_json::from_str(&prepared.body).unwrap();
        body[field] = json!(true);
        prepared.body = body.to_string();
        prepared.input_token_reservation = prepared.body.len() as u64;
        assert!(matches!(
            p.send(prepared, &CancellationToken::new()).await,
            Err(ProviderError::InvalidRequest)
        ));
    }
    let mut prepared = p.prepare(request()).unwrap();
    prepared.timeout_ms = 0;
    assert!(matches!(
        p.send(prepared, &CancellationToken::new()).await,
        Err(ProviderError::InvalidRequest)
    ));
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        p.send(p.prepare(request()).unwrap(), &token).await,
        Err(ProviderError::Cancelled)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn duplicate_call_fields_ids_and_invalid_usage_fail_closed() {
    let p = provider();
    let mut v: Value = serde_json::from_str(FIRST).unwrap();
    let same = v["steps"][1].clone();
    v["steps"].as_array_mut().unwrap().push(same);
    assert!(decode(&p, p.prepare(request()).unwrap(), &v.to_string()).is_err());
    for raw in [
        FIRST.replace(
            "\"id\":\"call-1\"",
            "\"id\":\"call-1\",\"id\":\"different\"",
        ),
        FIRST.replace("\"total_tokens\":23", "\"total_tokens\":-1"),
        FIRST.replace("\"round\":1", "\"round\":18446744073709551616"),
    ] {
        assert!(decode(&p, p.prepare(request()).unwrap(), &raw).is_err());
    }
}

#[test]
fn discovery_adds_and_removes_active_tools_without_losing_history() {
    let p = provider();
    let mut initial = request();
    initial.tools[0].name = "core_tools_search".into();
    let search_reply = FIRST.replace("oracle_probe", "core_tools_search");
    let first = decode(&p, p.prepare(initial).unwrap(), &search_reply).unwrap();
    // The host selects the discovered tool and removes search in the very next round.
    let next = resume(first, vec![result("call-1")]);
    let prepared = p.prepare(next).unwrap();
    let wire: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(wire["tools"].as_array().unwrap().len(), 1);
    assert_eq!(wire["tools"][0]["name"], "oracle_probe");
    assert_eq!(wire["input"][2]["name"], "core_tools_search");
    assert_eq!(wire["input"][3]["call_id"], "call-1");
    assert!(prepared.body.contains(
        r#"{"type":"thought", "signature":"AAEC/w==", "future_metadata":{"number":1.2300e+20}}"#
    ));
    let second = decode(&p, prepared, SECOND).unwrap();
    // Remove every active tool after resolving the newly selected operation.
    let mut final_request = resume(second, vec![result("call-2")]);
    final_request.tools.clear();
    let prepared = p.prepare(final_request).unwrap();
    let wire: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(wire["tools"], json!([]));
    assert_eq!(wire["input"].as_array().unwrap().len(), 7);
    assert_eq!(
        decode(&p, prepared, COMPLETE).unwrap().stop,
        StopReason::Completed
    );
}

#[test]
fn used_alias_cannot_change_after_being_removed() {
    let p = provider();
    for schema_change in [false, true] {
        let first = first(&p);
        let mut r = resume(first, vec![result("call-1")]);
        r.tools[0].name = "another_tool".into();
        let second_reply = SECOND.replace("oracle_probe", "another_tool");
        let second = decode(&p, p.prepare(r).unwrap(), &second_reply).unwrap();
        let mut r = resume(second, vec![result("call-2")]);
        if schema_change {
            r.tools[0].parameters["properties"]["prior"]["description"] = json!("New semantics");
        } else {
            r.tools[0].description = "Different operation generation".into();
        }
        assert!(matches!(p.prepare(r), Err(ProviderError::InvalidRequest)));
    }
}

#[tokio::test]
async fn changed_shortlist_is_the_only_schema_set_sent_over_http() {
    let (p, server) = loopback(http("200 OK", SECOND, ""), Duration::ZERO).await;
    let mut initial = request();
    initial.tools[0].name = "core_tools_search".into();
    let first = decode(
        &p,
        p.prepare(initial).unwrap(),
        &FIRST.replace("oracle_probe", "core_tools_search"),
    )
    .unwrap();
    let prepared = p.prepare(resume(first, vec![result("call-1")])).unwrap();
    let expected = prepared.body.clone();
    let turn = p.send(prepared, &CancellationToken::new()).await.unwrap();
    assert_eq!(turn.calls[0].name, "oracle_probe");
    let wire = server.await.unwrap();
    assert!(wire.ends_with(&expected));
    let native: Value = serde_json::from_str(&expected).unwrap();
    assert_eq!(native["tools"].as_array().unwrap().len(), 1);
    for private_field in [
        "historical_tools",
        "provider_metadata",
        "binding",
        "body_sha256",
    ] {
        assert!(native.get(private_field).is_none());
    }
    let state: State = serde_json::from_str(&turn.continuation.opaque).unwrap();
    assert_eq!(state.historical_tools.len(), 2);
}

#[test]
fn retired_tools_and_old_ids_are_not_callable_after_discovery() {
    let p = provider();
    let make = || {
        let mut r = resume(first(&p), vec![result("call-1")]);
        r.tools[0].name = "new_tool".into();
        p.prepare(r).unwrap()
    };
    // Valid schema for an old alias still must not authorize a new response call.
    assert!(matches!(
        decode(&p, make(), SECOND),
        Err(ProviderError::RejectedToolCall { .. })
    ));
    // A newly authorized alias cannot reuse a historical call ID either.
    let raw = SECOND
        .replace("oracle_probe", "new_tool")
        .replace("call-2", "call-1");
    assert!(matches!(
        decode(&p, make(), &raw),
        Err(ProviderError::RejectedToolCall { .. })
    ));
    let mut r = resume(first(&p), vec![]);
    r.tools.clear();
    assert!(p.prepare(r).is_err());
}

#[tokio::test]
async fn missing_swapped_or_oversized_private_metadata_never_connects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut p = provider();
    p.endpoint = format!("http://{}/v1/interactions", listener.local_addr().unwrap());
    p.client = Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let mut different = request();
    different.goal = "Another request".into();
    let swapped = p.prepare(different).unwrap().provider_metadata;
    for metadata in [
        None,
        Some("{".into()),
        swapped,
        Some("x".repeat(MAX_METADATA_BYTES + 1)),
    ] {
        let mut prepared = p.prepare(request()).unwrap();
        prepared.provider_metadata = metadata;
        assert!(matches!(
            p.send(prepared, &CancellationToken::new()).await,
            Err(ProviderError::InvalidRequest)
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn rejected_calls_preserve_reported_usage_without_private_response_data() {
    let p = provider();
    let mut wire: Value = serde_json::from_str(FIRST).unwrap();
    wire["steps"][1]["name"] = json!("PRIVATE_UNADVERTISED_NAME");
    wire["steps"][1]["arguments"] = json!({"secret":"PRIVATE_ARGUMENT_VALUE"});
    wire["steps"].as_array_mut().unwrap().push(
        json!({"type":"model_output","content":[{"type":"text","text":"PRIVATE_VISIBLE_TEXT"}]}),
    );
    wire["usage"] = json!({"total_input_tokens":20,"total_output_tokens":3,"total_tokens":23,"total_cached_tokens":4,"total_thought_tokens":2});
    let error = decode(&p, p.prepare(request()).unwrap(), &wire.to_string())
        .err()
        .unwrap();
    let usage = error
        .reported_usage()
        .expect("valid response usage must survive a rejected call");
    assert_eq!(usage.total_tokens, Some(23));
    assert_eq!(usage.input_tokens, Some(20));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(usage.cached_tokens, Some(4));
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert!(matches!(
        error,
        ProviderError::RejectedToolCall {
            reason: ToolCallRejection::UnknownTool,
            ..
        }
    ));
    let serialized = serde_json::to_string(&error).unwrap();
    for secret in [
        "PRIVATE_UNADVERTISED_NAME",
        "PRIVATE_ARGUMENT_VALUE",
        "PRIVATE_VISIBLE_TEXT",
        "AAEC/w==",
        "synthetic-key",
    ] {
        assert!(!serialized.contains(secret));
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }
}

#[test]
fn rejected_call_reasons_are_typed_and_batches_remain_atomic() {
    let p = provider();
    for (bad, reason) in [
        (
            json!({"type":"function_call","id":"","name":"oracle_probe","arguments":{"round":1,"prior":"none"}}),
            ToolCallRejection::EmptyId,
        ),
        (
            json!({"type":"function_call","id":"call-1","name":"oracle_probe","arguments":{"round":1,"prior":"none"}}),
            ToolCallRejection::DuplicateId,
        ),
        (
            json!({"type":"function_call","id":"second","name":"oracle_probe","arguments":[]}),
            ToolCallRejection::NonObjectArguments,
        ),
        (
            json!({"type":"function_call","id":"second","name":"not-advertised","arguments":{}}),
            ToolCallRejection::UnknownTool,
        ),
        (
            json!({"type":"function_call","id":"second","name":"oracle_probe","arguments":{"round":9,"prior":"none"}}),
            ToolCallRejection::InvalidArguments,
        ),
    ] {
        let mut wire: Value = serde_json::from_str(FIRST).unwrap();
        wire["steps"].as_array_mut().unwrap().push(bad);
        let error = decode(&p, p.prepare(request()).unwrap(), &wire.to_string())
            .err()
            .unwrap();
        assert_eq!(
            error,
            ProviderError::RejectedToolCall {
                reason,
                usage: Some(Usage {
                    total_tokens: Some(23),
                    ..Default::default()
                }),
            }
        );
        // Failure releases neither the valid prefix nor a continuation; the
        // rejected response cannot poison the next otherwise-valid decode.
        let valid = decode(&p, p.prepare(request()).unwrap(), FIRST).unwrap();
        assert_eq!(valid.calls.len(), 1);
        assert_eq!(valid.calls[0].id, "call-1");
    }
}

#[test]
fn rejected_calls_report_usage_only_after_complete_wire_validation() {
    let p = provider();
    let mut rejected: Value = serde_json::from_str(FIRST).unwrap();
    rejected["steps"][1]["name"] = json!("unadvertised");
    for (key, value) in [
        ("status", json!("unknown-status")),
        ("model", json!("wrong-model")),
        ("errors", json!({"code":"provider-failed"})),
        ("usage", json!({"total_tokens":-1})),
        ("usage", json!({"total_tokens":"23"})),
    ] {
        let mut wire = rejected.clone();
        wire[key] = value;
        let error = decode(&p, p.prepare(request()).unwrap(), &wire.to_string())
            .err()
            .unwrap();
        assert_eq!(error, ProviderError::ProtocolMismatch);
        assert!(error.reported_usage().is_none());
    }
    // A later malformed step must not be hidden by an earlier invalid proposal.
    for bad in [
        json!({"type":"unknown_step"}),
        json!({"type":"model_output","content":[{"type":"text","text":7}]}),
    ] {
        let mut wire = rejected.clone();
        wire["steps"].as_array_mut().unwrap().push(bad);
        let error = decode(&p, p.prepare(request()).unwrap(), &wire.to_string())
            .err()
            .unwrap();
        assert_eq!(error, ProviderError::ProtocolMismatch);
        assert!(error.reported_usage().is_none());
    }
    rejected.as_object_mut().unwrap().remove("usage");
    let error = decode(&p, p.prepare(request()).unwrap(), &rejected.to_string())
        .err()
        .unwrap();
    assert!(matches!(
        error,
        ProviderError::RejectedToolCall { usage: None, .. }
    ));
    assert!(error.reported_usage().is_none());
    // Preserve contradictory numeric reports as reports, not trusted refunds.
    // The existing budget/campaign validators decide whether they may settle.
    rejected["usage"] =
        json!({"total_input_tokens":100,"total_output_tokens":100,"total_tokens":100});
    let error = decode(&p, p.prepare(request()).unwrap(), &rejected.to_string())
        .err()
        .unwrap();
    assert_eq!(error.reported_usage().unwrap().input_tokens, Some(100));
    assert_eq!(error.reported_usage().unwrap().output_tokens, Some(100));
    assert_eq!(error.reported_usage().unwrap().total_tokens, Some(100));
    assert!(ProviderError::InvalidToolCall.reported_usage().is_none());
}

#[test]
fn rejected_calls_in_supplied_history_remain_invalid_requests() {
    let p = provider();
    for (field, value) in [
        ("id", json!("")),
        ("name", json!("unadvertised")),
        ("arguments", json!([])),
        ("arguments", json!({"round":9,"prior":"none"})),
    ] {
        let mut turn = first(&p);
        let mut state: Value = serde_json::from_str(&turn.continuation.opaque).unwrap();
        let call = state["history"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|step| step["type"] == "function_call")
            .unwrap();
        call[field] = value;
        turn.continuation.opaque = state.to_string();
        let error = p
            .prepare(resume(turn, vec![result("call-1")]))
            .err()
            .unwrap();
        assert_eq!(error, ProviderError::InvalidRequest);
        assert!(error.reported_usage().is_none());
    }
}
