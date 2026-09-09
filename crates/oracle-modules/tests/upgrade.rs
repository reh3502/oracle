//! Real v1/v2 binaries are built independently and supplied as immutable staged paths.
use oracle_core::*;
use oracle_modules::ModuleManager;
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
// Inject one persistence failure while keeping real SQLite and real module transforms.
struct CheckpointFailure {
    storage: Arc<Storage>,
    fail: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl ModuleRepository for CheckpointFailure {
    async fn installations(&self) -> Result<Vec<InstalledModule>> {
        self.storage.installations().await
    }
    async fn install_module(&self, installed: &InstalledModule) -> Result<()> {
        self.storage.install_module(installed).await
    }
    async fn remove_installation(&self, digest: &str) -> Result<()> {
        self.storage.remove_installation(digest).await
    }
    async fn desired_modules(&self) -> Result<Vec<DesiredModule>> {
        self.storage.desired_modules().await
    }
    async fn set_module_desired(&self, desired: &DesiredModule) -> Result<()> {
        self.storage.set_module_desired(desired).await
    }
    async fn desired_activations(&self) -> Result<Vec<DesiredActivation>> {
        self.storage.desired_activations().await
    }
    async fn set_activation_desired(&self, activation: &DesiredActivation) -> Result<()> {
        self.storage.set_activation_desired(activation).await
    }
    async fn document_get(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        collection: &str,
        key: &str,
    ) -> Result<Option<ModuleDocument>> {
        self.storage
            .document_get(module, guild, collection, key)
            .await
    }
    async fn document_batch(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        data_version: u32,
        writes: &[DocumentWrite],
    ) -> Result<Vec<ModuleDocument>> {
        self.storage
            .document_batch(module, guild, data_version, writes)
            .await
    }
    async fn migration_status(
        &self,
        module: &ModuleId,
        guild: &GuildId,
    ) -> Result<MigrationProgress> {
        self.storage.migration_status(module, guild).await
    }
    async fn begin_migration(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        from: u32,
        to: u32,
        digest: &str,
    ) -> Result<MigrationProgress> {
        self.storage
            .begin_migration(module, guild, from, to, digest)
            .await
    }
    async fn migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        limit: u32,
    ) -> Result<MigrationPage> {
        self.storage.migration_page(module, guild, limit).await
    }
    async fn commit_migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        digest: &str,
        expected_cursor: Option<&str>,
        writes: &[DocumentWrite],
        next_cursor: Option<&str>,
        complete: bool,
    ) -> Result<MigrationProgress> {
        if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::new(ErrorCode::StorageUnavailable));
        }
        self.storage
            .commit_migration_page(
                module,
                guild,
                digest,
                expected_cursor,
                writes,
                next_cursor,
                complete,
            )
            .await
    }
}
fn package(scratch: &Path, name: &str, binary: &Path, manifest: &str) -> PathBuf {
    let directory = scratch.join(name);
    std::fs::create_dir(&directory).unwrap();
    let bytes = std::fs::read(binary).unwrap();
    std::fs::write(directory.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(manifest).unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "upgrade-drill".into(),
        toolchain: "separate staged v1/v2 native binaries".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        directory.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    directory
}
#[tokio::test]
#[ignore = "requires ORACLE_COUNTER_V1 and ORACLE_COUNTER_V2 pointing to separately built counter binaries"]
async fn forward_upgrade_preserves_active_and_inactive_guild_data_and_rejects_downgrade() {
    let v1 = PathBuf::from(std::env::var_os("ORACLE_COUNTER_V1").expect("set ORACLE_COUNTER_V1"));
    let v2 = PathBuf::from(std::env::var_os("ORACLE_COUNTER_V2").expect("set ORACLE_COUNTER_V2"));
    assert_ne!(
        std::fs::canonicalize(&v1).unwrap(),
        std::fs::canonicalize(&v2).unwrap()
    );
    let scratch = std::env::temp_dir().join(format!("oracle-upgrade-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let storage = Arc::new(
        Storage::open(match std::env::var("ORACLE_TEST_UPGRADE_POSTGRES_URL") {
            Ok(url) => DatabaseConfig::Postgres { url },
            Err(_) => DatabaseConfig::Sqlite {
                path: scratch.join("state.sqlite"),
            },
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let inactive: GuildId = "456".parse().unwrap();
    storage
        .initialize_guilds(&[guild.clone(), inactive.clone()])
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        [guild.clone(), inactive.clone()]
            .into_iter()
            .map(|guild| GuildPolicy {
                guild,
                operators: vec![],
            })
            .collect(),
    ));
    let checkpoint = Arc::new(CheckpointFailure {
        storage: storage.clone(),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let manager = ModuleManager::new(checkpoint.clone(), core, scratch.join("artifacts")).unwrap();
    // Neither packages nor executables exist in the host's artifact store at construction.
    assert!(manager.health().await.is_empty());
    let p1 = package(
        &scratch,
        "v1",
        &v1,
        include_str!("../../../examples/modules/counter/manifest.json"),
    );
    let p2 = package(
        &scratch,
        "v2",
        &v2,
        include_str!("../../../examples/modules/counter/manifest-v2.json"),
    );
    let i1 = manager.install(&p1, true).await.unwrap();
    let i2 = manager.install(&p2, true).await.unwrap();
    let module = i1.package.manifest.id.clone();
    manager.load(&i1.digest).await.unwrap();
    for scope in [&guild, &inactive] {
        manager
            .activate(
                &PolicyContext::LocalOperator,
                DesiredActivation {
                    module: module.clone(),
                    guild: scope.clone(),
                    active: true,
                    grants: vec!["storage.own".into()],
                    bindings: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
    }
    let before = manager
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
    let dormant = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &inactive,
            "increment",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(dormant["value"], 1);
    manager
        .deactivate(
            &PolicyContext::LocalOperator,
            &module,
            &inactive,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    // Cancel an upgrade while a real old-generation invocation is still draining.
    // Retry must retain that old process's cleanup handle and cannot race a live writer.
    let waiting_manager = manager.clone();
    let waiting_module = module.clone();
    let waiting_guild = guild.clone();
    let waiting = tokio::spawn(async move {
        waiting_manager
            .invoke(
                &PolicyContext::LocalOperator,
                &waiting_module,
                &waiting_guild,
                "wait",
                json!({}),
            )
            .await
    });
    let old_pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let health = manager.health().await;
            if health[&module]["host"]["in_flight"].as_u64().unwrap_or(0) > 0 {
                break health[&module]["host"]["pid"].as_u64().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let cancelling_manager = manager.clone();
    let cancelling_module = module.clone();
    let cancelling_digest = i2.digest.clone();
    let upgrading = tokio::spawn(async move {
        cancelling_manager
            .upgrade(
                &cancelling_module,
                &cancelling_digest,
                Duration::from_secs(20),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.health().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    upgrading.abort();
    assert!(upgrading.await.unwrap_err().is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    checkpoint
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        manager
            .upgrade(&module, &i2.digest, Duration::from_secs(1))
            .await
            .unwrap_err()
            .code,
        ErrorCode::StorageUnavailable
    );
    assert!(
        !Path::new(&format!("/proc/{old_pid}")).exists(),
        "retry must reap old writer before migration"
    );
    assert!(
        manager.health().await.is_empty(),
        "failed migration must not publish any normal process"
    );
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    let intent = storage.desired_modules().await.unwrap();
    assert!(
        intent
            .iter()
            .any(|d| d.module == module && d.digest == i2.digest && d.loaded)
    );
    let checkpoint_state = storage.migration_status(&module, &guild).await.unwrap();
    assert_eq!(checkpoint_state.data_version, 1);
    assert_eq!(checkpoint_state.target_version, Some(2));
    assert_eq!(
        checkpoint_state.artifact_digest.as_deref(),
        Some(i2.digest.as_str())
    );
    // Retry resumes the persisted incoming digest without resurrecting the old writer.
    manager
        .upgrade(&module, &i2.digest, Duration::from_secs(1))
        .await
        .unwrap();
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
    assert_ne!(before["generation"], after["generation"]);
    assert_ne!(before["epoch"], after["epoch"]);
    for scope in [&guild, &inactive] {
        let progress = storage.migration_status(&module, scope).await.unwrap();
        assert_eq!(progress.data_version, 2);
        assert_eq!(progress.target_version, None);
    }
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &inactive,
                "get",
                json!({})
            )
            .await
            .is_err()
    );
    assert_eq!(
        manager
            .upgrade(&module, &i1.digest, Duration::from_secs(1))
            .await
            .unwrap_err()
            .code,
        ErrorCode::DataVersionMismatch
    );
    let still_v2 = manager
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "increment",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(still_v2["value"], 2);
    assert_eq!(still_v2["generation"], after["generation"]);
    let desired = storage.desired_modules().await.unwrap();
    assert!(
        desired
            .iter()
            .any(|d| d.module == module && d.digest == i2.digest && d.loaded)
    );
    manager
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: inactive.clone(),
                active: true,
                grants: vec!["storage.own".into()],
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &inactive,
                "get",
                json!({})
            )
            .await
            .unwrap()["value"],
        1
    );
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
    fn writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if path.is_dir() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            for entry in std::fs::read_dir(path).unwrap() {
                writable(&entry.unwrap().path());
            }
        }
    }
    writable(&scratch);
    std::fs::remove_dir_all(scratch).unwrap();
}
