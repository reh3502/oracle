//! Real DW subprocess qualification. Requires a built module and the complete
//! normalized run-eligibility catalog; no network or Discord transport is used.
use oracle_core::{
    member_mutation::MemberMutationPolicy,
    member_read::{MemberContext, MemberReadPolicy},
    *,
};
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, ModuleCatalogEntry, ModuleManager, NotificationCheck,
    NotificationRequest, NotificationTransport, runtime_settings::ModuleRuntimeSettings,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
const PREFIX: &str = "https://dandys-world-robloxhorror.fandom.com/index.php?oldid=";
const DAY: u64 = 86_400_000;
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn interaction() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    (((now() - 1_420_070_400_000 - 1000) << 22) + SEQ.fetch_add(1, Ordering::Relaxed)).to_string()
}
fn member(guild: &GuildId, user: &str) -> MemberContext {
    MemberContext {
        guild: guild.clone(),
        user: user.parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::new(),
        observed_at: Instant::now(),
    }
}
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
        if values == &json!({}) || values == &json!({"limits":{"drafts_per_guild":0}}) {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
    async fn validate_subscriptions(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        events: &[GuildEventKind],
    ) -> Result<()> {
        if events == [GuildEventKind::Maintenance] {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
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
        _: CancellationToken,
    ) -> Result<Value> {
        panic!("Stage 6 must never send a Discord message")
    }
}
fn snapshot(directory: &Path, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    fs::write(directory.join(format!("{digest}.json")), bytes).unwrap();
    fs::write(directory.join("active"), digest).unwrap();
}
async fn call(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    id: &str,
    op: &str,
    input: Value,
) -> Result<Value> {
    manager
        .invoke_member_mutation_bound(
            &member(guild, user),
            id,
            guild,
            &binding.module,
            op,
            input,
            &binding.session,
            binding.generation,
            binding.epoch,
        )
        .await
        .map(|reply| reply.value)
}
async fn change(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    id: &str,
    command: Value,
) -> Value {
    call(
        manager,
        guild,
        binding,
        user,
        &interaction(),
        "run_change",
        json!({"id":id,"command":command}),
    )
    .await
    .unwrap()
}
async fn binding(manager: &ModuleManager, guild: &GuildId) -> ModuleCatalogEntry {
    manager
        .catalog(&PolicyContext::LocalOperator, guild)
        .await
        .unwrap()
        .remove(0)
}
fn activation(module: &ModuleId, guild: &GuildId) -> DesiredActivation {
    DesiredActivation {
        module: module.clone(),
        guild: guild.clone(),
        active: true,
        grants: vec![
            "storage.own".into(),
            "config.own".into(),
            "events.guild".into(),
        ],
        bindings: BTreeMap::new(),
    }
}
async fn configure(manager: &ModuleManager, guild: &GuildId, module: &ModuleId) {
    manager
        .configure_member_mutations(
            &PolicyContext::LocalOperator,
            guild,
            module,
            Some(MemberMutationPolicy {
                channels: BTreeSet::from(["700".into()]),
                permission_roles: BTreeMap::from([(
                    "manage_all_runs".into(),
                    BTreeSet::from(["800".into()]),
                )]),
                per_user_per_minute: 60,
                per_guild_per_minute: 200,
            }),
        )
        .await
        .unwrap();
    let plan = manager
        .configuration_plan(
            &PolicyContext::LocalOperator,
            guild,
            module,
            None,
            json!({}),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .configuration_apply(&PolicyContext::LocalOperator, guild, module, &plan.id)
            .await
            .unwrap()
            .state,
        "effective"
    );
}
#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY and DW_RUN_CATALOG; run explicitly with --ignored"]
async fn native_runs_sqlite() {
    qualify(false).await;
}
#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY, DW_RUN_CATALOG and isolated ORACLE_TEST_DW_RUNS_POSTGRES_URL"]
async fn native_runs_postgres() {
    qualify(true).await;
}
async fn qualify(postgres: bool) {
    let temp = Temp(std::env::temp_dir().join(format!("oracle-dw-runs-{}", uuid::Uuid::new_v4())));
    fs::DirBuilder::new().mode(0o700).create(&temp.0).unwrap();
    let storage = Arc::new(
        Storage::open(if postgres {
            DatabaseConfig::Postgres {
                url: std::env::var("ORACLE_TEST_DW_RUNS_POSTGRES_URL")
                    .expect("isolated PostgreSQL database required"),
            }
        } else {
            DatabaseConfig::Sqlite {
                path: temp.0.join("state.sqlite"),
            }
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let other: GuildId = "456".parse().unwrap();
    let module: ModuleId = "community.dandys-world".parse().unwrap();
    storage
        .initialize_guilds(&[guild.clone(), other.clone()])
        .await
        .unwrap();
    // Begin from Stage 5's empty module namespace, independently of the wiki catalog.
    let digest = "a".repeat(64);
    storage
        .begin_migration(&module, &guild, 0, 1, &digest)
        .await
        .unwrap();
    assert!(
        storage
            .migration_page(&module, &guild, 1)
            .await
            .unwrap()
            .documents
            .is_empty()
    );
    storage
        .commit_migration_page(&module, &guild, &digest, None, &[], None, true)
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
                guild: other.clone(),
                operators: vec![],
            },
        ],
    ));
    let manager = ModuleManager::new(storage.clone(), core, temp.0.join("artifacts")).unwrap();
    manager
        .set_configuration_services(storage.clone(), Arc::new(Policy))
        .unwrap();
    manager
        .set_event_services(BTreeSet::from(["guilds".into()]), Arc::new(NoSend))
        .unwrap();
    let package_dir = temp.0.join("package");
    fs::create_dir(&package_dir).unwrap();
    let bytes =
        fs::read(std::env::var_os("DW_MODULE_BINARY").expect("built DW module required")).unwrap();
    fs::write(package_dir.join("module"), &bytes).unwrap();
    let manifest: ModuleManifest =
        serde_json::from_str(include_str!("../../../modules/dandys-world/manifest.json")).unwrap();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(&bytes)))]),
        source_revision: "dw-run-native-test".into(),
        toolchain: "real DW SDK executable".into(),
        license: "test fixture".into(),
    };
    fs::write(
        package_dir.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&package_dir, true).await.unwrap();
    let data = temp.0.join("catalog");
    manager
        .configure_runtime_settings(
            &PolicyContext::LocalOperator,
            BTreeMap::from([(
                module.clone(),
                ModuleRuntimeSettings {
                    image_prefix: None,
                    data_directory: Some(data.clone()),
                    citation_prefix: Some(PREFIX.into()),
                },
            )]),
            &[temp.0.join("state.sqlite")],
            fs::metadata(&temp.0).unwrap().uid(),
        )
        .await
        .unwrap();
    let catalog: Value = serde_json::from_slice(
        &fs::read(
            std::env::var_os("DW_RUN_CATALOG").expect("complete normalized catalog required"),
        )
        .unwrap(),
    )
    .unwrap();
    snapshot(&data, &catalog);
    manager.load(&installed.digest).await.unwrap();
    manager
        .activate(&PolicyContext::LocalOperator, activation(&module, &guild))
        .await
        .unwrap();
    manager
        .activate(&PolicyContext::LocalOperator, activation(&module, &other))
        .await
        .unwrap();
    assert_eq!(
        storage
            .migration_status(&module, &guild)
            .await
            .unwrap()
            .data_version,
        2
    );
    let bound = binding(&manager, &guild).await;
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "900",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err(),
        "mutations need an explicit host opt-in"
    );
    configure(&manager, &guild, &module).await;
    configure(&manager, &other, &module).await;
    let bound = binding(&manager, &guild).await;
    for input in [
        json!({"owner_id":"901"}),
        json!({"guild_id":"456"}),
        json!({"actor":{"user_id":"901"}}),
    ] {
        assert!(
            call(
                &manager,
                &guild,
                &bound,
                "900",
                &interaction(),
                "run_create_casual",
                input
            )
            .await
            .is_err()
        );
    }
    let create_id = interaction();
    let created = call(
        &manager,
        &guild,
        &bound,
        "900",
        &create_id,
        "run_create_casual",
        json!({"name":"Native run"}),
    )
    .await
    .unwrap();
    assert_eq!(
        created,
        call(
            &manager,
            &guild,
            &bound,
            "900",
            &create_id,
            "run_create_casual",
            json!({"name":"Native run"})
        )
        .await
        .unwrap()
    );
    let id = created["result"]["run_id"].as_str().unwrap();
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "901",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "901",
            &interaction(),
            "run_change",
            json!({"id":id,"command":{"action":"publish"}})
        )
        .await
        .is_err()
    );
    let foreign = binding(&manager, &other).await;
    assert!(
        call(
            &manager,
            &other,
            &foreign,
            "900",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .is_err()
    );
    assert!(
        manager
            .invoke_member_mutation_bound(
                &member(&guild, "900"),
                &interaction(),
                &other,
                &module,
                "run_view",
                json!({"id":id}),
                &bound.session,
                bound.generation,
                bound.epoch
            )
            .await
            .is_err()
    );
    change(
        &manager,
        &guild,
        &bound,
        "900",
        id,
        json!({"action":"publish"}),
    )
    .await;
    let join_id = interaction();
    let joined = call(
        &manager,
        &guild,
        &bound,
        "901",
        &join_id,
        "run_signup",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(
        joined,
        call(
            &manager,
            &guild,
            &bound,
            "901",
            &join_id,
            "run_signup",
            json!({"id":id})
        )
        .await
        .unwrap()
    );
    let saved = call(
        &manager,
        &guild,
        &bound,
        "900",
        &interaction(),
        "run_view",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(saved["run"]["owner_id"], "900");
    assert_eq!(saved["run"]["guild_id"], "123");
    assert_eq!(saved["run"]["assignments"].as_object().unwrap().len(), 2);
    assert!(saved["run"]["allocations"].is_null());
    assert_eq!(
        saved["publication"]["desired_revision"],
        saved["run"]["desired_card_revision"]
    );
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(MemberReadPolicy {
                channels: BTreeSet::new(),
                roles: BTreeSet::new(),
                per_user_per_minute: 20,
                per_guild_per_minute: 40,
            }),
        )
        .await
        .unwrap();
    assert!(
        manager
            .invoke_member_bound(
                &member(&guild, "900"),
                &guild,
                &module,
                "run_view",
                json!({"id":id}),
                &bound.session,
                bound.generation,
                bound.epoch
            )
            .await
            .is_err(),
        "MemberRead never gains run document callbacks"
    );
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "maintenance",
                json!({})
            )
            .await
            .is_err()
    );
    // Kill the actual module after acknowledged writes, then restart with stale
    // evidence. No graceful module shutdown participates in saving assignments.
    let health = manager.health().await;
    let pid = health[&module]["host"]["pid"]
        .as_u64()
        .expect("native subprocess PID");
    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    let mut stale = catalog;
    for source in stale["sources"].as_array_mut().unwrap() {
        source["validated_at_ms"] = json!(1);
    }
    snapshot(&data, &stale);
    manager.load(&installed.digest).await.unwrap();
    let rebound = binding(&manager, &guild).await;
    assert_ne!(bound.generation, rebound.generation);
    assert_eq!(
        created,
        call(
            &manager,
            &guild,
            &rebound,
            "900",
            &create_id,
            "run_create_casual",
            json!({"name":"Native run"})
        )
        .await
        .unwrap()
    );
    assert_eq!(
        saved,
        call(
            &manager,
            &guild,
            &rebound,
            "900",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .unwrap()
    );
    assert!(
        call(
            &manager,
            &guild,
            &rebound,
            "902",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err(),
        "stale catalog prevents new eligibility snapshots"
    );
    call(
        &manager,
        &guild,
        &rebound,
        "902",
        &interaction(),
        "run_signup",
        json!({"id":id}),
    )
    .await
    .unwrap();
    let leave_id = interaction();
    let left = call(
        &manager,
        &guild,
        &rebound,
        "901",
        &leave_id,
        "run_leave",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(
        left,
        call(
            &manager,
            &guild,
            &rebound,
            "901",
            &leave_id,
            "run_leave",
            json!({"id":id})
        )
        .await
        .unwrap()
    );
    // A later restart with no wiki snapshot must still recover durable runs.
    manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    fs::remove_file(data.join("active")).unwrap();
    manager.load(&installed.digest).await.unwrap();
    let rebound = binding(&manager, &guild).await;
    let recovered = call(
        &manager,
        &guild,
        &rebound,
        "900",
        &interaction(),
        "run_view",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(recovered["run"]["eligibility"], saved["run"]["eligibility"]);
    change(
        &manager,
        &guild,
        &rebound,
        "900",
        id,
        json!({"action":"lock"}),
    )
    .await;
    change(
        &manager,
        &guild,
        &rebound,
        "900",
        id,
        json!({"action":"reopen"}),
    )
    .await;
    assert!(
        call(
            &manager,
            &guild,
            &rebound,
            "903",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err()
    );
    // Age a never-published draft through the storage seam, then exercise the actual
    // authenticated host Maintenance delivery rather than calling cleanup directly.
    // Clone the existing pinned snapshot so no wiki refresh is needed for this fixture.
    let mut draft = saved.clone();
    let expired_id = "23456789";
    draft["publication"] = Value::Null;
    draft["run"]["id"] = json!(expired_id);
    draft["run"]["owner_id"] = json!("903");
    draft["run"]["state"] = json!("draft");
    draft["run"]["assignments"] = json!({});
    draft["run"]["created_at"] = json!(now() - 2 * DAY);
    draft["run"]["updated_at"] = json!(now() - 2 * DAY);
    draft["run"]["last_owner_edit_at"] = json!(now() - 2 * DAY);
    draft["run"]["desired_card_revision"] = json!(1);
    let live = storage
        .document_get(&module, &guild, "run_index", "live")
        .await
        .unwrap()
        .unwrap();
    let mut live_value = live.value;
    live_value[expired_id] =
        json!({"owner":"903","state":"draft","last_owner_edit_at":now()-2*DAY});
    storage
        .document_batch(
            &module,
            &guild,
            2,
            &[
                DocumentWrite {
                    collection: "runs".into(),
                    key: expired_id.into(),
                    expected_revision: None,
                    value: Some(draft),
                },
                DocumentWrite {
                    collection: "run_index".into(),
                    key: "live".into(),
                    expected_revision: Some(live.revision),
                    value: Some(live_value),
                },
            ],
        )
        .await
        .unwrap();
    // Reapply configuration after a new process, preserving explicit event admission.
    configure(&manager, &guild, &module).await;
    let held = manager
        .invoke_member_mutation_bound(
            &member(&guild, "900"),
            &interaction(),
            &guild,
            &module,
            "run_view",
            json!({"id":id}),
            &rebound.session,
            rebound.generation,
            rebound.epoch,
        )
        .await
        .unwrap();
    let plan = manager
        .configuration_plan(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            None,
            json!({"limits":{"drafts_per_guild":0}}),
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
    assert!(
        held.policy.check().is_err(),
        "configuration changes fence earlier mutation authorization"
    );
    assert_eq!(
        manager
            .deliver_event(
                &guild,
                GuildEvent {
                    id: "run-cleanup".into(),
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
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if storage
                .document_get(&module, &guild, "runs", expired_id)
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Maintenance must clean old draft despite stale wiki");
    let final_run = storage
        .document_get(&module, &guild, "runs", id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_run.value["run"]["assignments"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(final_run.value["run"]["assignments"]["900"].is_object());
    assert!(final_run.value["run"]["assignments"]["902"].is_object());
    assert_eq!(
        final_run.value["run"]["eligibility"],
        saved["run"]["eligibility"]
    );
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
}
