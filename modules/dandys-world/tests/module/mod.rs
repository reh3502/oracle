use super::*;
use dandys_world_core::{
    model::{CatalogData, SOURCE_ORIGIN},
    query::QueryResponse,
};
use oracle_module_sdk::CancellationToken;
use oracle_rpc::{RpcHandler, RpcPeer};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
const NOW: u64 = 1_800_000_000_000;
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dw-module-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn catalog() -> CatalogData {
    serde_json::from_value(json!({
        "schema_version":1,"adapter_version":"synthetic-module-fixture","source_origin":SOURCE_ORIGIN,
        "crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z",
        "sources":[{"id":"page:1","page_id":1,"title":"Fixture Rock","url":format!("{SOURCE_ORIGIN}/wiki/Fixture_Rock"),"revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":NOW,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}],
        "entities":[{"id":"toon:fixture","kind":"toon","name":"Fixture Rock","aliases":["Rock"],"availability":"supported","warnings":[],"facts":[{"id":"fixture.health","key":"health","text":"Fixture hearts","value":2,"unit":"hearts","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Fixture stats","quote":"Two fixture hearts"}]}],"relationships":[]}],
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}
    })).unwrap()
}
fn response(catalog: CatalogData, request: QueryRequest, now: u64) -> QueryResponse {
    QueryEngine::new("fixture-snapshot".into(), Arc::new(catalog))
        .execute(request, now)
        .unwrap()
}
fn lookup(offset: usize) -> QueryRequest {
    QueryRequest::Lookup {
        name: "Fixture Rock".into(),
        kind: Some(dandys_world_core::model::Kind::Toon),
        field: None,
        offset,
    }
}
fn assert_bounded(reply: &presentation::Reply) {
    assert!(reply.text.encode_utf16().count() <= 1800);
    assert!(reply.citations.len() <= 5);
    let rendered = format!(
        "{}{}",
        reply.text,
        reply
            .citations
            .iter()
            .map(|c| format!(
                "\n[{}]({SOURCE_ORIGIN}/index.php?oldid={})",
                c.label, c.revision
            ))
            .collect::<String>()
    );
    assert!(
        rendered.encode_utf16().count() <= 1900,
        "{}",
        rendered.len()
    );
}
struct Host(AtomicUsize);
#[async_trait]
impl RpcHandler for Host {
    async fn handle(
        &self,
        _peer: RpcPeer,
        _method: String,
        _params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RpcError::Remote("No callbacks permitted".into()))
    }
}
struct Harness {
    peer: RpcPeer,
    task: tokio::task::JoinHandle<Result<()>>,
    host: Arc<Host>,
}
impl Harness {
    fn new(module: Arc<DwModule>) -> Self {
        let (a, b) = tokio::io::duplex(65536);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let host = Arc::new(Host(AtomicUsize::new(0)));
        let peer = RpcPeer::new(ar, aw, host.clone());
        let task = tokio::spawn(oracle_module_sdk::serve_streams(module, br, bw));
        Self { peer, task, host }
    }
    async fn call(&self, method: &str, input: Value) -> Result<Value> {
        self.peer.call(method, input, Duration::from_secs(5)).await
    }
    async fn hello(&self) {
        assert_eq!(
            self.call(
                "hello",
                json!({"protocol_major":1,"protocol_minor":1,"session":"dw-test","generation":7})
            )
            .await
            .unwrap()["protocol_minor"],
            1
        );
    }
    async fn initialize(&self, runtime: Value) -> Result<Value> {
        self.call(
            "initialize",
            json!({"session":"dw-test","generation":7,"mode":"normal","runtime":runtime}),
        )
        .await
    }
    async fn invoke(&self, operation: &str, input: Value) -> Result<Value> {
        self.call("operation.invoke",json!({"invocation":"opaque-test","session":"dw-test","generation":7,"guild":"123","epoch":2,"operation":operation,"input":input})).await
    }
    async fn close(self) {
        self.call("shutdown", json!({})).await.unwrap();
        assert_eq!(self.host.0.load(Ordering::SeqCst), 0);
        self.peer.close().await;
        self.task.await.unwrap().unwrap();
    }
}
#[tokio::test]
async fn sdk_lifecycle_requires_snapshot_and_fences_queries_without_callbacks() {
    let dir = Temp::new();
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    assert!(
        h.invoke("lookup", json!({"name":"Fixture Rock"}))
            .await
            .is_err()
    );
    assert!(
        h.initialize(json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("data_directory")
    );
    assert!(
        h.initialize(json!({"data_directory":"relative"}))
            .await
            .is_err()
    );
    assert!(h.initialize(json!({"data_directory":dir.0})).await.is_err());
    let mut data = catalog();
    data.sources[0].validated_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    Store::new(&dir.0)
        .unwrap()
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    h.call("activate", json!({"guild":"123","epoch":2}))
        .await
        .unwrap();
    let reply = h
        .invoke("lookup", json!({"name":"Fixture Rock","field":"health"}))
        .await
        .unwrap();
    assert!(
        reply["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("Value: 2")
    );
    assert_eq!(reply["reply"]["citations"][0]["revision"], 2);
    assert!(
        h.invoke("lookup", json!({"name":"Fixture Rock","op":"status"}))
            .await
            .is_err()
    );
    h.call("quiesce", json!({"guild":"123"})).await.unwrap();
    assert!(h.invoke("status", json!({})).await.is_err());
    h.close().await;
    assert!(module.query("status", json!({}), NOW).is_err());
}
async fn wait_for_snapshot(module: &DwModule, expected: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if module
                .engine
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|e| e.id == expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("snapshot monitor adopted publication");
}
#[tokio::test]
async fn snapshot_monitor_adopts_publication_and_rollback_with_pinned_readers() {
    let dir = Temp::new();
    let store = Store::new(&dir.0).unwrap();
    let mut data = catalog();
    let first = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    let pinned = module.engine.read().unwrap().clone().unwrap();
    data.entities[0].facts[0].value = json!(9);
    let second = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    wait_for_snapshot(&module, &second.id).await;
    assert!(
        module.query("lookup", json!({"name":"Rock"}), NOW).unwrap()["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("Value: 9")
    );
    let old_reply = pinned.engine.execute(lookup(0), NOW).unwrap();
    assert_eq!(old_reply.snapshot_id, first.id);
    assert!(
        presentation::render(&lookup(0), &old_reply)
            .text
            .contains("Value: 2")
    );
    store.rollback().unwrap();
    wait_for_snapshot(&module, &first.id).await;
    assert!(
        module.query("lookup", json!({"name":"Rock"}), NOW).unwrap()["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("Value: 2")
    );
    h.close().await;
    store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        module.engine.read().unwrap().is_none(),
        "joined monitor cannot repopulate after shutdown"
    );
}
#[tokio::test]
async fn startup_recovers_previous_and_bad_reload_preserves_loaded_snapshot() {
    let dir = Temp::new();
    let store = Store::new(&dir.0).unwrap();
    let mut data = catalog();
    let first = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    data.entities[0].facts[0].value = json!(9);
    let second = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    fs::write(dir.0.join(format!("{}.json", second.id)), b"corrupt").unwrap();
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    assert_eq!(module.engine.read().unwrap().as_ref().unwrap().id, first.id);
    assert_eq!(
        *module.recovery.read().unwrap(),
        "Recovered previous snapshot"
    );
    fs::write(dir.0.join("active"), b"invalid pointer").unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while !module.recovery.read().unwrap().contains("reload failed") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(module.engine.read().unwrap().as_ref().unwrap().id, first.id);
    assert_eq!(
        module
            .engine
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .oldest_validation_ms,
        NOW
    );
    h.close().await;
}
#[test]
fn manifest_exposes_only_typed_member_routes_and_private_health() {
    let m = DwModule::default().manifest();
    assert_eq!(m.id.as_str(), "community.dandys-world");
    assert_eq!(m.manifest_version, 2);
    assert_eq!(m.protocol_minor_min, 1);
    assert!(m.runtime.unwrap().data_directory_required);
    assert!(m.capabilities.is_empty());
    assert!(
        m.operations
            .iter()
            .all(|o| o.capabilities.is_empty() && o.ai.is_none())
    );
    let routes = m.commands.unwrap().routes;
    assert_eq!(routes.len(), 6);
    for route in routes {
        assert_ne!(route.operation, "health");
        assert!(matches!(
            route.input,
            Some(oracle_contracts::ModuleCommandInput::Typed { .. })
        ));
        assert_eq!(
            m.operations
                .iter()
                .find(|o| o.name == route.operation)
                .unwrap()
                .audience,
            oracle_contracts::ModuleAudience::MemberRead
        );
    }
    assert_eq!(
        m.operations
            .iter()
            .find(|o| o.name == "health")
            .unwrap()
            .audience,
        oracle_contracts::ModuleAudience::Operator
    );
    for (operation, input) in [
        ("lookup", json!({"name":"Rock","surprise":1})),
        ("lookup", json!({"name":42})),
        ("search", json!({"query":"x","limit":1.5})),
        ("sources", json!({"offset":-1})),
        ("status", json!({"op":"lookup"})),
        ("unknown", json!({})),
    ] {
        assert!(request(operation, input).is_err());
    }
}
#[test]
fn renderer_keeps_complete_fact_conditions_and_evidence() {
    let req = lookup(0);
    let reply = presentation::render(&req, &response(catalog(), req.clone(), NOW));
    assert!(reply.text.contains("Fixture hearts"));
    assert!(reply.text.contains("Conditions: base"));
    assert!(reply.text.contains("Value: 2"));
    assert_eq!(reply.citations[0].revision, 2);
    assert_bounded(&reply);
}
#[test]
fn renderer_does_not_claim_conflicting_or_stale_values() {
    let mut data = catalog();
    data.entities[0].facts[0].state = dandys_world_core::model::EvidenceState::Conflicting;
    let req = lookup(0);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert!(reply.text.contains("Conflicting"));
    assert!(!reply.text.contains("Value: 2"));
    assert!(!reply.text.contains("Fixture hearts"));
    assert_eq!(reply.citations.len(), 1);
    assert_bounded(&reply);
    let reply = presentation::render(
        &req,
        &response(catalog(), req.clone(), NOW + 8 * 86_400_000),
    );
    assert!(!reply.text.contains("Value: 2"));
    assert!(reply.text.contains("seven days"));
    assert_bounded(&reply);
}
#[test]
fn oversized_fact_falls_back_without_truncating_claim_and_offers_next_offset() {
    let mut data = catalog();
    data.entities[0].facts[0].text = "oversized claim ".repeat(200);
    let mut second = data.entities[0].facts[0].clone();
    second.id = "second".into();
    second.key = "z_field".into();
    second.text = "Second complete fact".into();
    data.entities[0].facts.push(second);
    let req = lookup(0);
    let reply = presentation::render(&req, &response(data.clone(), req.clone(), NOW));
    assert!(!reply.text.contains("oversized claim"));
    assert!(reply.text.contains("consult the linked wiki"));
    assert!(reply.text.contains("offset:1"));
    assert_eq!(reply.citations.len(), 1);
    assert_bounded(&reply);
    let req = lookup(1);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert!(reply.text.contains("Second complete fact"));
    assert!(!reply.text.contains("offset:"));
    assert_bounded(&reply);
}
#[test]
fn lookup_and_sources_pagination_use_number_actually_shown() {
    let mut data = catalog();
    let base = data.entities[0].facts[0].clone();
    data.entities[0].facts.clear();
    for i in 0..8 {
        let mut fact = base.clone();
        fact.id = format!("f{i}");
        fact.key = format!("field{i}");
        fact.text = format!("Full fact {i}: {}", "detail ".repeat(50));
        data.entities[0].facts.push(fact);
    }
    let req = lookup(0);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    let count = (0..8)
        .filter(|i| reply.text.contains(&format!("Full fact {i}:")))
        .count();
    assert!(count > 0 && count < 8);
    assert!(reply.text.contains(&format!("offset:{count}")));
    assert_bounded(&reply);
    let mut data = catalog();
    let base = data.sources[0].clone();
    for i in 1..9 {
        let mut source = base.clone();
        source.id = format!("page:{i}");
        source.revision_id = 10 + i;
        source.page_id = 10 + i;
        source.title = format!("Fixture source {i}");
        data.sources.push(source);
    }
    let req = QueryRequest::Sources {
        name: None,
        kind: None,
        offset: 0,
    };
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert_eq!(reply.citations.len(), 5);
    assert!(reply.text.contains("offset:5"));
    assert_bounded(&reply);
}
#[test]
fn excessive_evidence_uses_navigation_fallback_without_partial_fact() {
    let mut data = catalog();
    let base = data.sources[0].clone();
    let citation = data.entities[0].facts[0].citations[0].clone();
    for i in 2..=6 {
        let mut source = base.clone();
        source.id = format!("page:{i}");
        source.revision_id = i + 10;
        data.sources.push(source.clone());
        let mut c = citation.clone();
        c.source_id = source.id;
        data.entities[0].facts[0].citations.push(c);
    }
    let req = lookup(0);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert!(!reply.text.contains("Fixture hearts"));
    assert!(reply.text.contains("No game fact"));
    assert_eq!(reply.citations.len(), 1);
    assert_bounded(&reply);
}
#[test]
fn comparisons_keep_both_complete_sides_or_show_no_claim() {
    let mut data = catalog();
    let mut other = data.entities[0].clone();
    other.id = "toon:other".into();
    other.name = "Other Rock".into();
    other.aliases.clear();
    other.facts[0].id = "other.health".into();
    other.facts[0].value = json!(3);
    data.entities.push(other);
    let req = QueryRequest::Compare {
        left: "toon:fixture".into(),
        right: "toon:other".into(),
        field: Some("health".into()),
    };
    let reply = presentation::render(&req, &response(data.clone(), req.clone(), NOW));
    assert!(reply.text.contains("Value: 2") && reply.text.contains("Value: 3"));
    assert_bounded(&reply);
    data.entities[1].facts[0].text = "huge ".repeat(400);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert!(!reply.text.contains("Value: 2") && !reply.text.contains("Value: 3"));
    assert_bounded(&reply);
}
#[test]
fn optional_full_catalog_human_reply_qualification() {
    let Ok(path) = std::env::var("DW_TEST_CATALOG") else {
        return;
    };
    let data: CatalogData = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let now = data
        .sources
        .iter()
        .map(|s| s.validated_at_ms)
        .max()
        .unwrap()
        + 1;
    let engine = QueryEngine::new("full-catalog".into(), Arc::new(data.clone()));
    let cases = [
        (
            "lookup",
            json!({"name":"Pebble","kind":"toon","field":"health"}),
        ),
        ("lookup", json!({"name":"Twisted Pebble","field":"speed"})),
        (
            "lookup",
            json!({"name":"Bone","kind":"trinket","field":"effect"}),
        ),
        ("ask", json!({"question":"How does research work?"})),
        ("search", json!({"query":"Pebble"})),
        ("sources", json!({"name":"Pebble","kind":"toon"})),
        ("status", json!({})),
    ];
    for (operation, input) in cases {
        let req = request(operation, input).unwrap();
        let reply = presentation::render(&req, &engine.execute(req.clone(), now).unwrap());
        println!("{operation}: {}", reply.text);
        assert_bounded(&reply);
    }
    // Every entity's lookup start and each distinct field must produce a bounded reply.
    for entity in &data.entities {
        let mut fields: std::collections::BTreeSet<_> =
            entity.facts.iter().map(|f| Some(f.key.clone())).collect();
        fields.insert(None);
        for field in fields {
            let req = QueryRequest::Lookup {
                name: entity.id.clone(),
                kind: None,
                field,
                offset: 0,
            };
            let reply = presentation::render(&req, &engine.execute(req.clone(), now).unwrap());
            assert_bounded(&reply);
        }
    }
}
#[test]
fn out_of_range_revision_fails_closed_without_uncited_claim() {
    let mut data = catalog();
    data.sources[0].revision_id = u64::MAX;
    let req = lookup(0);
    let reply = presentation::render(&req, &response(data, req.clone(), NOW));
    assert!(reply.text.contains("cannot be displayed safely"));
    assert!(!reply.text.contains("Fixture hearts"));
    assert!(reply.citations.is_empty());
    assert_bounded(&reply);
}
#[tokio::test]
async fn status_and_health_report_pinned_snapshot_metadata_without_paths() {
    let dir = Temp::new();
    let store = Store::new(&dir.0).unwrap();
    let mut data = catalog();
    let mut source = data.sources[0].clone();
    source.id = "page:2".into();
    source.page_id = 2;
    source.title = "Fixture second source".into();
    source.url = format!("{SOURCE_ORIGIN}/wiki/Fixture_second_source");
    source.revision_id = 3;
    source.validated_at_ms = NOW - 1000;
    data.sources.push(source);
    data.coverage.discovered_pages = 2;
    data.coverage.imported_pages = 2;
    data.coverage.nonredirect_articles = 2;
    data.coverage.namespace_counts.insert("articles".into(), 2);
    let first = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    h.call("activate", json!({"guild":"123","epoch":2}))
        .await
        .unwrap();
    for operation in ["status", "health"] {
        let result = h.invoke(operation, json!({})).await.unwrap();
        let text = result["reply"]["text"].as_str().unwrap();
        assert!(text.contains(&first.id));
        assert!(text.contains("Entities: 1; source pages: 2."));
        if operation == "health" {
            assert!(text.contains(&format!("oldest {}; latest {}", NOW - 1000, NOW)));
            assert!(text.contains("Schema: 1"));
            assert!(text.contains("Unknown/unverified facts:"));
            assert!(text.contains("Last observed disk bytes:"));
            assert!(text.contains("Module queries:"));
        } else {
            assert!(text.contains("minutes ago") || text.contains("dated in the future"));
        }
        assert!(text.contains("Network refresh: Disabled"));
        assert!(!text.contains(dir.0.to_str().unwrap()));
        assert!(!text.contains("data_directory"));
        assert!(text.encode_utf16().count() <= 1800);
        assert_eq!(result["reply"]["citations"], json!([]));
        assert!(
            h.invoke(operation, json!({"directory":"forged"}))
                .await
                .is_err()
        );
    }
    // Metadata must describe the same retained snapshot as answers, not a newer disk pointer.
    data.sources[0].validated_at_ms = NOW + 1000;
    let second = store
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    assert_ne!(first.id, second.id);
    let old_status = module.query("status", json!({}), NOW).unwrap();
    let text = old_status["reply"]["text"].as_str().unwrap();
    assert!(text.contains(&first.id));
    assert!(!text.contains(&second.id));
    assert!(text.contains("oldest 0 minutes ago; newest 0 minutes ago"));
    let cached = module
        .query("status", json!({}), NOW + 2 * 86_400_000)
        .unwrap();
    assert!(
        cached["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("cached sources")
    );
    h.close().await;
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    let new_status = module.query("health", json!({}), NOW + 1001).unwrap();
    let text = new_status["reply"]["text"].as_str().unwrap();
    assert!(text.contains(&second.id));
    assert!(text.contains(&format!("latest {}.", NOW + 1000)));
    h.close().await;
}

#[tokio::test]
async fn invalid_refresh_configuration_preserves_queries_and_never_starts_worker() {
    for settings in [
        br#"{"url":"https://unapproved.example"}"#.as_slice(),
        br#"not json"#.as_slice(),
    ] {
        let dir = Temp::new();
        Store::new(&dir.0)
            .unwrap()
            .publish_bytes(&serde_json::to_vec(&catalog()).unwrap())
            .unwrap();
        fs::write(dir.0.join("refresh-settings.json"), settings).unwrap();
        let module = Arc::new(DwModule::default());
        let h = Harness::new(module.clone());
        h.hello().await;
        h.initialize(json!({"data_directory":dir.0})).await.unwrap();
        assert_eq!(
            *module.refresh.read().unwrap(),
            "Disabled: invalid refresh configuration"
        );
        assert!(module.query("lookup", json!({"name":"Rock"}), NOW).is_ok());
        assert!(!dir.0.join("refresh-schedule.json").exists());
        h.close().await;
    }
}

fn configure_fixture_worker(dir: &Path, script: &str) -> PathBuf {
    let worker = dir.join("fixture_worker.py");
    fs::write(&worker, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        dir.join("refresh-settings.json"),
        serde_json::to_vec(&json!({
            "enabled":true,"source_access_qualified":true,
            "python":fs::canonicalize("/usr/bin/python3").unwrap(),
            "worker":worker,"previous":null
        }))
        .unwrap(),
    )
    .unwrap();
    worker
}
async fn wait_for_refresh_result(dir: &Path, result: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(dir.join("refresh-schedule.json"))
                && let Ok(state) = serde_json::from_slice::<Value>(&bytes)
                && state["last_result"] == result
            {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "refresh attempt did not reach {result}: {:?}",
            fs::read_to_string(dir.join("refresh-schedule.json"))
        )
    })
}
#[tokio::test]
async fn scheduled_access_denial_is_persisted_across_module_restart() {
    let dir = Temp::new();
    Store::new(&dir.0)
        .unwrap()
        .publish_bytes(&serde_json::to_vec(&catalog()).unwrap())
        .unwrap();
    let worker = configure_fixture_worker(
        &dir.0,
        "from pathlib import Path\nimport sys\np=Path(__file__).with_suffix('.count')\np.write_text(str(int(p.read_text())+1) if p.exists() else '1')\nsys.exit(3)\n",
    );
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    let state = wait_for_refresh_result(&dir.0, "source_denied").await;
    assert_eq!(state["stopped_denied"], true);
    assert!(state["last_success_ms"].is_null());
    assert!(module.query("lookup", json!({"name":"Rock"}), NOW).is_ok());
    h.close().await;
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        fs::read_to_string(worker.with_extension("count")).unwrap(),
        "1"
    );
    assert!(module.query("lookup", json!({"name":"Rock"}), NOW).is_ok());
    h.close().await;
}

#[tokio::test]
async fn stalled_refresh_keeps_queries_available_and_shutdown_reaps_worker() {
    let dir = Temp::new();
    let store = Store::new(&dir.0).unwrap();
    let first = store
        .publish_bytes(&serde_json::to_vec(&catalog()).unwrap())
        .unwrap();
    let worker = configure_fixture_worker(
        &dir.0,
        "from pathlib import Path\nimport os,time\nPath(__file__).with_suffix('.pid').write_text(str(os.getpid()))\ntime.sleep(60)\n",
    );
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !worker.with_extension("pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(worker.with_extension("pid")).unwrap();
    h.call("activate", json!({"guild":"123","epoch":2}))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(1), h.invoke("status", json!({})))
        .await
        .unwrap()
        .unwrap();
    assert!(reply["reply"]["text"].as_str().unwrap().contains(&first.id));
    h.close().await;
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "worker is reaped before SDK shutdown completes"
    );
    assert_eq!(store.load().unwrap().id, first.id);
    assert!(module.engine.read().unwrap().is_none());
    let state: Value =
        serde_json::from_slice(&fs::read(dir.0.join("refresh-schedule.json")).unwrap()).unwrap();
    assert_eq!(
        state["running"], true,
        "restart must treat cancelled work as interrupted"
    );
    assert!(state["last_success_ms"].is_null());
}

