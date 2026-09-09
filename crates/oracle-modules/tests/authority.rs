//! Real adversarial native RPC fixture; callbacks must derive all authority from host leases.
use oracle_core::*;
use oracle_modules::ModuleManager;
use oracle_storage::{DatabaseConfig, PgTools, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
async fn install(
    manager: &Arc<ModuleManager>,
    root: &Path,
    scratch: &Path,
    binary: &Path,
    v2: bool,
) -> InstalledModule {
    let path = scratch.join(if v2 { "package-v2" } else { "package-v1" });
    std::fs::create_dir(&path).unwrap();
    let bytes = std::fs::read(binary)
        .expect("supply separately built ORACLE_AUTHORITY_V1 / ORACLE_AUTHORITY_V2");
    std::fs::write(path.join("module"), &bytes).unwrap();
    let manifest: ModuleManifest = serde_json::from_slice(
        &std::fs::read(root.join(if v2 {
            "examples/modules/authority-probe/manifest-v2.json"
        } else {
            "examples/modules/authority-probe/manifest.json"
        }))
        .unwrap(),
    )
    .unwrap();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "adversarial-authority-test".into(),
        toolchain: "separate default and v2 cargo builds".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        path.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    manager.install(&path, true).await.unwrap()
}
fn batch(handle: Value, key: &str, revision: Option<u64>, value: Value) -> Value {
    json!({"invocation":handle,"writes":[{"collection":"probes","key":key,"expected_revision":revision,"value":value}]})
}
async fn probe(
    manager: &Arc<ModuleManager>,
    module: &ModuleId,
    guild: &GuildId,
    method: &str,
    params: Value,
    capture: bool,
) -> Value {
    manager
        .invoke(
            &PolicyContext::LocalOperator,
            module,
            guild,
            "probe",
            json!({"method":method,"params":params,"capture":capture}),
        )
        .await
        .unwrap()
}
async fn drill() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let v1 = PathBuf::from(
        std::env::var_os("ORACLE_AUTHORITY_V1").expect("ORACLE_AUTHORITY_V1 required"),
    );
    let v2 = PathBuf::from(
        std::env::var_os("ORACLE_AUTHORITY_V2").expect("ORACLE_AUTHORITY_V2 required"),
    );
    let scratch = std::env::temp_dir().join(format!("oracle-authority-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: scratch.join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let other: GuildId = "456".parse().unwrap();
    let restricted: GuildId = "789".parse().unwrap();
    let module: ModuleId = "fixture.authority-probe".parse().unwrap();
    let guilds = vec![guild.clone(), other.clone(), restricted.clone()];
    storage.initialize_guilds(&guilds).await.unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        guilds
            .iter()
            .cloned()
            .map(|guild| GuildPolicy {
                guild,
                operators: vec![],
            })
            .collect(),
    ));
    let manager = ModuleManager::new(storage.clone(), core, scratch.join("artifacts")).unwrap();
    assert!(manager.health().await.is_empty());
    let original = install(&manager, &root, &scratch, &v1, false).await;
    assert!(
        manager.health().await.is_empty(),
        "install executed probe code"
    );
    manager.load(&original.digest).await.unwrap();
    for scope in &guilds {
        manager
            .activate(
                &PolicyContext::LocalOperator,
                DesiredActivation {
                    module: module.clone(),
                    guild: scope.clone(),
                    active: true,
                    grants: if scope == &restricted {
                        vec!["storage.own".into()]
                    } else {
                        vec!["storage.own".into(), "host.echo".into()]
                    },
                    bindings: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
    }
    // A real host-issued handle authorizes exactly this module and guild. Retain it
    // deliberately in a document and in the caller, then exercise it after expiry.
    let accepted = probe(
        &manager,
        &module,
        &guild,
        "host.document_batch",
        batch(
            json!("$current"),
            "record",
            None,
            json!({"owner":"first","old_handle":"$current"}),
        ),
        true,
    )
    .await;
    assert_eq!(accepted["accepted"], true);
    let old = accepted["handle"].clone();
    assert!(old.is_string());
    let second = probe(
        &manager,
        &module,
        &other,
        "host.document_batch",
        batch(
            json!("$current"),
            "record",
            None,
            json!({"owner":"other","old_handle":"$current"}),
        ),
        true,
    )
    .await;
    assert_eq!(second["accepted"], true);
    assert_ne!(old, second["handle"]);
    for (field, forged) in [
        ("guild", json!(other)),
        ("module", json!("unrelated.module")),
        ("generation", json!(999)),
        ("epoch", json!(999)),
        ("session", json!("forged")),
    ] {
        let mut params = batch(
            json!("$current"),
            "record",
            Some(1),
            json!({"owner":"forged"}),
        );
        params[field] = forged;
        assert_eq!(
            probe(
                &manager,
                &module,
                &guild,
                "host.document_batch",
                params,
                false
            )
            .await["accepted"],
            false,
            "forged {field} admitted"
        );
        let own = storage
            .document_get(&module, &guild, "probes", "record")
            .await
            .unwrap()
            .unwrap();
        let foreign = storage
            .document_get(&module, &other, "probes", "record")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(own.revision, 1);
        assert_eq!(own.value["owner"], "first");
        assert_eq!(foreign.revision, 1);
        assert_eq!(foreign.value["owner"], "other");
    }
    for handle in [json!("never-issued"), old.clone()] {
        assert_eq!(
            probe(
                &manager,
                &module,
                &guild,
                "host.document_batch",
                batch(handle.clone(), "record", Some(1), json!({"owner":"replay"})),
                false
            )
            .await["accepted"],
            false
        );
        assert_eq!(
            probe(
                &manager,
                &module,
                &guild,
                "host.echo",
                json!({"invocation":handle,"purpose":"forged","body":{}}),
                false
            )
            .await["accepted"],
            false
        );
    }
    // Foreign identity embedded in a batch mutation is rejected, not ignored.
    let mut nested = batch(
        json!("$current"),
        "record",
        Some(1),
        json!({"owner":"cross-guild"}),
    );
    nested["writes"][0]["guild"] = json!(other);
    assert_eq!(
        probe(
            &manager,
            &module,
            &guild,
            "host.document_batch",
            nested,
            false
        )
        .await["accepted"],
        false
    );
    assert!(
        storage
            .document_get(
                &"unrelated.module".parse().unwrap(),
                &guild,
                "probes",
                "record"
            )
            .await
            .unwrap()
            .is_none()
    );
    // Operation capabilities are checked before this adversarial code can dispatch.
    assert_eq!(manager.invoke(&PolicyContext::LocalOperator,&module,&restricted,"probe",json!({"method":"host.echo","params":{"invocation":"$current","purpose":"ungranted","body":{}}})).await.unwrap_err().code,ErrorCode::ForbiddenPermission);
    assert!(
        storage
            .document_get(&module, &restricted, "probes", "record")
            .await
            .unwrap()
            .is_none()
    );
    let old_generation = manager.health().await[&module]["host"]["generation"].clone();
    manager
        .unload(&module, Duration::from_secs(1))
        .await
        .unwrap();
    manager.load(&original.digest).await.unwrap();
    assert_ne!(
        manager.health().await[&module]["host"]["generation"],
        old_generation
    );
    assert_eq!(
        probe(
            &manager,
            &module,
            &guild,
            "host.document_batch",
            batch(old, "record", Some(1), json!({"owner":"old-generation"})),
            false
        )
        .await["accepted"],
        false
    );
    // The raw migration process attempts storage and external-effect callbacks using
    // the retained old handles from real documents. Host normal-mode fencing must
    // reject both before the migrator can return its authorized CAS transform.
    let incoming = install(&manager, &root, &scratch, &v2, true).await;
    manager
        .upgrade(&module, &incoming.digest, Duration::from_secs(1))
        .await
        .unwrap();
    for (scope, owner) in [(&guild, "first"), (&other, "other")] {
        let doc = storage
            .document_get(&module, scope, "probes", "record")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.value["owner"], owner);
        assert_eq!(doc.value["migration_authority_denied"], true);
        assert_eq!(doc.revision, 2);
        assert_eq!(
            storage
                .migration_status(&module, scope)
                .await
                .unwrap()
                .data_version,
            2
        );
        assert!(
            storage
                .document_get(&module, scope, "probes", "migration-escape")
                .await
                .unwrap()
                .is_none()
        );
    }
    let backup = storage
        .backup(&scratch.join("audit"), &PgTools::default())
        .await
        .unwrap();
    assert_eq!(
        backup.table_counts["oracle_effects"], 0,
        "rejected authority created an effect"
    );
    assert_eq!(
        backup.table_counts["oracle_operations"], 0,
        "rejected authority entered the effect journal"
    );
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
    fn writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                writable(&entry.path())
            }
        }
    }
    writable(&scratch);
    std::fs::remove_dir_all(scratch).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires separately built ORACLE_AUTHORITY_V1 and ORACLE_AUTHORITY_V2 binaries"]
async fn forged_scopes_expired_handles_and_migration_callbacks_never_gain_authority() {
    tokio::time::timeout(Duration::from_secs(60), drill())
        .await
        .expect("authority drill exceeded cleanup deadline");
}
