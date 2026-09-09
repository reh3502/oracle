use super::*;
use oracle_core::ModuleRepository;
use serde_json::json;
fn module() -> ModuleId {
    ModuleId::new("fixture.echo").unwrap()
}
fn digest() -> String {
    "a".repeat(64)
}
fn write(key: &str, revision: Option<u64>) -> DocumentWrite {
    DocumentWrite {
        collection: "docs".into(),
        key: key.into(),
        expected_revision: revision,
        value: Some(json!({"test":true})),
    }
}
fn installed() -> InstalledModule {
    InstalledModule{digest:digest(),package:serde_json::from_value(json!({"manifest":{"manifest_version":1,"id":"fixture.echo","version":"1.0.0","target":"x86_64-unknown-linux-gnu","protocol_major":1,"protocol_minor_min":0,"host_api":"^1","data_version":1,"readable_data_versions":[1],"operations":[]},"entrypoint":"bin/module","files":{},"source_revision":"test","toolchain":"test","license":"MIT"})).unwrap()}
}
pub(super) async fn exercise(config: &DatabaseConfig, store: Storage) -> Storage {
    let m = module();
    let g = GuildId::new("123").unwrap();
    let other = GuildId::new("456").unwrap();
    let d = digest();
    let package = installed();
    store.install_module(&package).await.unwrap();
    store.install_module(&package).await.unwrap();
    let mut changed = package.clone();
    changed.package.license = "changed".into();
    assert_eq!(
        store.install_module(&changed).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    store
        .set_module_desired(&DesiredModule {
            module: m.clone(),
            digest: d.clone(),
            loaded: true,
        })
        .await
        .unwrap();
    store
        .set_activation_desired(&DesiredActivation {
            module: m.clone(),
            guild: g.clone(),
            active: true,
            grants: vec!["fixture.echo".into()],
            bindings: BTreeMap::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        store.remove_installation(&d).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store.migration_status(&m, &g).await.unwrap().data_version,
        0
    );
    store.begin_migration(&m, &g, 0, 1, &d).await.unwrap();
    let page = store.migration_page(&m, &g, 100).await.unwrap();
    assert!(page.documents.is_empty());
    store
        .commit_migration_page(&m, &g, &d, None, &[], None, true)
        .await
        .unwrap();
    let all: Vec<_> = (0..153)
        .map(|i| write(&format!("key{i:03}"), None))
        .collect();
    for batch in all.chunks(100) {
        store.document_batch(&m, &g, 1, batch).await.unwrap();
    }
    assert!(
        store
            .document_get(&m, &other, "docs", "key000")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .document_get(
                &ModuleId::new("other.module").unwrap(),
                &g,
                "docs",
                "key000"
            )
            .await
            .unwrap()
            .is_none()
    );
    // A late conflict must roll back the earlier successful mutation in the same transaction.
    assert_eq!(
        store
            .document_batch(
                &m,
                &g,
                1,
                &[write("key000", Some(1)), write("key001", Some(999))]
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store
            .document_get(&m, &g, "docs", "key000")
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let mut concurrent = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let s = store.clone();
        let m = m.clone();
        let g = g.clone();
        concurrent.spawn(async move {
            s.document_batch(&m, &g, 1, &[write("key000", Some(1))])
                .await
        });
    }
    let mut wins = 0;
    while let Some(result) = concurrent.join_next().await {
        match result.unwrap() {
            Ok(_) => wins += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(wins, 1);
    let oversized = DocumentWrite {
        value: Some(json!("x".repeat(65536))),
        ..write("oversized", None)
    };
    assert_eq!(
        store
            .document_batch(&m, &g, 1, &[oversized])
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        store
            .document_batch(&m, &g, 1, &all[..101])
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    let oversized_batch: Vec<_> = (0..10)
        .map(|i| DocumentWrite {
            value: Some(json!("x".repeat(60000))),
            ..write(&format!("large{i}"), None)
        })
        .collect();
    assert_eq!(
        store
            .document_batch(&m, &g, 1, &oversized_batch)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    store.begin_migration(&m, &g, 1, 2, &d).await.unwrap();
    assert_eq!(
        store
            .begin_migration(&m, &g, 1, 2, &"b".repeat(64))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store
            .begin_migration(&m, &g, 1, 1, &d)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        store
            .document_batch(&m, &g, 1, &[write("ordinary", None)])
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let page = store.migration_page(&m, &g, 50).await.unwrap();
    assert_eq!(page.documents.len(), 50);
    let next = page.next_cursor.clone().unwrap();
    let writes: Vec<_> = page
        .documents
        .iter()
        .map(|doc| write(&doc.key, Some(doc.revision)))
        .collect();
    assert_eq!(
        store
            .commit_migration_page(&m, &g, &d, None, &writes[..49], Some(&next), false)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store
            .commit_migration_page(&m, &g, &"b".repeat(64), None, &writes, Some(&next), false)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store
            .commit_migration_page(&m, &g, &d, None, &writes, None, true)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let state = store
        .commit_migration_page(&m, &g, &d, None, &writes, Some(&next), false)
        .await
        .unwrap();
    assert_eq!(state.data_version, 1);
    assert_eq!(state.cursor.as_deref(), Some(next.as_str()));
    // Persist an issued page across a complete pool close and process-style reopen.
    let page = store.migration_page(&m, &g, 50).await.unwrap();
    let writes: Vec<_> = page
        .documents
        .iter()
        .map(|doc| write(&doc.key, Some(doc.revision)))
        .collect();
    store.close().await.unwrap();
    drop(store);
    let store = Storage::open(config.clone()).await.unwrap();
    assert_eq!(
        store
            .migration_status(&m, &g)
            .await
            .unwrap()
            .cursor
            .as_deref(),
        Some(next.as_str())
    );
    store
        .commit_migration_page(
            &m,
            &g,
            &d,
            Some(&next),
            &writes,
            page.next_cursor.as_deref(),
            false,
        )
        .await
        .unwrap();
    loop {
        let state = store.migration_status(&m, &g).await.unwrap();
        let page = store.migration_page(&m, &g, 50).await.unwrap();
        let writes: Vec<_> = page
            .documents
            .iter()
            .map(|doc| write(&doc.key, Some(doc.revision)))
            .collect();
        let complete = page.next_cursor.is_none();
        store
            .commit_migration_page(
                &m,
                &g,
                &d,
                state.cursor.as_deref(),
                &writes,
                page.next_cursor.as_deref(),
                complete,
            )
            .await
            .unwrap();
        if complete {
            break;
        }
    }
    assert_eq!(
        store.migration_status(&m, &g).await.unwrap().data_version,
        2
    );
    assert_eq!(
        store
            .document_get(&m, &g, "docs", "key152")
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert_eq!(
        store
            .begin_migration(&m, &g, 2, 1, &d)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        store
            .document_batch(&m, &g, 1, &[write("wrong-version", None)])
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    // Large valid documents force byte-bounded pages, independent of count limit.
    for batch in oversized_batch.chunks(5) {
        store.document_batch(&m, &g, 2, batch).await.unwrap();
    }
    store.begin_migration(&m, &g, 2, 3, &d).await.unwrap();
    let mut count = 0;
    loop {
        let state = store.migration_status(&m, &g).await.unwrap();
        let page = store.migration_page(&m, &g, 100).await.unwrap();
        assert!(serde_json::to_vec(&page.documents).unwrap().len() <= 512 * 1024);
        count += page.documents.len();
        let writes: Vec<_> = page
            .documents
            .iter()
            .map(|doc| DocumentWrite {
                collection: doc.collection.clone(),
                key: doc.key.clone(),
                expected_revision: Some(doc.revision),
                value: Some(doc.value.clone()),
            })
            .collect();
        let complete = page.next_cursor.is_none();
        store
            .commit_migration_page(
                &m,
                &g,
                &d,
                state.cursor.as_deref(),
                &writes,
                page.next_cursor.as_deref(),
                complete,
            )
            .await
            .unwrap();
        if complete {
            break;
        }
    }
    assert_eq!(count, 163);
    store
}
pub(super) async fn restored(store: &Storage) {
    let desired = store.desired_modules().await.unwrap();
    assert_eq!(desired.len(), 1);
    assert!(!desired[0].loaded);
    let activations = store.desired_activations().await.unwrap();
    assert_eq!(activations.len(), 1);
    assert!(activations[0].active);
    assert_eq!(activations[0].grants, vec!["fixture.echo"]);
    assert_eq!(store.installations().await.unwrap(), vec![installed()]);
    assert_eq!(
        store
            .migration_status(&module(), &GuildId::new("123").unwrap())
            .await
            .unwrap()
            .data_version,
        3
    );
}

async fn isolated(base: &DatabaseConfig, root: &Path, label: &str) -> DatabaseConfig {
    match base {
        DatabaseConfig::Sqlite { .. } => DatabaseConfig::Sqlite {
            path: root.join(format!("{label}.sqlite")),
        },
        DatabaseConfig::Postgres { url } => {
            let name = format!("oracle_stage2_{}", uuid::Uuid::new_v4().simple());
            let mut conn = PgConnection::connect(url).await.unwrap();
            let sql = format!("CREATE DATABASE \"{name}\"");
            sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            let mut parsed = url::Url::parse(url).unwrap();
            parsed.set_path(&name);
            DatabaseConfig::Postgres {
                url: parsed.to_string(),
            }
        }
    }
}
async fn remove_isolated(base: &DatabaseConfig, target: &DatabaseConfig) {
    if let (DatabaseConfig::Postgres { url }, DatabaseConfig::Postgres { url: target }) =
        (base, target)
    {
        let name = url::Url::parse(target)
            .unwrap()
            .path()
            .trim_start_matches('/')
            .to_string();
        let mut conn = PgConnection::connect(url).await.unwrap();
        let sql = format!("DROP DATABASE \"{name}\" WITH (FORCE)");
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }
}
pub(super) async fn upgrades(base: &DatabaseConfig, root: &Path, tools: &PgTools) -> Result<()> {
    let source = isolated(base, root, "legacy-source").await;
    let destination = isolated(base, root, "legacy-restore").await;
    let store = Storage::connect(normalize(source.clone()).unwrap(), None)
        .await
        .unwrap();
    write_tx!(&store, tx, {
        sqlx::query(
            "CREATE TABLE oracle_migrations(version BIGINT PRIMARY KEY,checksum TEXT NOT NULL)",
        )
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::raw_sql(MIGRATION)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("INSERT INTO oracle_migrations(version,checksum) VALUES(1,$1)")
            .bind(checksum(MIGRATION.as_bytes()))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("INSERT INTO oracle_deployment(singleton,id,restored) VALUES(1,$1,0)")
            .bind(DeploymentId::generate().as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        Ok(())
    })
    .unwrap();
    let g = GuildId::new("123").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&g))
        .await
        .unwrap();
    let original = store.status(None).await.unwrap().deployment;
    let old_bundle = root.join("old-format1");
    let mut manifest = store.backup(&old_bundle, tools).await.unwrap();
    assert_eq!(manifest.migrations.len(), 1);
    manifest.format = 1;
    manifest.migrations.clear();
    let mut legacy = serde_json::to_value(&manifest).unwrap();
    legacy.as_object_mut().unwrap().remove("migrations");
    std::fs::write(
        old_bundle.join("manifest.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    store.close().await.unwrap();
    drop(store);
    // A failed required backup must leave all old schema and data untouched.
    let blocked = root.join("backup-root-is-file");
    std::fs::write(&blocked, b"occupied").unwrap();
    assert!(
        Storage::open_with_options(source.clone(), tools, &blocked)
            .await
            .is_err()
    );
    let raw = Storage::connect(normalize(source.clone()).unwrap(), None)
        .await
        .unwrap();
    assert_eq!(raw.schema_version().await.unwrap(), 1);
    assert_eq!(raw.status(None).await.unwrap().deployment, original);
    raw.close().await.unwrap();
    drop(raw);
    if matches!(source, DatabaseConfig::Postgres { .. }) {
        match Storage::open(source.clone()).await {
            Err(e) => assert_eq!(e.code, ErrorCode::Backup),
            Ok(_) => panic!("PostgreSQL schema1 guessed backup destination"),
        }
    }
    let backups = root.join("upgrades");
    let upgraded = Storage::open_with_options(source.clone(), tools, &backups)
        .await
        .unwrap();
    assert_eq!(upgraded.schema_version().await.unwrap(), 2);
    assert_eq!(upgraded.status(None).await.unwrap().deployment, original);
    let bundles: Vec<_> = std::fs::read_dir(&backups).unwrap().collect();
    assert_eq!(bundles.len(), 1);
    let saved: BackupManifest = serde_json::from_slice(
        &std::fs::read(bundles[0].as_ref().unwrap().path().join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.migrations, vec![(1, checksum(MIGRATION.as_bytes()))]);
    upgraded.close().await.unwrap();
    drop(upgraded);
    let restored = Storage::restore(destination.clone(), &old_bundle, tools)
        .await
        .unwrap();
    assert_eq!(restored.schema_version().await.unwrap(), 2);
    let status = restored.status(None).await.unwrap();
    assert_ne!(status.deployment, original);
    assert!(status.guilds[0].paused);
    assert!(restored.desired_modules().await.unwrap().is_empty());
    restored.close().await.unwrap();
    drop(restored);
    remove_isolated(base, &source).await;
    remove_isolated(base, &destination).await;
    Ok(())
}