#[tokio::test]
async fn scheduled_candidate_requires_review_before_serving_changed_facts() {
    use dandys_world_core::{refresh_control::RefreshControl, refresh_review::ReviewApproval};
    let dir = Temp::new();
    let mut data = catalog();
    data.sources[0].validated_at_ms = 1_789_171_200_000;
    let first = Store::new(&dir.0)
        .unwrap()
        .publish_bytes(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    data.entities[0].facts[0].value = json!(9);
    fs::write(
        dir.0.join("fixture_candidate.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
    configure_fixture_worker(
        &dir.0,
        "from pathlib import Path\nimport sys\np=Path(sys.argv[sys.argv.index('--output')+1])\np.mkdir()\n(p/'candidate.json').write_bytes(Path(__file__).with_name('fixture_candidate.json').read_bytes())\n",
    );
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    let state = wait_for_refresh_result(&dir.0, "review_required").await;
    assert!(state["last_success_ms"].is_null());
    assert_eq!(module.engine.read().unwrap().as_ref().unwrap().id, first.id);
    let control = RefreshControl::new(&dir.0).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let review = control.inspect_pending(now).unwrap().unwrap();
    let published = control
        .approve(
            &ReviewApproval {
                active_digest: review.active_digest,
                candidate_digest: review.candidate_digest,
            },
            now,
        )
        .unwrap();
    wait_for_snapshot(&module, &published.id).await;
    assert!(
        module
            .query("lookup", json!({"name":"Rock"}), 1_789_171_260_000)
            .unwrap()["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("Value: 9")
    );
    h.close().await;
}

#[tokio::test]
async fn server_retry_floor_survives_worker_and_scheduler_boundaries() {
    let dir = Temp::new();
    Store::new(&dir.0)
        .unwrap()
        .publish_bytes(&serde_json::to_vec(&catalog()).unwrap())
        .unwrap();
    let floor = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 86_400_000;
    configure_fixture_worker(
        &dir.0,
        &format!(
            "from pathlib import Path\nimport sys,json\np=Path(sys.argv[sys.argv.index('--output')+1])\np.mkdir()\n(p/'result.json').write_text(json.dumps({{'status':'retry','retry_not_before_ms':{floor}}}))\nsys.exit(2)\n"
        ),
    );
    let module = Arc::new(DwModule::default());
    let h = Harness::new(module.clone());
    h.hello().await;
    h.initialize(json!({"data_directory":dir.0})).await.unwrap();
    let state = wait_for_refresh_result(&dir.0, "rate_limited").await;
    assert!(state["next_due_ms"].as_u64().unwrap() >= floor);
    assert!(state["last_success_ms"].is_null());
    assert!(module.query("lookup", json!({"name":"Rock"}), NOW).is_ok());
    h.close().await;
}
