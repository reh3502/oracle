//! Activation remains unavailable until its durable desired-state commit succeeds.
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
use tokio::sync::{Notify, Semaphore};
struct AbortActivation(tokio::task::AbortHandle);
impl Drop for AbortActivation {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct PausedPublication {
    storage: Arc<Storage>,
    entered: Notify,
    release: Semaphore,
    fail: bool,
}
#[async_trait::async_trait]
impl ModuleRepository for PausedPublication {
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
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        if self.fail {
            return Err(Error::new(ErrorCode::StorageUnavailable));
        }
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
#[ignore = "requires ORACLE_COUNTER_V1 pointing to the current default counter binary"]
async fn activation_success_keeps_admission_closed_until_durable_ack() {
    publication_case("success").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_COUNTER_V1 pointing to the current default counter binary"]
async fn activation_failure_keeps_admission_closed_until_durable_ack() {
    publication_case("failure").await;
}
#[tokio::test]
#[ignore = "requires ORACLE_COUNTER_V1 pointing to the current default counter binary"]
async fn activation_cancel_keeps_admission_closed_until_durable_ack() {
    publication_case("cancel").await;
}
async fn publication_case(outcome: &'static str) {
    let binary =
        PathBuf::from(std::env::var_os("ORACLE_COUNTER_V1").expect("set ORACLE_COUNTER_V1"));
    let scratch = std::env::temp_dir().join(format!("oracle-publication-{}", uuid::Uuid::new_v4()));
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
    let repository = Arc::new(PausedPublication {
        storage: storage.clone(),
        entered: Notify::new(),
        release: Semaphore::new(0),
        fail: outcome == "failure",
    });
    let manager =
        ModuleManager::new(repository.clone(), core.clone(), scratch.join("artifacts")).unwrap();
    let task_manager = manager.clone();
    let task_storage = storage.clone();
    let task_scratch = scratch.clone();
    let task_binary = binary.clone();
    let test = tokio::spawn(async move {
        let manager = task_manager;
        let storage = task_storage;
        let source = package(
            &task_scratch,
            "source",
            &task_binary,
            include_str!("../../../examples/modules/counter/manifest.json"),
        );
        let installed = manager.install(&source, true).await.unwrap();
        let module = installed.package.manifest.id;
        manager.load(&installed.digest).await.unwrap();
        let activation = {
            let manager = manager.clone();
            let module = module.clone();
            let guild = guild.clone();
            tokio::spawn(async move {
                manager
                    .activate(
                        &PolicyContext::LocalOperator,
                        DesiredActivation {
                            module,
                            guild,
                            active: true,
                            grants: vec!["storage.own".into()],
                            bindings: BTreeMap::new(),
                        },
                    )
                    .await
            })
        };
        let _activation_cleanup = AbortActivation(activation.abort_handle());
        // This seam is after SDK activate acknowledgement, before persistence returns.
        tokio::time::timeout(Duration::from_secs(5), repository.entered.notified())
            .await
            .unwrap();
        let blocked_call = manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "increment",
                json!({}),
            )
            .await;
        let untouched = storage
            .document_get(&module, &guild, "counters", "main")
            .await
            .unwrap()
            .is_none();
        let unpublished = core
            .status(&PolicyContext::LocalOperator, Some(&guild))
            .await
            .unwrap()
            .modules_loaded;
        // Release/abort before assertions so a failing regression does not strand a task.
        if outcome == "cancel" {
            activation.abort();
            assert!(activation.await.unwrap_err().is_cancelled());
        } else {
            repository.release.add_permits(1);
            let result = activation.await.unwrap();
            if outcome == "failure" {
                assert_eq!(result.unwrap_err().code, ErrorCode::StorageUnavailable);
            } else {
                result.unwrap();
            }
        }
        assert!(
            blocked_call.is_err(),
            "operation reached host storage before activation commit"
        );
        assert!(untouched, "uncommitted activation changed its document");
        assert_eq!(unpublished, 0, "uncommitted activation was reported active");
        if outcome == "success" {
            assert_eq!(
                manager
                    .invoke(
                        &PolicyContext::LocalOperator,
                        &module,
                        &guild,
                        "increment",
                        json!({})
                    )
                    .await
                    .unwrap()["value"],
                1
            );
            assert_eq!(
                core.status(&PolicyContext::LocalOperator, Some(&guild))
                    .await
                    .unwrap()
                    .modules_loaded,
                1
            );
            assert!(storage.desired_activations().await.unwrap()[0].active);
        } else {
            assert!(
                manager
                    .invoke(
                        &PolicyContext::LocalOperator,
                        &module,
                        &guild,
                        "increment",
                        json!({})
                    )
                    .await
                    .is_err()
            );
            assert!(
                storage
                    .document_get(&module, &guild, "counters", "main")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                core.status(&PolicyContext::LocalOperator, Some(&guild))
                    .await
                    .unwrap()
                    .modules_loaded,
                0
            );
            assert!(
                storage
                    .desired_activations()
                    .await
                    .unwrap()
                    .iter()
                    .all(|a| !a.active)
            );
        }
    })
    .await;
    let stopped = manager.shutdown().await;
    let closed = storage.close().await;
    remove_tree(&scratch);
    if let Err(error) = test {
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
