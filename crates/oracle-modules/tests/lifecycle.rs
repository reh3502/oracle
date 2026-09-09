//! Build fixture executables first; this drill uses real processes, RPC and SQLite.
use oracle_core::*;
use oracle_modules::ModuleManager;
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

#[tokio::test]
#[ignore = "requires cargo build -p oracle-example-counter -p oracle-example-dependent"]
async fn dynamic_install_two_guilds_dependency_call_and_unload() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch =
        std::env::temp_dir().join(format!("oracle-module-lifecycle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let storage = Arc::new(
        Storage::open(match std::env::var("ORACLE_TEST_MODULE_POSTGRES_URL") {
            Ok(url) => DatabaseConfig::Postgres { url },
            Err(_) => DatabaseConfig::Sqlite {
                path: scratch.join("state.sqlite"),
            },
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let other: GuildId = "456".parse().unwrap();
    storage
        .initialize_guilds(&[guild.clone(), other.clone()])
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
    let manager =
        ModuleManager::new(storage.clone(), core.clone(), scratch.join("artifacts")).unwrap();
    assert!(manager.health().await.is_empty());
    // The manager is already running before the package directory even exists.
    let mut packages = BTreeMap::new();
    for name in ["counter", "dependent"] {
        let directory = scratch.join(name);
        std::fs::create_dir(&directory).unwrap();
        let bytes = std::fs::read(root.join(format!("target/debug/oracle-example-{name}")))
            .expect("build fixture executable first");
        std::fs::write(directory.join("module"), &bytes).unwrap();
        let manifest: ModuleManifest = serde_json::from_slice(
            &std::fs::read(root.join(format!("examples/modules/{name}/manifest.json"))).unwrap(),
        )
        .unwrap();
        let package = ModulePackage {
            manifest,
            entrypoint: "module".into(),
            files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
            source_revision: "integration-test".into(),
            toolchain: "cargo fixture build".into(),
            license: "test-only".into(),
        };
        std::fs::write(
            directory.join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manager.install(&directory, false).await.unwrap_err().code,
            ErrorCode::TrustedCodeRequired
        );
        let installed = manager.install(&directory, true).await.unwrap();
        assert!(manager.health().await.is_empty(), "install executed code");
        packages.insert(name, installed);
    }
    for name in ["counter", "dependent"] {
        manager.load(&packages[name].digest).await.unwrap();
    }
    let counter: ModuleId = "fixture.counter".parse().unwrap();
    let dependent: ModuleId = "fixture.dependent".parse().unwrap();
    let activation = |module: ModuleId, guild: GuildId, grants: Vec<String>| DesiredActivation {
        module,
        guild,
        active: true,
        grants,
        bindings: BTreeMap::new(),
    };
    assert_eq!(
        manager
            .activate(
                &PolicyContext::LocalOperator,
                activation(
                    dependent.clone(),
                    guild.clone(),
                    vec!["contracts.invoke".into(), "storage.own".into()]
                )
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::DependencyUnavailable
    );
    for scope in [&guild, &other] {
        manager
            .activate(
                &PolicyContext::LocalOperator,
                activation(counter.clone(), scope.clone(), vec!["storage.own".into()]),
            )
            .await
            .unwrap();
    }
    let first = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &guild,
            "increment",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(first["value"], 1);
    let second = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &other,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(second["value"], 0);
    assert_eq!(first["generation"], second["generation"]);
    assert_ne!(first["epoch"], second["epoch"]);
    manager
        .activate(
            &PolicyContext::LocalOperator,
            activation(
                dependent.clone(),
                guild.clone(),
                vec!["contracts.invoke".into(), "storage.own".into()],
            ),
        )
        .await
        .unwrap();
    let operation = &packages["dependent"].package.manifest.operations[0].name;
    let delegated = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &dependent,
            &guild,
            operation,
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(delegated["value"], 1);
    assert_eq!(
        manager
            .unload(&counter, Duration::from_secs(1))
            .await
            .unwrap_err()
            .code,
        ErrorCode::DependencyUnavailable
    );
    manager
        .unload(&dependent, Duration::from_secs(1))
        .await
        .unwrap();
    manager
        .deactivate(
            &PolicyContext::LocalOperator,
            &counter,
            &guild,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &counter,
                &guild,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    let surviving = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &other,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(surviving["generation"], first["generation"]);
    manager
        .unload(&counter, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(manager.health().await.is_empty());
    manager.load(&packages["counter"].digest).await.unwrap();
    let reloaded = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &other,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(reloaded["value"], 0);
    assert_ne!(reloaded["generation"], surviving["generation"]);
    assert_ne!(reloaded["epoch"], surviving["epoch"]);
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &counter,
                &guild,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    manager
        .activate(
            &PolicyContext::LocalOperator,
            activation(counter.clone(), guild.clone(), vec!["storage.own".into()]),
        )
        .await
        .unwrap();
    let task_manager = manager.clone();
    let task_counter = counter.clone();
    let task_guild = other.clone();
    let waiting = tokio::spawn(async move {
        task_manager
            .invoke(
                &PolicyContext::LocalOperator,
                &task_counter,
                &task_guild,
                "wait",
                json!({}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if manager.health().await[&counter]["host"]["in_flight"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let impacted = manager
        .deactivate(
            &PolicyContext::LocalOperator,
            &counter,
            &other,
            Duration::from_millis(20),
        )
        .await
        .unwrap();
    assert!(impacted.contains(&guild) && impacted.contains(&other));
    assert!(waiting.await.unwrap().is_err());
    let recovered = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &guild,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(recovered["value"], 1);
    assert_ne!(recovered["generation"], reloaded["generation"]);
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &counter,
                &other,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    manager.load(&packages["dependent"].digest).await.unwrap();
    let dependent_pid = manager.health().await[&dependent]["host"]["pid"]
        .as_u64()
        .unwrap();
    let provider_pid = manager.health().await[&counter]["host"]["pid"]
        .as_u64()
        .unwrap();
    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", &provider_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            manager.tick().await.unwrap();
            if manager.health().await[&counter]["restart_attempts"].is_u64() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        manager.health().await[&dependent]["host"].is_null(),
        "required consumer stayed active after provider crash"
    );
    assert!(!std::path::Path::new(&format!("/proc/{dependent_pid}")).exists());
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &dependent,
                &guild,
                operation,
                json!({})
            )
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            manager.tick().await.unwrap();
            let health = manager.health().await;
            if health[&counter]["host"]["pid"].is_u64()
                && health[&dependent]["host"]["pid"].is_u64()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &dependent,
                &guild,
                operation,
                json!({})
            )
            .await
            .unwrap()["value"],
        1
    );
    assert_ne!(
        manager.health().await[&dependent]["host"]["pid"],
        dependent_pid
    );
    manager
        .unload(&dependent, Duration::from_secs(1))
        .await
        .unwrap();
    manager
        .unload(&counter, Duration::from_secs(1))
        .await
        .unwrap();
    manager.load(&packages["counter"].digest).await.unwrap();
    // Cancel an unload during a real drain, then reload before its monitor finishes.
    let old_pid = manager.health().await[&counter]["host"]["pid"]
        .as_u64()
        .unwrap();
    let before_cancel = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &guild,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    let waiting = {
        let manager = manager.clone();
        let module = counter.clone();
        let guild = guild.clone();
        tokio::spawn(async move {
            manager
                .invoke(
                    &PolicyContext::LocalOperator,
                    &module,
                    &guild,
                    "wait",
                    json!({}),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.health().await[&counter]["host"]["in_flight"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let unloading = {
        let manager = manager.clone();
        let module = counter.clone();
        tokio::spawn(async move { manager.unload(&module, Duration::from_secs(20)).await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let stopped_intent = storage
                .desired_modules()
                .await
                .unwrap()
                .iter()
                .any(|d| d.module == counter && !d.loaded);
            let health = manager.health().await;
            let hidden = health.get(&counter).is_none_or(|v| v["host"].is_null());
            if stopped_intent && hidden {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    unloading.abort();
    assert!(unloading.await.unwrap_err().is_cancelled());
    assert!(waiting.await.unwrap().is_err());
    manager.load(&packages["counter"].digest).await.unwrap();
    assert!(!std::path::Path::new(&format!("/proc/{old_pid}")).exists());
    let after_cancel = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &counter,
            &guild,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(after_cancel["value"], 1);
    assert_ne!(after_cancel["generation"], before_cancel["generation"]);
    // Actual child crashes get bounded recovery, rather than reusing the dead generation.
    for crash in 0..4 {
        let pid = manager.health().await[&counter]["host"]["pid"]
            .as_u64()
            .unwrap();
        assert!(
            std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                manager.tick().await.unwrap();
                let health = manager.health().await;
                if crash == 3 {
                    if health[&counter]["quarantined"] == true {
                        break;
                    }
                } else if health[&counter]["host"]["pid"]
                    .as_u64()
                    .is_some_and(|new| new != pid)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        if crash < 3 {
            assert_eq!(
                manager
                    .invoke(
                        &PolicyContext::LocalOperator,
                        &counter,
                        &guild,
                        "get",
                        json!({})
                    )
                    .await
                    .unwrap()["value"],
                1
            );
        }
    }
    assert_eq!(
        core.status(&PolicyContext::LocalOperator, None)
            .await
            .unwrap()
            .modules_loaded,
        0
    );
    manager.tick().await.unwrap();
    assert_eq!(manager.health().await[&counter]["restart_attempts"], 3);
    assert!(
        manager
            .unload(&counter, Duration::from_secs(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !storage
            .desired_modules()
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.module == counter)
            .unwrap()
            .loaded
    );
    manager.tick().await.unwrap();
    assert!(manager.health().await.is_empty());
    manager.load(&packages["counter"].digest).await.unwrap();
    assert_eq!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &counter,
                &guild,
                "get",
                json!({})
            )
            .await
            .unwrap()["value"],
        1
    );
    manager
        .unload(&counter, Duration::from_secs(1))
        .await
        .unwrap();
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
    // Installed files are deliberately read-only; make private test tree writable for cleanup.
    fn writable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        if path.is_dir() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            for child in std::fs::read_dir(path).unwrap() {
                writable(&child.unwrap().path());
            }
        }
    }
    writable(&scratch);
    std::fs::remove_dir_all(scratch).unwrap();
}
