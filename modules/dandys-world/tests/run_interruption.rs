//! Abrupt process death at the real Documents batch boundary. The pre-commit
//! case stops before entering the database transaction; the post-commit case
//! stops after the database acknowledges commit but before RunService replies.
//! This qualifies application process recovery with SQLite and PostgreSQL, not
//! power loss or an interrupted database engine transaction.
use async_trait::async_trait;
use dandys_world_core::runs::{
    domain::*,
    storage::{self, Documents, Request, Response, RunService},
    ui::public_projection,
};
use oracle_core::{DocumentWrite, GuildId, ModuleDocument, ModuleId, ModuleRepository};
use oracle_storage::{DatabaseConfig, Storage};
use std::{collections::BTreeMap, path::PathBuf, process::Child, sync::Arc, time::Duration};

const NOW: u64 = 1_800_000_000_000;
fn interaction(sequence: u64) -> String {
    (((NOW - 1_420_070_400_000) << 22) + sequence).to_string()
}
fn actor(user: &str) -> Actor {
    Actor {
        guild_id: "123".into(),
        user_id: user.into(),
        manage_all_runs: false,
    }
}
fn catalog() -> EligibilitySnapshot {
    EligibilitySnapshot {
        source_hash: "a".repeat(64),
        source_revisions: BTreeMap::from([("Toons".into(), 42)]),
        toons: BTreeMap::from([("pebble".into(), "Pebble".into())]),
        observed_at: NOW - 1,
        fresh_until: NOW + storage::DAY,
        disputed: false,
    }
}
struct Db {
    store: Arc<Storage>,
    guild: GuildId,
    module: ModuleId,
    stop: Option<(String, PathBuf)>,
}
impl Db {
    fn new(store: Arc<Storage>, module: &str) -> Self {
        Self {
            store,
            guild: "123".parse().unwrap(),
            module: module.parse().unwrap(),
            stop: None,
        }
    }
    async fn boundary(&self, point: &str) {
        if let Some((expected, marker)) = &self.stop
            && expected == point
        {
            let pending_marker = marker.with_extension("pending");
            std::fs::write(&pending_marker, point).unwrap();
            std::fs::rename(pending_marker, marker).unwrap();
            // Only the parent can release this boundary, by killing the process.
            std::future::pending::<()>().await;
        }
    }
}
#[async_trait]
impl Documents for Db {
    fn guild(&self) -> &str {
        self.guild.as_str()
    }
    async fn get(&self, collection: &str, key: &str) -> storage::Result<Option<ModuleDocument>> {
        Ok(self
            .store
            .document_get(&self.module, &self.guild, collection, key)
            .await
            .unwrap())
    }
    async fn batch(&self, writes: Vec<DocumentWrite>) -> storage::Result<()> {
        if self.stop.is_some() {
            assert!(writes.iter().any(|w| w.collection == "runs"));
            assert!(
                writes
                    .iter()
                    .any(|w| w.collection == "run_receipts" && w.key == interaction(9))
            );
        }
        self.boundary("before_commit").await;
        self.store
            .document_batch(&self.module, &self.guild, storage::DATA_VERSION, &writes)
            .await
            .unwrap();
        self.boundary("after_commit").await;
        Ok(())
    }
}
fn config(root: &std::path::Path, postgres: bool) -> DatabaseConfig {
    if postgres {
        DatabaseConfig::Postgres {
            url: std::env::var("DW_TEST_POSTGRES_URL")
                .expect("fresh isolated qualification database"),
        }
    } else {
        DatabaseConfig::Sqlite {
            path: root.join("state.sqlite"),
        }
    }
}
async fn execute(
    service: &RunService<Db>,
    user: &str,
    sequence: u64,
    request: Request,
) -> Response {
    service
        .execute(
            &actor(user),
            &interaction(sequence),
            request,
            &catalog(),
            NOW,
        )
        .await
        .unwrap()
}
fn change(id: &str, command: Command) -> Request {
    Request::Change {
        run_id: id.into(),
        command,
        confirmation: None,
        expected_revision: None,
    }
}
async fn fixture(root: &std::path::Path, postgres: bool, module: &str) -> String {
    let store = Arc::new(Storage::open(config(root, postgres)).await.unwrap());
    let db = Db::new(store.clone(), module);
    store
        .initialize_guilds(std::slice::from_ref(&db.guild))
        .await
        .unwrap();
    let digest = "a".repeat(64);
    store
        .begin_migration(&db.module, &db.guild, 0, storage::DATA_VERSION, &digest)
        .await
        .unwrap();
    assert!(
        store
            .migration_page(&db.module, &db.guild, 100)
            .await
            .unwrap()
            .documents
            .is_empty()
    );
    store
        .commit_migration_page(&db.module, &db.guild, &digest, None, &[], None, true)
        .await
        .unwrap();
    let service = RunService::new(db);
    let Response::Applied { result } = execute(
        &service,
        "100",
        1,
        Request::Create {
            mode: RunMode::Casual,
            name: Some("Interruption qualification".into()),
        },
    )
    .await
    else {
        panic!()
    };
    let id = result.run_id;
    execute(
        &service,
        "100",
        2,
        change(
            &id,
            Command::SetSchedule {
                schedule: RunSchedule {
                    starts_at: 4_070_908_800,
                    duration_minutes: 90,
                    timezone: Some("UTC".into()),
                },
            },
        ),
    )
    .await;
    execute(&service, "100", 3, change(&id, Command::Publish)).await;
    store.close().await.unwrap();
    id
}

