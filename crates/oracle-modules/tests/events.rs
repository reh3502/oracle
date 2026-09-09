//! Real subprocess event admission with controlled host transport readiness.
//! Optional ORACLE_TEST_EVENTS_POSTGRES_URL selects an isolated PostgreSQL database.
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
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
struct Policy;
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        if values["destination"] == "456" {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
}
struct Transport {
    entered: Notify,
    ready: Semaphore,
    sent: Mutex<Vec<String>>,
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
        self.entered.notify_one();
        tokio::select! {biased;_=cancel.cancelled()=>return Err(Error::new(ErrorCode::Cancelled)),ready=self.ready.acquire()=>ready.unwrap().forget()}
        check.validate().await?;
        permit.dispatch(|| self.sent.lock().unwrap().push(request.text.clone()))?;
        Ok(json!({"id":self.sent.lock().unwrap().len(),"destination":request.destination}))
    }
}
const ACTOR: PolicyContext = PolicyContext::LocalOperator;
fn intents() -> BTreeSet<String> {
    BTreeSet::from(["guilds".into(), "guild_members".into()])
}
fn event(id: &str) -> GuildEvent {
    GuildEvent {
        id: id.into(),
        kind: GuildEventKind::ChannelChanged,
        occurred_at_ms: 1000,
        origin: GuildEventOrigin::External,
        subject_id: Some("789".into()),
        actor_id: None,
        related_id: None,
    }
}
#[tokio::test]
#[ignore = "requires ORACLE_EVENT_PROBE built with configuration-probe --features events"]
async fn event_queue_is_bounded_and_unload_fences_waiting_notification() {
    run("fence").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_EVENT_PROBE built with configuration-probe --features events"]
async fn event_notification_rechecks_configuration_revision_after_readiness() {
    run("configuration").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_EVENT_PROBE built with configuration-probe --features events"]
async fn event_notification_rechecks_intents_after_readiness() {
    run("intents").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_EVENT_PROBE built with configuration-probe --features events"]
async fn event_routes_require_configuration_and_reuse_verified_effect_receipts() {
    run("routing").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ORACLE_EVENT_PROBE and ORACLE_COMMAND_COLLISION_PROBE"]
async fn command_namespace_collision_is_refused_before_activation_and_scoped_to_guild() {
    run("collision").await;
}
async fn run(case: &'static str) {
    let scratch = std::env::temp_dir().join(format!("oracle-event-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let database = match std::env::var("ORACLE_TEST_EVENTS_POSTGRES_URL") {
        Ok(url) => DatabaseConfig::Postgres { url },
        Err(_) => DatabaseConfig::Sqlite {
            path: scratch.join("state.sqlite"),
        },
    };
    let storage = Arc::new(Storage::open(database.clone()).await.unwrap());
    let guild: GuildId = "123".parse().unwrap();
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    storage
        .initialize_guilds(&["124".parse().unwrap()])
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![
            GuildPolicy {
                guild: guild.clone(),
                operators: vec![],
            },
            GuildPolicy {
                guild: "124".parse().unwrap(),
                operators: vec![],
            },
        ],
    ));
    let manager = ModuleManager::new(storage.clone(), core, scratch.join("artifacts")).unwrap();
    manager
        .set_configuration_services(storage.clone(), Arc::new(Policy))
        .unwrap();
    let transport = Arc::new(Transport {
        entered: Notify::new(),
        ready: Semaphore::new(0),
        sent: Mutex::new(vec![]),
    });
    manager
        .set_event_services(BTreeSet::new(), transport.clone())
        .unwrap();
    let binary =
        PathBuf::from(std::env::var_os("ORACLE_EVENT_PROBE").expect("set ORACLE_EVENT_PROBE"));
    let bytes = std::fs::read(binary).unwrap();
    let source = scratch.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest-events.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "event-fixture".into(),
        toolchain: "separate feature artifact".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&source, true).await.unwrap();
    let module = installed.package.manifest.id;
    manager.load(&installed.digest).await.unwrap();
    let desired = DesiredActivation {
        module: module.clone(),
        guild: guild.clone(),
        active: true,
        grants: vec![
            "config.own".into(),
            "events.guild".into(),
            "discord.notify".into(),
        ],
        bindings: BTreeMap::new(),
    };
    assert!(manager.activate(&ACTOR, desired.clone()).await.is_err());
    manager.set_event_intents(intents()).unwrap();
    manager.activate(&ACTOR, desired).await.unwrap();
    let task_manager = manager.clone();
    let task_scratch = scratch.clone();
    let task_storage = storage.clone();
    let task = tokio::spawn(async move {
        let manager = task_manager;
        if case == "routing" {
            assert_eq!(
                manager
                    .deliver_event(&guild, event("unconfigured"))
                    .await
                    .unwrap()
                    .accepted,
                1
            );
            wait_health(&manager, |delivered, dropped| {
                delivered == 0 && dropped == 1
            })
            .await;
            assert!(transport.sent.lock().unwrap().is_empty());
        }
        let plan = manager
            .configuration_plan(
                &ACTOR,
                &guild,
                &module,
                Some("moderate/v1"),
                json!({"destination":"456"}),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .configuration_apply(&ACTOR, &guild, &module, &plan.id)
                .await
                .unwrap()
                .state,
            "effective"
        );
        assert!(
            manager
                .invoke(&ACTOR, &module, &guild, "event.deliver", json!({}))
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .deliver_event(&guild, event("first"))
                .await
                .unwrap()
                .accepted,
            1
        );
        tokio::time::timeout(Duration::from_secs(5), transport.entered.notified())
            .await
            .unwrap();
        match case {
            "collision" => {
                transport.ready.add_permits(1);
                wait_health(&manager, |delivered, _| delivered == 1).await;
                let path = PathBuf::from(
                    std::env::var_os("ORACLE_COMMAND_COLLISION_PROBE")
                        .expect("set ORACLE_COMMAND_COLLISION_PROBE"),
                );
                let bytes = std::fs::read(path).unwrap();
                let source = task_scratch.join("collision-source");
                std::fs::create_dir(&source).unwrap();
                std::fs::write(source.join("module"), &bytes).unwrap();
                let package = ModulePackage {
                    manifest: serde_json::from_str(include_str!(
                        "../../../examples/modules/configuration-probe/manifest-collision.json"
                    ))
                    .unwrap(),
                    entrypoint: "module".into(),
                    files: BTreeMap::from([(
                        "module".into(),
                        format!("{:x}", Sha256::digest(bytes)),
                    )]),
                    source_revision: "collision-fixture".into(),
                    toolchain: "separate feature artifact".into(),
                    license: "test-only".into(),
                };
                std::fs::write(
                    source.join("package.json"),
                    serde_json::to_vec(&package).unwrap(),
                )
                .unwrap();
                let installed = manager.install(&source, true).await.unwrap();
                let second = installed.package.manifest.id.clone();
                manager.load(&installed.digest).await.unwrap();
                let desired = DesiredActivation {
                    module: second.clone(),
                    guild: guild.clone(),
                    active: true,
                    grants: vec![
                        "config.own".into(),
                        "events.guild".into(),
                        "discord.notify".into(),
                    ],
                    bindings: BTreeMap::new(),
                };
                assert_eq!(
                    manager
                        .activate(&ACTOR, desired.clone())
                        .await
                        .unwrap_err()
                        .code,
                    ErrorCode::Conflict
                );
                assert!(
                    !task_storage
                        .desired_activations()
                        .await
                        .unwrap()
                        .iter()
                        .any(|a| a.module == second)
                );
                let other: GuildId = "124".parse().unwrap();
                manager
                    .activate(
                        &ACTOR,
                        DesiredActivation {
                            guild: other.clone(),
                            ..desired
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    manager.catalog(&ACTOR, &guild).await.unwrap()[0].module,
                    module
                );
                assert_eq!(
                    manager.catalog(&ACTOR, &other).await.unwrap()[0].module,
                    second
                );
            }
            "fence" => {
                let mut accepted = 0;
                let mut dropped = 0;
                for n in 0..129 {
                    let out = manager
                        .deliver_event(&guild, event(&format!("queued-{n}")))
                        .await
                        .unwrap();
                    accepted += out.accepted;
                    dropped += out.dropped;
                }
                assert_eq!(accepted, 128);
                assert_eq!(dropped, 1);
                assert_eq!(manager.event_health()[0].queued, 128);
                manager.unload(&module, Duration::ZERO).await.unwrap();
                transport.ready.add_permits(1);
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(transport.sent.lock().unwrap().is_empty());
                assert_eq!(
                    manager
                        .deliver_event(&guild, event("after-unload"))
                        .await
                        .unwrap()
                        .accepted,
                    0
                );
            }
            "configuration" => {
                let change = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"level":9}),
                        Duration::from_secs(60),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &change.id)
                        .await
                        .unwrap()
                        .stored_revision,
                    2
                );
                transport.ready.add_permits(1);
                wait_health(&manager, |_, dropped| dropped == 1).await;
                assert!(transport.sent.lock().unwrap().is_empty());
                transport.ready.add_permits(1);
                manager
                    .deliver_event(&guild, event("new-config"))
                    .await
                    .unwrap();
                wait_health(&manager, |delivered, _| delivered == 1).await;
                assert_eq!(
                    transport.sent.lock().unwrap().as_slice(),
                    ["event new-config"]
                );
            }
            "intents" => {
                manager.set_event_intents(BTreeSet::new()).unwrap();
                transport.ready.add_permits(1);
                wait_health(&manager, |_, dropped| dropped == 1).await;
                assert!(transport.sent.lock().unwrap().is_empty());
                assert_eq!(
                    manager
                        .deliver_event(&guild, event("missing"))
                        .await
                        .unwrap()
                        .unavailable,
                    1
                );
                assert!(!manager.event_health()[0].missing_intents.is_empty());
            }
            "routing" => {
                transport.ready.add_permits(1);
                wait_health(&manager, |delivered, _| delivered == 1).await;
                manager.deliver_event(&guild, event("first")).await.unwrap();
                wait_health(&manager, |delivered, _| delivered == 2).await;
                assert_eq!(transport.sent.lock().unwrap().as_slice(), ["event first"]);
                let catalog = manager.catalog(&ACTOR, &guild).await.unwrap();
                assert_eq!(catalog.len(), 1);
                let binding = &catalog[0];
                assert_eq!(
                    binding
                        .operations
                        .iter()
                        .map(|op| op.name.as_str())
                        .collect::<Vec<_>>(),
                    ["status"]
                );
                assert_eq!(
                    manager
                        .invoke_bound(
                            &ACTOR,
                            &guild,
                            &module,
                            "status",
                            json!({}),
                            &binding.session,
                            binding.generation,
                            binding.epoch
                        )
                        .await
                        .unwrap()["epoch"],
                    binding.epoch
                );
                assert!(
                    manager
                        .invoke_bound(
                            &ACTOR,
                            &guild,
                            &module,
                            "status",
                            json!({}),
                            &binding.session,
                            binding.generation,
                            binding.epoch + 1
                        )
                        .await
                        .is_err()
                );
                let snapshot = manager.catalog_snapshot(&ACTOR, &guild).await.unwrap();
                let permit = manager.registry_permit(snapshot.revision);
                let mut changes = manager.registry_changes();
                changes.borrow_and_update();
                manager
                    .deactivate(&ACTOR, &module, &guild, Duration::from_secs(1))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(1), changes.changed())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(manager.registry_revision() > snapshot.revision);
                let sent = std::sync::atomic::AtomicUsize::new(0);
                assert!(
                    permit
                        .dispatch(|| sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                        .is_err()
                );
                assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert!(
                    manager
                        .catalog_snapshot(&ACTOR, &guild)
                        .await
                        .unwrap()
                        .entries
                        .is_empty()
                );
                manager
                    .activate(
                        &ACTOR,
                        DesiredActivation {
                            module: module.clone(),
                            guild: guild.clone(),
                            active: true,
                            grants: vec![
                                "config.own".into(),
                                "events.guild".into(),
                                "discord.notify".into(),
                            ],
                            bindings: BTreeMap::new(),
                        },
                    )
                    .await
                    .unwrap();
                assert!(
                    manager
                        .invoke_bound(
                            &ACTOR,
                            &guild,
                            &module,
                            "status",
                            json!({}),
                            &binding.session,
                            binding.generation,
                            binding.epoch
                        )
                        .await
                        .is_err()
                );
                let current = manager.catalog(&ACTOR, &guild).await.unwrap();
                assert_eq!(current[0].generation, binding.generation);
                assert_ne!(current[0].epoch, binding.epoch);
                manager
                    .unload(&module, Duration::from_secs(1))
                    .await
                    .unwrap();
                manager.load(&installed.digest).await.unwrap();
                assert!(
                    manager
                        .invoke_bound(
                            &ACTOR,
                            &guild,
                            &module,
                            "status",
                            json!({}),
                            &current[0].session,
                            current[0].generation,
                            current[0].epoch
                        )
                        .await
                        .is_err()
                );

                let mut invalid = event("invalid");
                invalid.subject_id = Some("message body should never become metadata".into());
                assert!(manager.deliver_event(&guild, invalid).await.is_err());
                let foreign: GuildId = "999".parse().unwrap();
                assert!(
                    manager
                        .deliver_event(&foreign, event("foreign"))
                        .await
                        .is_err()
                );
            }
            _ => unreachable!(),
        }
    })
    .await;
    let stopped = manager.shutdown().await;
    let closed = storage.close().await;
    if task.is_ok() && case == "routing" {
        stopped.as_ref().unwrap();
        closed.as_ref().unwrap();
        // Reopen the actual backend and rebuild the host. The verified event purpose
        // must resolve to its prior receipt without dispatching another notification.
        let reopened = Arc::new(Storage::open(database).await.unwrap());
        let guild: GuildId = "123".parse().unwrap();
        let core = Arc::new(CoreService::new(
            reopened.clone(),
            vec![GuildPolicy {
                guild: guild.clone(),
                operators: vec![],
            }],
        ));
        let restarted =
            ModuleManager::new(reopened.clone(), core, scratch.join("artifacts")).unwrap();
        restarted
            .set_configuration_services(reopened.clone(), Arc::new(Policy))
            .unwrap();
        let no_resend = Arc::new(Transport {
            entered: Notify::new(),
            ready: Semaphore::new(0),
            sent: Mutex::new(vec![]),
        });
        restarted
            .set_event_services(intents(), no_resend.clone())
            .unwrap();
        let worker = restarted.clone();
        let recovery = tokio::spawn(async move {
            assert!(worker.restore_desired().await.unwrap().is_empty());
            assert_eq!(
                worker
                    .deliver_event(&guild, event("first"))
                    .await
                    .unwrap()
                    .accepted,
                1
            );
            wait_health(&worker, |delivered, _| delivered == 1).await;
            assert!(no_resend.sent.lock().unwrap().is_empty());
        })
        .await;
        let stopped = restarted.shutdown().await;
        let closed = reopened.close().await;
        if let Err(error) = recovery {
            remove_tree(&scratch);
            std::panic::resume_unwind(error.into_panic());
        }
        stopped.unwrap();
        closed.unwrap();
    }
    remove_tree(&scratch);
    if let Err(error) = task {
        std::panic::resume_unwind(error.into_panic());
    }
    stopped.unwrap();
    closed.unwrap();
}
async fn wait_health(manager: &ModuleManager, done: impl Fn(usize, usize) -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .event_health()
                .first()
                .is_some_and(|h| done(h.delivered, h.dropped))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "event health wait failed: {:?}",
        manager.event_health()
    );
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
