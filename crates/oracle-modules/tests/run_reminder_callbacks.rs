//! Actual native callbacks prove maintenance authority and authoritative document reads.
use oracle_core::{member_mutation::MemberMutationPolicy, member_read::MemberContext, *};
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, ModuleManager, NotificationCheck, NotificationRequest,
    NotificationTransport, SharedCardService,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct NoSend;
#[async_trait::async_trait]
impl NotificationTransport for NoSend {
    async fn send(
        &self,
        _: &NotificationRequest,
        _: &DispatchPermit,
        _: &dyn NotificationCheck,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        panic!("reminder callback authority tests must not send notifications")
    }
}
struct Policy;
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate_subscriptions(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        _: &[GuildEventKind],
    ) -> Result<()> {
        Ok(())
    }
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        _: &Value,
    ) -> Result<()> {
        Ok(())
    }
}
struct Service {
    calls: Mutex<Vec<(ModuleId, GuildId, String, Value)>>,
    hold: AtomicBool,
    entered: Notify,
    release: Semaphore,
}
#[async_trait::async_trait]
impl SharedCardService for Service {
    async fn enqueue(&self, _: &ModuleId, _: &GuildId, _: Value) -> Result<Value> {
        unreachable!()
    }
    async fn status(&self, _: &ModuleId, _: &GuildId, _: &str) -> Result<Value> {
        unreachable!()
    }
    async fn run_reminder(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        key: &str,
        document: Value,
    ) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((module.clone(), guild.clone(), key.into(), document));
        self.entered.notify_one();
        if self.hold.load(Ordering::SeqCst) {
            self.release.acquire().await.unwrap().forget();
        }
        Ok(json!({"state":"confirmed"}))
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
async fn deliver(manager: &Arc<ModuleManager>, guild: &GuildId, id: &str) {
    assert_eq!(
        manager
            .deliver_event(
                guild,
                GuildEvent {
                    id: id.into(),
                    kind: GuildEventKind::Maintenance,
                    occurred_at_ms: now(),
                    origin: GuildEventOrigin::Unknown,
                    subject_id: None,
                    actor_id: None,
                    related_id: None
                }
            )
            .await
            .unwrap()
            .accepted,
        1
    );
}
async fn outcome(storage: &Storage, module: &ModuleId, guild: &GuildId, key: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(doc) = storage
                .document_get(module, guild, "results", key)
                .await
                .unwrap()
            {
                return doc.value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker must record its callback result")
}
#[tokio::test]
#[ignore = "requires RUN_REMINDER_PROBE_BINARY; build tests/fixtures/run-reminders first"]
async fn run_reminder_callbacks_require_worker_and_current_document_revision() {
    let temp = Temp(std::env::temp_dir().join(format!(
        "oracle-reminder-callbacks-{}",
        uuid::Uuid::new_v4()
    )));
    std::fs::create_dir(&temp.0).unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: temp.0.join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let module: ModuleId = "fixture.run-reminders".parse().unwrap();
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
    let manager = ModuleManager::new(storage.clone(), core, temp.0.join("artifacts")).unwrap();
    manager
        .set_configuration_services(storage.clone(), Arc::new(Policy))
        .unwrap();
    manager
        .set_event_services(BTreeSet::from(["guilds".into()]), Arc::new(NoSend))
        .unwrap();
    let service = Arc::new(Service {
        calls: Mutex::new(vec![]),
        hold: AtomicBool::new(false),
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    manager.set_shared_card_service(service.clone()).unwrap();
    let source = temp.0.join("package");
    std::fs::create_dir(&source).unwrap();
    let bytes = std::fs::read(
        std::env::var_os("RUN_REMINDER_PROBE_BINARY").expect("set RUN_REMINDER_PROBE_BINARY"),
    )
    .unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let manifest: ModuleManifest =
        serde_json::from_str(include_str!("fixtures/run-reminders/manifest.json")).unwrap();
    let grants = manifest.capabilities.clone();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(&bytes)))]),
        source_revision: "reminder-callback-test".into(),
        toolchain: "native fixture".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&source, true).await.unwrap();
    manager.load(&installed.digest).await.unwrap();
    manager
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants,
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let plan = manager
        .configuration_plan(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            None,
            json!({}),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .configuration_apply(&PolicyContext::LocalOperator, &guild, &module, &plan.id)
            .await
            .unwrap()
            .state,
        "effective"
    );
    manager
        .configure_member_mutations(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(MemberMutationPolicy::default()),
        )
        .await
        .unwrap();
    let document = json!({"run":{"owner":"900","participants":["901"],"authoritative_marker":"full document"},"intent":{"key":"run","desired_revision":1,"destination":"runs","created_at":1,"card":{},"actions":[]}});
    storage
        .document_batch(
            &module,
            &guild,
            1,
            &[DocumentWrite {
                collection: "runs".into(),
                key: "run".into(),
                expected_revision: None,
                value: Some(document.clone()),
            }],
        )
        .await
        .unwrap();
    deliver(&manager, &guild, "success").await;
    assert_eq!(
        outcome(&storage, &module, &guild, "success").await,
        json!({"accepted":true,"result":{"state":"confirmed"}})
    );
    assert_eq!(
        *service.calls.lock().unwrap(),
        vec![(module.clone(), guild.clone(), "run".into(), document)]
    );
    deliver(&manager, &guild, "stale").await;
    assert_eq!(
        outcome(&storage, &module, &guild, "stale").await["accepted"],
        false
    );
    assert_eq!(
        service.calls.lock().unwrap().len(),
        1,
        "stale revision must never reach service"
    );
    let binding = manager
        .catalog(&PolicyContext::LocalOperator, &guild)
        .await
        .unwrap()
        .remove(0);
    let actor = MemberContext {
        guild: guild.clone(),
        user: "900".parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::new(),
        observed_at: Instant::now(),
    };
    let interaction = ((now() - 1_420_070_400_000 - 1000) << 22).to_string();
    let denied = manager
        .invoke_member_mutation_bound(
            &actor,
            &interaction,
            &guild,
            &module,
            "mutation_probe",
            json!({}),
            &binding.session,
            binding.generation,
            binding.epoch,
        )
        .await
        .unwrap();
    assert_eq!(denied.value["accepted"], false);
    assert_eq!(
        service.calls.lock().unwrap().len(),
        1,
        "member callback must never reach service"
    );
    // Revoke policy while an authenticated callback is in flight. Neither its response
    // nor a subsequent document write may survive the old worker lease.
    service.entered.notified().await; // consume the successful call's notification
    service.hold.store(true, Ordering::SeqCst);
    deliver(&manager, &guild, "revoked").await;
    tokio::time::timeout(Duration::from_secs(5), service.entered.notified())
        .await
        .unwrap();
    manager.invalidate_member_mutations(&guild);
    service.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .event_health()
                .iter()
                .any(|h| h.delivered + h.dropped >= 3)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revoked event should settle");
    assert!(
        storage
            .document_get(&module, &guild, "results", "revoked")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        manager
            .invoke_member_mutation_bound(
                &actor,
                &interaction,
                &guild,
                &module,
                "mutation_probe",
                json!({}),
                &binding.session,
                binding.generation,
                binding.epoch
            )
            .await
            .is_err()
    );
    manager.shutdown().await.unwrap();
}