// Invoked only by the parent test with a fresh disposable database path.
#[tokio::test]
#[ignore = "subprocess helper; parent supplies the isolated database and kill boundary"]
async fn interrupted_writer_child() {
    let root = PathBuf::from(std::env::var_os("DW_INTERRUPTION_ROOT").expect("parent test root"));
    let postgres = std::env::var("DW_INTERRUPTION_BACKEND").unwrap() == "postgres";
    let module = std::env::var("DW_INTERRUPTION_MODULE").unwrap();
    let store = Arc::new(Storage::open(config(&root, postgres)).await.unwrap());
    let mut db = Db::new(store, &module);
    db.stop = Some((
        std::env::var("DW_INTERRUPTION_POINT").unwrap(),
        root.join("boundary"),
    ));
    let id = std::env::var("DW_INTERRUPTION_RUN").unwrap();
    execute(
        &RunService::new(db),
        "200",
        9,
        change(
            &id,
            Command::Join {
                toon: Some("pebble".into()),
            },
        ),
    )
    .await;
    panic!("writer returned without stopping at the requested boundary");
}
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn qualify(point: &str, postgres: bool) {
    let backend = if postgres { "postgres" } else { "sqlite" };
    // Each boundary has a separate module namespace in the disposable database.
    // PostgreSQL cases run sequentially; the qualification runner drops the DB.
    let module = if point == "before_commit" {
        "community.dw-interruption-before"
    } else {
        "community.dw-interruption-after"
    };
    let root = std::env::temp_dir().join(format!(
        "dw-interruption-{}-{backend}-{point}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let id = fixture(&root, postgres, module).await;
    let store = Arc::new(Storage::open(config(&root, postgres)).await.unwrap());
    let service = RunService::new(Db::new(store.clone(), module));
    let before = service.view(&actor("100"), &id).await.unwrap();
    assert_eq!(before.run.state, RunState::Open);
    assert_eq!(before.run.assignments.len(), 1);
    assert!(before.run.schedule.is_some());
    store.close().await.unwrap();
    drop(service);
    drop(store);

    let output = std::fs::File::create(root.join("child.log")).unwrap();
    let mut child = KillOnDrop(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "interrupted_writer_child",
                "--ignored",
                "--nocapture",
            ])
            .env("DW_INTERRUPTION_ROOT", &root)
            .env("DW_INTERRUPTION_BACKEND", backend)
            .env("DW_INTERRUPTION_MODULE", module)
            .env("DW_INTERRUPTION_POINT", point)
            .env("DW_INTERRUPTION_RUN", &id)
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "writer exited before boundary: {}",
            std::fs::read_to_string(root.join("child.log")).unwrap()
        );
        if root.join("boundary").exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer failed to reach {point}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read_to_string(root.join("boundary")).unwrap(),
        point
    );
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());

    let store = Arc::new(Storage::open(config(&root, postgres)).await.unwrap());
    let db = Db::new(store.clone(), module);
    let receipt = db.get("run_receipts", &interaction(9)).await.unwrap();
    let service = RunService::new(Db::new(store.clone(), module));
    let reopened = service.view(&actor("100"), &id).await.unwrap();
    if point == "before_commit" {
        assert_eq!(reopened, before, "no partial signup or publication intent");
        assert!(
            receipt.is_none(),
            "uncommitted write cannot leave a receipt"
        );
    } else {
        assert!(receipt.is_some(), "committed signup must have its receipt");
        assert_eq!(reopened.run.assignments.len(), 2);
        assert!(reopened.run.assignments.contains_key("200"));
        assert_eq!(
            reopened.run.desired_card_revision,
            before.run.desired_card_revision + 1
        );
        let intent = reopened.publication.as_ref().unwrap();
        assert_eq!(intent.desired_revision, reopened.run.desired_card_revision);
        let (card, actions) = public_projection(&reopened.run);
        assert_eq!(intent.card, card);
        assert_eq!(intent.actions, actions);
    }
    let request = change(
        &id,
        Command::Join {
            toon: Some("pebble".into()),
        },
    );
    let response = execute(&service, "200", 9, request.clone()).await;
    let committed = service.view(&actor("100"), &id).await.unwrap();
    assert_eq!(committed.run.assignments.len(), 2);
    assert!(committed.run.assignments.contains_key("200"));
    assert_eq!(
        committed.run.desired_card_revision,
        before.run.desired_card_revision + 1
    );
    let intent = committed.publication.as_ref().unwrap();
    assert_eq!(intent.desired_revision, committed.run.desired_card_revision);
    let (card, actions) = public_projection(&committed.run);
    assert_eq!(intent.card, card);
    assert_eq!(intent.actions, actions);
    if point == "after_commit" {
        assert_eq!(committed, reopened);
    }
    let persisted = db.get("runs", &id).await.unwrap().unwrap();
    let receipt = db
        .get("run_receipts", &interaction(9))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execute(&service, "200", 9, request).await, response);
    assert_eq!(service.view(&actor("100"), &id).await.unwrap(), committed);
    assert_eq!(
        db.get("runs", &id).await.unwrap().unwrap().revision,
        persisted.revision
    );
    assert_eq!(
        db.get("run_receipts", &interaction(9))
            .await
            .unwrap()
            .unwrap()
            .revision,
        receipt.revision
    );
    assert_eq!(service.pending_projections().await.unwrap(), vec![id]);
    store.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn sqlite_process_killed_before_commit_replays_one_signup() {
    qualify("before_commit", false).await;
}
#[tokio::test]
async fn sqlite_process_killed_after_commit_before_reply_replays_one_signup() {
    qualify("after_commit", false).await;
}

#[tokio::test]
#[ignore = "requires a fresh disposable PostgreSQL database in DW_TEST_POSTGRES_URL"]
async fn postgres_process_killed_at_write_boundaries_replays_one_signup() {
    qualify("before_commit", true).await;
    qualify("after_commit", true).await;
}
