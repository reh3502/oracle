//! Native database backup plus explicit verified artifact reinstall, using a real child.
use oracle_core::*;
use oracle_modules::ModuleManager;
use oracle_storage::{DatabaseConfig, PgTools, Storage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Cleanup {
    managers: Mutex<Vec<Arc<ModuleManager>>>,
    stores: Mutex<Vec<Arc<Storage>>>,
}
impl Cleanup {
    fn store(&self, store: Storage) -> Arc<Storage> {
        let store = Arc::new(store);
        self.stores.lock().unwrap().push(store.clone());
        store
    }
    fn manager(
        &self,
        store: &Arc<Storage>,
        core: Arc<CoreService>,
        path: PathBuf,
    ) -> Arc<ModuleManager> {
        let manager = ModuleManager::new(store.clone(), core, path).unwrap();
        self.managers.lock().unwrap().push(manager.clone());
        manager
    }
    async fn finish(&self) {
        let managers = std::mem::take(&mut *self.managers.lock().unwrap());
        for manager in managers {
            let _ = manager.shutdown().await;
        }
        let stores = std::mem::take(&mut *self.stores.lock().unwrap());
        for store in stores {
            let _ = store.close().await;
        }
    }
}
fn remove_tree(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Ok(children) = std::fs::read_dir(path) {
            for child in children.flatten() {
                remove_tree(&child.path());
            }
        }
        let _ = std::fs::remove_dir(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}
fn core(store: &Arc<Storage>, guild: &GuildId) -> Arc<CoreService> {
    Arc::new(CoreService::new(
        store.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ))
}
#[tokio::test]
#[ignore = "requires ORACLE_COUNTER_V1 pointing to the current default counter binary"]
async fn restored_database_requires_artifact_reinstall_and_explicit_resume() {
    let binary =
        PathBuf::from(std::env::var_os("ORACLE_COUNTER_V1").expect("set ORACLE_COUNTER_V1"));
    let scratch =
        std::env::temp_dir().join(format!("oracle-module-restore-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let cleanup = Arc::new(Cleanup::default());
    let task_cleanup = cleanup.clone();
    let task_scratch = scratch.clone();
    // Catch assertion panics at the task boundary, then still reap every owned process.
    let outcome =
        tokio::spawn(async move { drill(task_cleanup, task_scratch, binary).await }).await;
    cleanup.finish().await;
    remove_tree(&scratch);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error.into_panic());
    }
}
async fn drill(cleanup: Arc<Cleanup>, scratch: PathBuf, binary: PathBuf) {
    let guild: GuildId = "123".parse().unwrap();
    let storage = cleanup.store(
        Storage::open(DatabaseConfig::Sqlite {
            path: scratch.join("original.sqlite"),
        })
        .await
        .unwrap(),
    );
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let original_core = core(&storage, &guild);
    let original = cleanup.manager(
        &storage,
        original_core.clone(),
        scratch.join("original-artifacts"),
    );
    let source = scratch.join("source");
    std::fs::create_dir(&source).unwrap();
    let bytes = std::fs::read(binary).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/counter/manifest.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "restore-drill".into(),
        toolchain: "staged native v1".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = original.install(&source, true).await.unwrap();
    let module = installed.package.manifest.id.clone();
    original.load(&installed.digest).await.unwrap();
    original
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants: vec!["storage.own".into()],
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let before = original
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "increment",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(before["value"], 1);
    let old_session = original.health().await[&module]["host"]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let old_pid = original.health().await[&module]["host"]["pid"]
        .as_u64()
        .unwrap();
    let old_deployment = original_core
        .status(&PolicyContext::LocalOperator, None)
        .await
        .unwrap()
        .deployment;
    let bundle = scratch.join("backup");
    storage.backup(&bundle, &PgTools::default()).await.unwrap();
    original.shutdown().await.unwrap();
    storage.close().await.unwrap();
    assert!(!Path::new(&format!("/proc/{old_pid}")).exists());
    let restored = cleanup.store(
        Storage::restore(
            DatabaseConfig::Sqlite {
                path: scratch.join("restored.sqlite"),
            },
            &bundle,
            &PgTools::default(),
        )
        .await
        .unwrap(),
    );
    let restored_core = core(&restored, &guild);
    let artifact_root = scratch.join("restored-artifacts");
    let manager = cleanup.manager(&restored, restored_core.clone(), artifact_root.clone());
    assert_eq!(std::fs::read_dir(&artifact_root).unwrap().count(), 0);
    let status = restored_core
        .status(&PolicyContext::LocalOperator, Some(&guild))
        .await
        .unwrap();
    assert_ne!(status.deployment, old_deployment);
    assert!(status.guilds[0].paused);
    assert!(!restored.desired_modules().await.unwrap()[0].loaded);
    assert!(restored.desired_activations().await.unwrap()[0].active);
    assert!(manager.restore_desired().await.unwrap().is_empty());
    assert!(manager.health().await.is_empty());
    assert!(manager.load(&installed.digest).await.is_err());
    assert!(manager.health().await.is_empty());
    assert_eq!(
        manager.install(&source, false).await.unwrap_err().code,
        ErrorCode::TrustedCodeRequired
    );
    let reinstalled = manager.install(&source, true).await.unwrap();
    assert_eq!(reinstalled.digest, installed.digest);
    assert!(manager.health().await.is_empty());
    restored_core
        .control(
            &PolicyContext::LocalOperator,
            &guild,
            false,
            status.guilds[0].revision,
        )
        .await
        .unwrap();
    manager.load(&installed.digest).await.unwrap();
    let after = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "get",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(after["value"], 1);
    // Epoch counters are scoped to the fresh process session, not globally unique.
    let health = manager.health().await;
    let new_session = health[&module]["host"]["session"].as_str().unwrap();
    assert_ne!(new_session, old_session);
    assert_ne!(
        (new_session, &after["epoch"]),
        (old_session.as_str(), &before["epoch"])
    );
    let new_pid = manager.health().await[&module]["host"]["pid"]
        .as_u64()
        .unwrap();
    assert_ne!(new_pid, old_pid);
    manager.shutdown().await.unwrap();
    assert!(!Path::new(&format!("/proc/{new_pid}")).exists());
    restored.close().await.unwrap();
}
