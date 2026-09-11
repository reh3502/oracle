//! Real subprocess configuration qualification. SQLite by default; the same
//! tests accept an isolated ORACLE_TEST_CONFIGURATION_POSTGRES_URL database.
use oracle_core::*;
use oracle_modules::{ConfigurationPolicy, ModuleManager};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
struct Policy(AtomicBool);
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        if self.0.load(Ordering::SeqCst) && values["destination"] == "staff" {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
}
const ACTOR: PolicyContext = PolicyContext::LocalOperator;
const TTL: Duration = Duration::from_secs(60);
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_exact_readback_preserves_settings_and_cas() {
    run("success").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_policy_schema_expiry_and_generation_reject_stale_plans() {
    run("denied").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_prepare_rejection_and_readback_mismatch_are_not_success() {
    run("partial").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_real_crash_and_host_restart_recover_same_revision() {
    run("crash").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_cancelled_pending_does_not_replace_active_values() {
    run("cancel").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_CONFIGURATION_PROBE current fixture binary"]
async fn configuration_policy_changed_during_prepare_cannot_commit() {
    run("policy_wait").await;
}
async fn run(case: &'static str) {
    let scratch = std::env::temp_dir().join(format!("oracle-config-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let db = match std::env::var("ORACLE_TEST_CONFIGURATION_POSTGRES_URL") {
        Ok(url) => DatabaseConfig::Postgres { url },
        Err(_) => DatabaseConfig::Sqlite {
            path: scratch.join("state.sqlite"),
        },
    };
    let storage = Arc::new(Storage::open(db.clone()).await.unwrap());
    let guild: GuildId = ((uuid::Uuid::new_v4().as_u128() as u64 >> 1) + 1)
        .to_string()
        .parse()
        .unwrap();
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec!["42".parse().unwrap()],
        }],
    ));
    let manager =
        ModuleManager::new(storage.clone(), core.clone(), scratch.join("artifacts")).unwrap();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    manager
        .set_configuration_services(storage.clone(), policy.clone())
        .unwrap();
    let binary = PathBuf::from(
        std::env::var_os("ORACLE_CONFIGURATION_PROBE").expect("set ORACLE_CONFIGURATION_PROBE"),
    );
    let source = scratch.join("source");
    std::fs::create_dir(&source).unwrap();
    let bytes = std::fs::read(binary).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "configuration-fixture".into(),
        toolchain: "current".into(),
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
    manager
        .activate(
            &ACTOR,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants: vec!["config.own".into()],
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    // Configuration-only artifacts must be discoverable without a slash command.
    assert!(manager.catalog(&ACTOR, &guild).await.unwrap().is_empty());
    let ai = manager.ai_catalog_snapshot(&ACTOR, &guild).await.unwrap();
    assert_eq!(ai.entries.len(), 1);
    assert_eq!(ai.entries[0].module, module);
    assert_eq!(ai.entries[0].artifact_digest, installed.digest);
    assert!(
        ai.entries[0]
            .configuration
            .as_ref()
            .unwrap()
            .presets
            .contains_key("moderate/v1")
    );
    let foreign = PolicyContext::Discord {
        guild: "999".parse().unwrap(),
        user: "42".parse().unwrap(),
        manage_guild: true,
    };
    assert_eq!(
        manager
            .ai_catalog_snapshot(&foreign, &guild)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    let task_manager = manager.clone();
    let task_storage = storage.clone();
    let task_scratch = scratch.clone();
    let task = tokio::spawn(async move {
        let manager = task_manager;
        let storage = task_storage;
        let scratch = task_scratch;
        let first = manager
            .configuration_plan(
                &ACTOR,
                &guild,
                &module,
                Some("moderate/v1"),
                json!({"destination":"staff","note":"keep"}),
                TTL,
            )
            .await
            .unwrap();
        let receipt = manager
            .configuration_apply(&ACTOR, &guild, &module, &first.id)
            .await
            .unwrap();
        assert_eq!(receipt.state, "effective");
        assert_eq!(receipt.stored_revision, 1);
        assert_eq!(receipt.effective_revision, Some(1));
        match case {
            "success" => {
                let retry = manager
                    .configuration_apply(&ACTOR, &guild, &module, &first.id)
                    .await
                    .unwrap();
                assert_eq!(retry.stored_revision, 1);
                let unchanged = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({}), TTL)
                    .await
                    .unwrap();
                let repeated = manager
                    .configuration_apply(&ACTOR, &guild, &module, &unchanged.id)
                    .await
                    .unwrap();
                assert_eq!(repeated.plan, unchanged.id);
                assert_eq!(repeated.state, "effective");
                assert_eq!(
                    repeated.stored_revision, 1,
                    "unchanged desired values must not create another revision"
                );
                assert_eq!(repeated.effective_revision, Some(1));
                assert_eq!(
                    manager
                        .configuration_inspect(&ACTOR, &guild, &module)
                        .await
                        .unwrap()
                        .receipt
                        .unwrap()
                        .plan,
                    unchanged.id
                );
                let third = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({}), TTL)
                    .await
                    .unwrap();
                let third_receipt = manager
                    .configuration_apply(&ACTOR, &guild, &module, &third.id)
                    .await
                    .unwrap();
                assert_eq!(third_receipt.plan, third.id);
                assert_eq!(third_receipt.state, "effective");
                assert_eq!(third_receipt.stored_revision, 1);
                let recovered = manager
                    .configuration_recover(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                assert_eq!(recovered.plan, third.id);
                assert_eq!(recovered.state, "effective");
                assert_eq!(recovered.stored_revision, 1);
                assert_eq!(recovered.effective_revision, Some(1));
                let stale = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({"level":3}), TTL)
                    .await
                    .unwrap();
                let newer = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"note":"human change","level":5}),
                        TTL,
                    )
                    .await
                    .unwrap();
                manager
                    .configuration_apply(&ACTOR, &guild, &module, &newer.id)
                    .await
                    .unwrap();
                assert_eq!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &stale.id)
                        .await
                        .unwrap_err()
                        .code,
                    ErrorCode::Conflict
                );
                let preset = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        Some("moderate/v1"),
                        json!({}),
                        TTL,
                    )
                    .await
                    .unwrap();
                assert_eq!(preset.values["note"], "human change");
                assert_eq!(preset.values["level"], 2);
                manager
                    .configuration_apply(&ACTOR, &guild, &module, &preset.id)
                    .await
                    .unwrap();
                let status = manager
                    .configuration_inspect(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                assert_eq!(status.stored_revision, 3);
                assert_eq!(status.effective.unwrap().values, preset.values);
            }
            "denied" => {
                assert!(
                    manager
                        .configuration_plan(
                            &ACTOR,
                            &guild,
                            &module,
                            None,
                            json!({"note":"x".repeat(25*1024)}),
                            TTL
                        )
                        .await
                        .is_err(),
                    "a config plan must fit desired and candidate in the bounded workflow record"
                );
                assert!(
                    manager
                        .configuration_plan(
                            &ACTOR,
                            &guild,
                            &module,
                            Some("unknown/v1"),
                            json!({}),
                            TTL
                        )
                        .await
                        .is_err()
                );
                assert!(
                    manager
                        .configuration_plan(&ACTOR, &guild, &module, None, json!({"level":-1}), TTL)
                        .await
                        .is_err()
                );
                let plan = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({"level":4}), TTL)
                    .await
                    .unwrap();
                let unchanged = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({}), TTL)
                    .await
                    .unwrap();
                policy.0.store(false, Ordering::SeqCst);
                assert!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &unchanged.id)
                        .await
                        .is_err(),
                    "unchanged configuration still needs current authorization"
                );
                assert!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &plan.id)
                        .await
                        .is_err()
                );
                assert_eq!(
                    manager
                        .configuration_inspect(&ACTOR, &guild, &module)
                        .await
                        .unwrap()
                        .stored_revision,
                    1
                );
                policy.0.store(true, Ordering::SeqCst);
                let foreign = PolicyContext::Discord {
                    guild: guild.clone(),
                    user: "42".parse().unwrap(),
                    manage_guild: true,
                };
                assert!(
                    manager
                        .configuration_apply(&foreign, &guild, &module, &plan.id)
                        .await
                        .is_err()
                );
                let expired = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({}),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(1100)).await;
                assert!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &expired.id)
                        .await
                        .is_err()
                );
                let slow = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"wait_ms":1500,"level":99}),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                assert!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &slow.id)
                        .await
                        .is_err(),
                    "prepare must not carry expired approval across the commit boundary"
                );
                assert_eq!(
                    manager
                        .configuration_inspect(&ACTOR, &guild, &module)
                        .await
                        .unwrap()
                        .stored_revision,
                    1
                );
                manager
                    .unload(&module, Duration::from_secs(1))
                    .await
                    .unwrap();
                assert!(
                    manager
                        .ai_catalog_snapshot(&ACTOR, &guild)
                        .await
                        .unwrap()
                        .entries
                        .is_empty()
                );
                manager.load(&installed.digest).await.unwrap();
                let reloaded = manager.ai_catalog_snapshot(&ACTOR, &guild).await.unwrap();
                assert!(reloaded.revision > ai.revision);
                assert_eq!(reloaded.entries.len(), 1);
                assert_ne!(reloaded.entries[0].generation, ai.entries[0].generation);
                assert!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &plan.id)
                        .await
                        .is_err()
                );
            }
            "partial" => {
                let reject = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({"reject":true}), TTL)
                    .await
                    .unwrap();
                let receipt = manager
                    .configuration_apply(&ACTOR, &guild, &module, &reject.id)
                    .await
                    .unwrap();
                assert_eq!(receipt.state, "rejected");
                assert_eq!(receipt.stored_revision, 1);
                let status = manager
                    .configuration_inspect(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                assert_eq!(status.values.unwrap()["level"], 2);
                assert_eq!(status.effective.unwrap().revision, 1);
                let mismatch = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"mismatch":true}),
                        TTL,
                    )
                    .await
                    .unwrap();
                let receipt = manager
                    .configuration_apply(&ACTOR, &guild, &module, &mismatch.id)
                    .await
                    .unwrap();
                assert_eq!(receipt.state, "unknown");
                assert_eq!(
                    receipt.problem.as_deref(),
                    Some("effective_config_mismatch")
                );
                assert_eq!(receipt.effective_revision, None);
                let unchanged = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({}), TTL)
                    .await
                    .unwrap();
                let retried = manager
                    .configuration_apply(&ACTOR, &guild, &module, &unchanged.id)
                    .await
                    .unwrap();
                assert_eq!(retried.plan, unchanged.id);
                assert_eq!(retried.stored_revision, receipt.stored_revision);
                assert_eq!(
                    retried.state, "unknown",
                    "unchanged desired values cannot certify a mismatched active configuration"
                );
                assert_eq!(retried.effective_revision, None);
            }
            "crash" => {
                let plan = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"level":7,"crash":true,"crash_marker":scratch.join("crashed-once")}),
                        TTL,
                    )
                    .await
                    .unwrap();
                let receipt = manager
                    .configuration_apply(&ACTOR, &guild, &module, &plan.id)
                    .await
                    .unwrap();
                assert_eq!(receipt.state, "unknown");
                assert_eq!(receipt.stored_revision, 2);
                manager.shutdown().await.unwrap();
                storage.close().await.unwrap();
                let reopened = Arc::new(Storage::open(db).await.unwrap());
                let core = Arc::new(CoreService::new(
                    reopened.clone(),
                    vec![GuildPolicy {
                        guild: guild.clone(),
                        operators: vec![],
                    }],
                ));
                let fresh =
                    ModuleManager::new(reopened.clone(), core, scratch.join("artifacts")).unwrap();
                fresh
                    .set_configuration_services(reopened.clone(), policy)
                    .unwrap();
                fresh.load(&installed.digest).await.unwrap();
                let recovered = fresh
                    .configuration_recover(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                let status = fresh
                    .configuration_inspect(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                fresh.shutdown().await.unwrap();
                reopened.close().await.unwrap();
                assert_eq!(recovered.state, "effective");
                assert_eq!(recovered.stored_revision, 2);
                assert_eq!(recovered.effective_revision, Some(2));
                assert_eq!(status.values.unwrap()["level"], 7);
            }
            "cancel" | "policy_wait" => {
                let plan = manager
                    .configuration_plan(
                        &ACTOR,
                        &guild,
                        &module,
                        None,
                        json!({"wait_ms":3000,"level":9}),
                        TTL,
                    )
                    .await
                    .unwrap();
                let m = manager.clone();
                let g = guild.clone();
                let id = module.clone();
                let applying =
                    tokio::spawn(
                        async move { m.configuration_apply(&ACTOR, &g, &id, &plan.id).await },
                    );
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let record = storage
                            .workflow_get(&guild, WorkflowKind::Configuration, module.as_str())
                            .await
                            .unwrap()
                            .unwrap();
                        if record.value["receipt"]["state"] == "pending" {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                if case == "cancel" {
                    applying.abort();
                    let _ = applying.await;
                } else {
                    policy.0.store(false, Ordering::SeqCst);
                    assert!(applying.await.unwrap().is_err());
                    policy.0.store(true, Ordering::SeqCst);
                }
                let status = manager
                    .configuration_inspect(&ACTOR, &guild, &module)
                    .await
                    .unwrap();
                assert_eq!(status.stored_revision, 1);
                assert_eq!(status.values.unwrap()["level"], 2);
                assert!(
                    manager
                        .configuration_recover(&ACTOR, &guild, &module)
                        .await
                        .is_err()
                );
                let retry = manager
                    .configuration_plan(&ACTOR, &guild, &module, None, json!({"level":4}), TTL)
                    .await
                    .unwrap();
                assert_eq!(
                    manager
                        .configuration_apply(&ACTOR, &guild, &module, &retry.id)
                        .await
                        .unwrap()
                        .state,
                    "effective"
                );
            }
            _ => unreachable!(),
        }
    })
    .await;
    let _ = manager.shutdown().await;
    let _ = storage.close().await;
    remove_tree(&scratch);
    if let Err(error) = task {
        std::panic::resume_unwind(error.into_panic());
    }
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
