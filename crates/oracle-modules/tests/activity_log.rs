//! Real, separately built logging executable over SDK RPC, SQLite and a TCP notification peer.
use oracle_core::*;
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, ModuleManager, NotificationCheck, NotificationRequest,
    NotificationTransport,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
const ACTOR: PolicyContext = PolicyContext::LocalOperator;
const HOUR: u64 = 3_600_000;
const RETENTION: u64 = 14 * 24 * HOUR;
#[derive(Default)]
struct Policy {
    denied: AtomicBool,
    subscriptions_denied: AtomicBool,
}
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate_subscriptions(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        _: &[GuildEventKind],
    ) -> Result<()> {
        if self.subscriptions_denied.load(Ordering::SeqCst) {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        } else {
            Ok(())
        }
    }

    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        if values["destination"] == "456" && !self.denied.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
}
#[derive(Default)]
struct RemoteState {
    messages: Vec<String>,
    readbacks: usize,
}
struct Transport {
    address: std::net::SocketAddr,
    state: Arc<Mutex<RemoteState>>,
}
impl Transport {
    async fn start(mismatch: bool) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(Mutex::new(RemoteState::default()));
        let transport = Arc::new(Self {
            address: listener.local_addr().unwrap(),
            state: state.clone(),
        });
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = {
                    let mut state = state.lock().unwrap();
                    if request["action"] == "create" {
                        assert_eq!(request["destination"], "456");
                        state
                            .messages
                            .push(request["text"].as_str().unwrap().into());
                        json!({"id":state.messages.len().to_string()})
                    } else {
                        state.readbacks += 1;
                        let index = request["id"].as_str().unwrap().parse::<usize>().unwrap() - 1;
                        json!({"id":request["id"],"destination":"456","text":if mismatch{"wrong remote content".to_owned()}else{state.messages[index].clone()}})
                    }
                };
                stream
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        (transport, server)
    }
    async fn exchange(
        &self,
        value: Value,
        permit: &DispatchPermit,
        check: &dyn NotificationCheck,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let payload = format!("{value}\n");
        let mut sent = 0;
        while sent < payload.len() {
            tokio::select! {biased;_=cancel.cancelled()=>return Err(Error::new(ErrorCode::Cancelled)),ready=stream.writable()=>ready.unwrap()}
            check.validate().await?;
            match permit.dispatch(|| stream.try_write(&payload.as_bytes()[sent..]))? {
                Ok(0) => return Err(Error::new(ErrorCode::UnknownOutcome)),
                Ok(count) => sent += count,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return Err(Error::new(ErrorCode::UnknownOutcome)),
            }
        }
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        Ok(serde_json::from_str(&line).unwrap())
    }
}
#[async_trait::async_trait]
impl NotificationTransport for Transport {
    async fn send(
        &self,
        request: &NotificationRequest,
        permit: &DispatchPermit,
        check: &dyn NotificationCheck,
        cancel: CancellationToken,
    ) -> Result<Value> {
        assert!(request.configuration_revision > 0);
        let receipt = self
            .exchange(
                json!({"action":"create","destination":request.destination,"text":request.text}),
                permit,
                check,
                &cancel,
            )
            .await?;
        // A second request observes remote state; creation acknowledgement is not success evidence.
        let observed = self
            .exchange(
                json!({"action":"read","id":receipt["id"]}),
                permit,
                check,
                &cancel,
            )
            .await?;
        if observed["text"] != request.text || observed["destination"] != request.destination {
            return Err(Error::new(ErrorCode::UnknownOutcome));
        }
        Ok(json!({"message_id":receipt["id"],"verified":true,"readback":observed}))
    }
}
fn event(id: &str, kind: GuildEventKind, at: u64) -> GuildEvent {
    GuildEvent {
        id: id.into(),
        kind,
        occurred_at_ms: at,
        origin: GuildEventOrigin::External,
        subject_id: Some("789".into()),
        actor_id: Some("1234".into()),
        related_id: None,
    }
}
async fn delivered(manager: &ModuleManager, count: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if manager
                .event_health()
                .first()
                .is_some_and(|h| h.delivered >= count)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn status(manager: &ModuleManager, module: &ModuleId, guild: &GuildId) -> Value {
    tokio::time::timeout(
        Duration::from_secs(5),
        manager.invoke(&ACTOR, module, guild, "status", json!({})),
    )
    .await
    .expect("status must not recursively wait on the module RPC lock")
    .unwrap()
}
#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn real_activity_log_configures_delivers_deduplicates_reloads_and_expires_metadata() {
    run(false, false, false).await;
}
#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn real_activity_log_does_not_verify_or_replay_uncertain_delivery() {
    run(true, false, false).await;
}
#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn real_activity_log_maintenance_removes_expired_metadata() {
    run(false, true, false).await;
}
#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn real_activity_log_reports_fresh_host_health_and_preserves_event_attribution() {
    run(false, false, true).await;
}
async fn run(mismatch: bool, retention_only: bool, health_and_origin: bool) {
    let scratch =
        std::env::temp_dir().join(format!("oracle-activity-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: scratch.join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let manager = ModuleManager::new(storage.clone(), core, scratch.join("artifacts")).unwrap();
    let policy = Arc::new(Policy::default());
    manager
        .set_configuration_services(storage.clone(), policy.clone())
        .unwrap();
    let (transport, server) = Transport::start(mismatch).await;
    manager
        .set_event_services(
            BTreeSet::from([
                "guilds".into(),
                "guild_members".into(),
                "guild_moderation".into(),
            ]),
            transport.clone(),
        )
        .unwrap();
    let binary =
        PathBuf::from(std::env::var_os("ORACLE_ACTIVITY_LOG").expect("set ORACLE_ACTIVITY_LOG"));
    let bytes = std::fs::read(binary).unwrap();
    let source = scratch.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/activity-log/manifest.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "activity-log-integration".into(),
        toolchain: "separate executable".into(),
        license: "test".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&source, true).await.unwrap();
    let module = installed.package.manifest.id.clone();
    manager.load(&installed.digest).await.unwrap();
    manager
        .activate(
            &ACTOR,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants: installed.package.manifest.capabilities.clone(),
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let task_manager = manager.clone();
    let task_storage = storage.clone();
    let result = tokio::spawn(async move {
        let manager = task_manager;
        let storage = task_storage;
        let plan = manager
            .configuration_plan(
                &ACTOR,
                &guild,
                &module,
                Some("moderate/v1"),
                json!({"destination":"456","operator_note":"keep this setting"}),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(plan.values["retention_days"], 14);
        assert_eq!(plan.values["retain_message_content"], false);
        assert_eq!(plan.values["queue_limit"], 128);
        let receipt = manager
            .configuration_apply(&ACTOR, &guild, &module, &plan.id)
            .await
            .unwrap();
        assert_eq!(receipt.state, "effective");
        let initial = status(&manager, &module, &guild).await;
        assert_eq!(initial["effective_configuration"]["values"], plan.values);
        assert_eq!(initial["end_to_end_probe_verified"], false);
        assert_eq!(initial["real_moderation_event_observed"], false);
        assert_eq!(initial["state"], "ready");
        assert_eq!(
            initial["host"]["configuration"]["stored"],
            initial["effective_configuration"]
        );
        assert_eq!(initial["host"]["configuration"]["verified"], true);
        assert_eq!(
            initial["host"]["configuration"]["receipt_state"],
            "effective"
        );
        assert_eq!(initial["destination_permissions"]["id"], "456");
        assert_eq!(initial["destination_permissions"]["verified"], true);
        assert_eq!(
            initial["effective_event_subscriptions"],
            initial["requested_event_subscriptions"]
        );
        assert!(
            !initial["effective_event_subscriptions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        if health_and_origin {
            policy.denied.store(true, Ordering::SeqCst);
            let denied = status(&manager, &module, &guild).await;
            assert_eq!(denied["state"], "degraded");
            assert_eq!(denied["destination_permissions"]["verified"], false);
            assert_eq!(denied["host"]["configuration"]["verified"], true);
            assert_eq!(denied["effective_event_subscriptions"], json!([]));
            policy.denied.store(false, Ordering::SeqCst);
            policy.subscriptions_denied.store(true, Ordering::SeqCst);
            let denied = status(&manager, &module, &guild).await;
            assert_eq!(denied["state"], "degraded");
            assert_eq!(denied["host"]["subscriptions"]["ready"], false);
            assert_eq!(denied["destination_permissions"]["verified"], true);
            policy.subscriptions_denied.store(false, Ordering::SeqCst);
            manager
                .set_event_intents(BTreeSet::from(["guilds".into(), "guild_members".into()]))
                .unwrap();
            let missing = status(&manager, &module, &guild).await;
            assert_eq!(missing["state"], "degraded");
            assert_eq!(missing["missing_intents"], json!(["guild_moderation"]));
            assert_eq!(missing["effective_event_subscriptions"], json!([]));
            manager
                .set_event_intents(BTreeSet::from([
                    "guilds".into(),
                    "guild_members".into(),
                    "guild_moderation".into(),
                ]))
                .unwrap();
            assert_eq!(status(&manager, &module, &guild).await["state"], "ready");
            let at = 2 * HOUR;
            let mut unknown = event("raw-unknown", GuildEventKind::ChannelChanged, at);
            unknown.origin = GuildEventOrigin::Unknown;
            unknown.actor_id = None;
            manager.deliver_event(&guild, unknown).await.unwrap();
            delivered(&manager, 1).await;
            let mut own = event("own-audit", GuildEventKind::ModerationAudit, at);
            own.origin = GuildEventOrigin::Oracle;
            manager.deliver_event(&guild, own).await.unwrap();
            delivered(&manager, 2).await;
            manager
                .deliver_event(
                    &guild,
                    event("flush-unknown", GuildEventKind::Maintenance, at + 30_000),
                )
                .await
                .unwrap();
            delivered(&manager, 3).await;
            let unknown_status = status(&manager, &module, &guild).await;
            assert_eq!(unknown_status["observed_metadata_events"], 1);
            assert_eq!(unknown_status["unknown_origin_observations"], 1);
            assert_eq!(
                unknown_status["unattributed_administrative_observations"],
                1
            );
            assert_eq!(unknown_status["real_moderation_event_observed"], false);
            assert!(transport.state.lock().unwrap().messages.is_empty());
            let bucket = storage
                .document_get(&module, &guild, "logging", "hour:2")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bucket.value["records"].as_array().unwrap().len(), 1);
            assert_eq!(bucket.value["records"][0]["origin"], "unknown");
            manager
                .deliver_event(
                    &guild,
                    event(
                        "external-audit",
                        GuildEventKind::ModerationAudit,
                        at + 30_000,
                    ),
                )
                .await
                .unwrap();
            delivered(&manager, 4).await;
            manager
                .deliver_event(
                    &guild,
                    event("flush-external", GuildEventKind::Maintenance, at + 60_000),
                )
                .await
                .unwrap();
            delivered(&manager, 5).await;
            let observed = status(&manager, &module, &guild).await;
            assert_eq!(observed["observed_metadata_events"], 2);
            assert_eq!(observed["real_moderation_event_observed"], true);
            assert_eq!(observed["delivered"], 1);
            assert_eq!(transport.state.lock().unwrap().messages.len(), 1);
            assert_eq!(transport.state.lock().unwrap().readbacks, 1);
            // Persist the pre-attribution document shape, then load it through the
            // separate module executable rather than a domain-only serde test.
            let saved = storage
                .document_get(&module, &guild, "logging", "state")
                .await
                .unwrap()
                .unwrap();
            let mut legacy_state = saved.value;
            for field in [
                "unknown_origin_observations",
                "unattributed_administrative_observations",
                "unknown_audit_actor_events",
            ] {
                legacy_state.as_object_mut().unwrap().remove(field);
            }
            let bucket = storage
                .document_get(&module, &guild, "logging", "hour:2")
                .await
                .unwrap()
                .unwrap();
            let mut legacy_bucket = bucket.value;
            for record in legacy_bucket["records"].as_array_mut().unwrap() {
                record.as_object_mut().unwrap().remove("origin");
            }
            storage
                .document_batch(
                    &module,
                    &guild,
                    1,
                    &[
                        DocumentWrite {
                            collection: "logging".into(),
                            key: "state".into(),
                            expected_revision: Some(saved.revision),
                            value: Some(legacy_state),
                        },
                        DocumentWrite {
                            collection: "logging".into(),
                            key: "hour:2".into(),
                            expected_revision: Some(bucket.revision),
                            value: Some(legacy_bucket),
                        },
                    ],
                )
                .await
                .unwrap();
            manager
                .unload(&module, Duration::from_secs(2))
                .await
                .unwrap();
            manager.load(&installed.digest).await.unwrap();
            let compatible = status(&manager, &module, &guild).await;
            assert_eq!(compatible["state"], "ready");
            assert_eq!(compatible["observed_metadata_events"], 2);
            assert_eq!(compatible["unknown_origin_observations"], 0);
            manager
                .deliver_event(
                    &guild,
                    event("legacy-flush", GuildEventKind::Maintenance, at + 90_000),
                )
                .await
                .unwrap();
            delivered(&manager, 1).await;
            assert_eq!(transport.state.lock().unwrap().messages.len(), 1);
            return;
        }
        let probe = manager
            .invoke(&ACTOR, &module, &guild, "probe", json!({}))
            .await;
        if mismatch {
            assert!(probe.is_err());
            assert_eq!(transport.state.lock().unwrap().messages.len(), 1);
            assert_eq!(transport.state.lock().unwrap().readbacks, 1);
            assert_eq!(
                status(&manager, &module, &guild).await["end_to_end_probe_verified"],
                false
            );
            assert!(
                manager
                    .invoke(&ACTOR, &module, &guild, "probe", json!({}))
                    .await
                    .is_err()
            );
            assert_eq!(
                transport.state.lock().unwrap().messages.len(),
                1,
                "unknown effect must not be blindly resent"
            );
            let recovery = storage.recovery(&guild, 100).await.unwrap();
            assert_eq!(recovery.len(), 1);
            assert_eq!(recovery[0].state, EffectState::Unknown);
            return;
        }
        let probe = probe.unwrap();
        assert_eq!(probe["verified"], true);
        let effect_id =
            EffectId::new(probe["receipt"]["host_effect_id"].as_str().unwrap()).unwrap();
        let verified_effect = storage.effect(&guild, &effect_id).await.unwrap();
        assert_eq!(verified_effect.state, EffectState::Verified);
        assert!(
            verified_effect
                .purpose
                .starts_with(&format!("module:{module}:notify:"))
        );
        manager
            .invoke(&ACTOR, &module, &guild, "probe", json!({}))
            .await
            .unwrap();
        assert_eq!(
            transport.state.lock().unwrap().messages.len(),
            1,
            "verified probe uses durable receipt"
        );
        assert_eq!(
            status(&manager, &module, &guild).await["real_moderation_event_observed"],
            false,
            "a synthetic probe is not a moderation event"
        );
        let at = 2 * HOUR;
        let incoming = event("audit-1", GuildEventKind::ModerationAudit, at);
        assert_eq!(
            manager
                .deliver_event(&guild, incoming.clone())
                .await
                .unwrap()
                .accepted,
            1
        );
        delivered(&manager, 1).await;
        manager
            .deliver_event(&guild, incoming.clone())
            .await
            .unwrap();
        delivered(&manager, 2).await;
        assert_eq!(
            status(&manager, &module, &guild).await["observed_metadata_events"],
            1
        );
        assert_eq!(
            transport.state.lock().unwrap().messages.len(),
            1,
            "coalescing delays event notification"
        );
        manager
            .deliver_event(
                &guild,
                event("flush", GuildEventKind::Maintenance, at + 30_000),
            )
            .await
            .unwrap();
        delivered(&manager, 3).await;
        let state = status(&manager, &module, &guild).await;
        assert_eq!(state["delivered"], 1);
        assert_eq!(state["delivery_backlog"], 0);
        assert_eq!(state["real_moderation_event_observed"], true);
        assert_eq!(transport.state.lock().unwrap().messages.len(), 2);
        assert_eq!(transport.state.lock().unwrap().readbacks, 2);
        let bucket = storage
            .document_get(&module, &guild, "logging", "hour:2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bucket.value["records"].as_array().unwrap().len(), 1);
        if retention_only {
            manager
                .deliver_event(
                    &guild,
                    event("expire", GuildEventKind::Maintenance, at + RETENTION),
                )
                .await
                .unwrap();
            delivered(&manager, 4).await;
            assert!(
                storage
                    .document_get(&module, &guild, "logging", "hour:2")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                status(&manager, &module, &guild).await["retention_last_run_ms"],
                at + RETENTION
            );
            assert_eq!(transport.state.lock().unwrap().messages.len(), 2);
            return;
        }
        let previous = manager.catalog(&ACTOR, &guild).await.unwrap();
        manager
            .unload(&module, Duration::from_secs(2))
            .await
            .unwrap();
        manager.load(&installed.digest).await.unwrap();
        let current = manager.catalog(&ACTOR, &guild).await.unwrap();
        assert_eq!(current.len(), 1, "activation is restored by load");
        assert_eq!(current[0].module, module);
        assert_ne!(current[0].session, previous[0].session);
        let reloaded = status(&manager, &module, &guild).await;
        assert_eq!(reloaded["effective_configuration"]["values"], plan.values);
        assert_eq!(reloaded["observed_metadata_events"], 1);
        assert_eq!(reloaded["end_to_end_probe_verified"], true);
        manager.deliver_event(&guild, incoming).await.unwrap();
        delivered(&manager, 1).await;
        manager
            .deliver_event(
                &guild,
                event("flush-replay", GuildEventKind::Maintenance, at + 60_000),
            )
            .await
            .unwrap();
        delivered(&manager, 2).await;
        assert_eq!(
            transport.state.lock().unwrap().messages.len(),
            2,
            "replayed event stays deduplicated after module process replacement"
        );
        manager
            .deliver_event(
                &guild,
                event("expire", GuildEventKind::Maintenance, at + RETENTION),
            )
            .await
            .unwrap();
        delivered(&manager, 3).await;
        assert!(
            storage
                .document_get(&module, &guild, "logging", "hour:2")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            status(&manager, &module, &guild).await["retention_last_run_ms"],
            at + RETENTION
        );
        assert!(storage.recovery(&guild, 100).await.unwrap().is_empty());
    })
    .await;
    let stopped = manager.shutdown().await;
    let closed = storage.close().await;
    server.abort();
    let _ = server.await;
    remove_tree(&scratch);
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
    stopped.unwrap();
    closed.unwrap();
}
fn remove_tree(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                remove_tree(&entry.path());
            }
        }
        let _ = std::fs::remove_dir(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}
