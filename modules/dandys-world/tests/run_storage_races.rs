//! Deterministic two-writer schedules against the actual database/CAS boundary.
use async_trait::async_trait;
use dandys_world_core::runs::{
    domain::*,
    storage::{self, ConfirmationToken, DAY, Documents, Request, Response, RunService},
};
use oracle_core::{DocumentWrite, ErrorCode, GuildId, ModuleDocument, ModuleId, ModuleRepository};
use oracle_storage::{DatabaseConfig, Storage};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{Barrier, Semaphore};
const NOW: u64 = 1_800_000_000_000;
static INTERACTION: AtomicU64 = AtomicU64::new(10_000);
fn interaction(now: u64) -> String {
    (((now - 1_420_070_400_000) << 22) + INTERACTION.fetch_add(1, Ordering::Relaxed)).to_string()
}
fn actor(id: &str) -> Actor {
    Actor {
        guild_id: "987".into(),
        user_id: id.into(),
        manage_all_runs: false,
    }
}
fn catalog() -> EligibilitySnapshot {
    EligibilitySnapshot {
        source_hash: "a".repeat(64),
        source_revisions: BTreeMap::from([("Toons".into(), 42)]),
        toons: BTreeMap::from([
            ("pebble".into(), "Pebble".into()),
            ("poppy".into(), "Poppy".into()),
        ]),
        observed_at: NOW - 1,
        fresh_until: NOW + 7 * DAY,
        disputed: false,
    }
}
fn dummy() -> Confirmation {
    Confirmation {
        actor_id: "untrusted".into(),
        revision: u64::MAX,
    }
}
#[derive(Clone)]
struct Schedule {
    run_id: String,
    barrier: Arc<Barrier>,
    first_read: Arc<AtomicBool>,
    first_batch: Arc<AtomicBool>,
    versions: Arc<Mutex<Vec<u64>>>,
    leader: bool,
    committed: Arc<Semaphore>,
}
#[derive(Clone)]
struct Db {
    storage: Arc<Storage>,
    guild: GuildId,
    module: ModuleId,
    schedule: Option<Schedule>,
}
impl Db {
    fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            guild: "987".parse().unwrap(),
            module: "community.dandys-world".parse().unwrap(),
            schedule: None,
        }
    }
}
fn map_error(error: oracle_core::Error) -> storage::Error {
    if error.code == ErrorCode::Conflict {
        storage::Error::Conflict
    } else {
        panic!("database failure: {error:?}")
    }
}
#[async_trait]
impl Documents for Db {
    fn guild(&self) -> &str {
        self.guild.as_str()
    }
    async fn get(&self, collection: &str, key: &str) -> storage::Result<Option<ModuleDocument>> {
        let document = self
            .storage
            .document_get(&self.module, &self.guild, collection, key)
            .await
            .map_err(map_error)?;
        if let Some(schedule) = &self.schedule
            && collection == "runs"
            && key == schedule.run_id
            && schedule.first_read.swap(false, Ordering::SeqCst)
        {
            schedule
                .versions
                .lock()
                .unwrap()
                .push(document.as_ref().unwrap().revision);
            schedule.barrier.wait().await;
        }
        Ok(document)
    }
    async fn batch(&self, writes: Vec<DocumentWrite>) -> storage::Result<()> {
        let first = self
            .schedule
            .as_ref()
            .is_some_and(|s| s.first_batch.swap(false, Ordering::SeqCst));
        if let Some(schedule) = &self.schedule
            && first
            && !schedule.leader
        {
            schedule.committed.acquire().await.unwrap().forget();
        }
        let result = self
            .storage
            .document_batch(&self.module, &self.guild, 2, &writes)
            .await
            .map(|_| ())
            .map_err(map_error);
        if let Some(schedule) = &self.schedule
            && first
            && schedule.leader
        {
            schedule.committed.add_permits(1);
        }
        result
    }
}
async fn initialize(config: DatabaseConfig) -> Db {
    let storage = Arc::new(Storage::open(config).await.unwrap());
    let db = Db::new(storage);
    db.storage
        .initialize_guilds(std::slice::from_ref(&db.guild))
        .await
        .unwrap();
    let digest = "a".repeat(64);
    db.storage
        .begin_migration(&db.module, &db.guild, 0, 2, &digest)
        .await
        .unwrap();
    db.storage
        .migration_page(&db.module, &db.guild, 100)
        .await
        .unwrap();
    db.storage
        .commit_migration_page(&db.module, &db.guild, &digest, None, &[], None, true)
        .await
        .unwrap();
    db
}
async fn execute(
    service: &RunService<Db>,
    actor: &Actor,
    request: Request,
    now: u64,
) -> storage::Result<Response> {
    service
        .execute(actor, &interaction(now), request, &catalog(), now)
        .await
}
async fn change(
    service: &RunService<Db>,
    actor: &Actor,
    id: &str,
    command: Command,
) -> storage::Result<Response> {
    execute(
        service,
        actor,
        Request::Change {
            run_id: id.into(),
            command,
            confirmation: None,
        },
        NOW,
    )
    .await
}
async fn create(service: &RunService<Db>, owner: &Actor, mode: RunMode) -> String {
    let Response::Applied { result } = execute(
        service,
        owner,
        Request::Create {
            mode,
            name: Some("Race qualification".into()),
        },
        NOW,
    )
    .await
    .unwrap() else {
        panic!()
    };
    result.run_id
}
async fn setup(service: &RunService<Db>, owner: &Actor, mode: RunMode) -> String {
    let id = create(service, owner, mode).await;
    if mode == RunMode::Organized {
        change(
            service,
            owner,
            &id,
            Command::EditDraft {
                name: None,
                allocations: Some(BTreeMap::from([("pebble".into(), 1), ("poppy".into(), 1)])),
                host_toon: Some("pebble".into()),
            },
        )
        .await
        .unwrap();
    }
    change(service, owner, &id, Command::Publish).await.unwrap();
    id
}
async fn prepare(
    service: &RunService<Db>,
    owner: &Actor,
    id: &str,
    command: Command,
) -> ConfirmationToken {
    let Response::Confirm { confirmation, .. } = execute(
        service,
        owner,
        Request::Prepare {
            run_id: id.into(),
            command,
        },
        NOW,
    )
    .await
    .unwrap() else {
        panic!()
    };
    confirmation
}
fn request(id: &str, command: Command) -> Request {
    Request::Change {
        run_id: id.into(),
        command,
        confirmation: None,
    }
}
fn rule(result: storage::Result<Response>, expected: Error) {
    match result {
        Err(storage::Error::Rule(actual)) => assert_eq!(actual, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}
async fn race(
    db: &Db,
    id: &str,
    first_actor: Actor,
    first: Request,
    second_actor: Actor,
    second: Request,
) -> (storage::Result<Response>, storage::Result<Response>) {
    let barrier = Arc::new(Barrier::new(2));
    let committed = Arc::new(Semaphore::new(0));
    let versions = Arc::new(Mutex::new(Vec::new()));
    let writer = |leader| {
        let mut db = db.clone();
        db.schedule = Some(Schedule {
            run_id: id.into(),
            barrier: barrier.clone(),
            first_read: Arc::new(AtomicBool::new(true)),
            first_batch: Arc::new(AtomicBool::new(true)),
            versions: versions.clone(),
            leader,
            committed: committed.clone(),
        });
        RunService::new(db)
    };
    let first_service = writer(true);
    let second_service = writer(false);
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(
            execute(&first_service, &first_actor, first, NOW),
            execute(&second_service, &second_actor, second, NOW)
        )
    })
    .await
    .expect("two writers must finish without a lock-order deadlock");
    let versions = versions.lock().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(
        versions[0], versions[1],
        "both attempts must read the same database revision"
    );
    result
}

async fn lifecycle_races(db: &Db) {
    let service = RunService::new(db.clone());
    for join_first in [true, false] {
        let owner = actor(if join_first { "100" } else { "101" });
        let member = actor("200");
        let id = setup(&service, &owner, RunMode::Organized).await;
        let join = request(
            &id,
            Command::Join {
                toon: Some("poppy".into()),
            },
        );
        let lock = request(&id, Command::Lock);
        let (joined, locked) = if join_first {
            race(db, &id, member.clone(), join, owner.clone(), lock).await
        } else {
            let (locked, joined) = race(db, &id, owner.clone(), lock, member.clone(), join).await;
            (joined, locked)
        };
        locked.unwrap();
        if join_first {
            joined.unwrap();
        } else {
            rule(joined, Error::Closed);
        }
        let stored = service.view(&owner, &id).await.unwrap();
        assert_eq!(stored.run.state, RunState::Locked);
        assert_eq!(stored.run.assignments.len(), if join_first { 2 } else { 1 });
        assert_eq!(
            stored.publication.unwrap().desired_revision,
            stored.run.desired_card_revision
        );
    }
    for join_first in [true, false] {
        let owner = actor(if join_first { "110" } else { "111" });
        let member = actor("210");
        let id = setup(&service, &owner, RunMode::Organized).await;
        let command = Command::Cancel {
            confirmation: dummy(),
        };
        let token = prepare(&service, &owner, &id, command.clone()).await;
        let cancel = Request::Change {
            run_id: id.clone(),
            command,
            confirmation: Some(token),
        };
        let join = request(
            &id,
            Command::Join {
                toon: Some("poppy".into()),
            },
        );
        let (joined, cancelled) = if join_first {
            race(db, &id, member.clone(), join, owner.clone(), cancel).await
        } else {
            let (cancelled, joined) =
                race(db, &id, owner.clone(), cancel, member.clone(), join).await;
            (joined, cancelled)
        };
        let stored = service.view(&owner, &id).await.unwrap();
        if join_first {
            joined.unwrap();
            rule(cancelled, Error::StaleConfirmation);
            assert_eq!(stored.run.state, RunState::Open);
            assert_eq!(stored.run.assignments.len(), 2);
        } else {
            cancelled.unwrap();
            rule(joined, Error::Closed);
            assert_eq!(stored.run.state, RunState::Cancelled);
            assert_eq!(stored.run.assignments.len(), 1);
        }
        assert_eq!(
            stored.publication.unwrap().desired_revision,
            stored.run.desired_card_revision
        );
    }
}
async fn allocation_races(db: &Db) {
    let service = RunService::new(db.clone());
    for join_first in [true, false] {
        let owner = actor(if join_first { "120" } else { "121" });
        let member = actor("220");
        let id = setup(&service, &owner, RunMode::Organized).await;
        let join = request(
            &id,
            Command::Join {
                toon: Some("poppy".into()),
            },
        );
        let edit = request(
            &id,
            Command::SetAllocations {
                allocations: BTreeMap::from([("pebble".into(), 1)]),
            },
        );
        let (joined, edited) = if join_first {
            race(db, &id, member.clone(), join, owner.clone(), edit).await
        } else {
            let (edited, joined) = race(db, &id, owner.clone(), edit, member.clone(), join).await;
            (joined, edited)
        };
        let stored = service.view(&owner, &id).await.unwrap();
        if join_first {
            joined.unwrap();
            rule(edited, Error::BelowOccupancy);
            assert_eq!(stored.run.capacity(), 2);
            assert_eq!(stored.run.assignments["220"].toon.as_deref(), Some("poppy"));
        } else {
            edited.unwrap();
            rule(joined, Error::UnallocatedToon);
            assert_eq!(stored.run.capacity(), 1);
            assert!(!stored.run.assignments.contains_key("220"));
        }
        assert_eq!(
            stored.publication.unwrap().desired_revision,
            stored.run.desired_card_revision
        );
    }
}
async fn confirmation_bindings(db: &Db) {
    let service = RunService::new(db.clone());
    let owner = actor("300");
    let member = actor("301");
    let mut moderator = actor("302");
    moderator.manage_all_runs = true;
    let id = setup(&service, &owner, RunMode::Casual).await;
    let other = setup(&service, &owner, RunMode::Casual).await;
    change(&service, &member, &id, Command::Join { toon: None })
        .await
        .unwrap();
    let remove = Command::Remove {
        member_id: member.user_id.clone(),
        confirmation: dummy(),
    };
    let token = prepare(&service, &owner, &id, remove.clone()).await;
    let saved = service.view(&owner, &id).await.unwrap();
    // A valid token cannot be forwarded to another actor, run, action, or target.
    for (who, run_id, command) in [
        (moderator.clone(), id.clone(), remove.clone()),
        (owner.clone(), other.clone(), remove.clone()),
        (
            owner.clone(),
            id.clone(),
            Command::Cancel {
                confirmation: dummy(),
            },
        ),
        (
            owner.clone(),
            id.clone(),
            Command::Remove {
                member_id: owner.user_id.clone(),
                confirmation: dummy(),
            },
        ),
    ] {
        rule(
            execute(
                &service,
                &who,
                Request::Change {
                    run_id,
                    command,
                    confirmation: Some(token.clone()),
                },
                NOW,
            )
            .await,
            Error::StaleConfirmation,
        );
    }
    assert_eq!(service.view(&owner, &id).await.unwrap(), saved);
    change(
        &service,
        &owner,
        &id,
        Command::Rename {
            name: "New review needed".into(),
        },
    )
    .await
    .unwrap();
    rule(
        execute(
            &service,
            &owner,
            Request::Change {
                run_id: id.clone(),
                command: remove.clone(),
                confirmation: Some(token),
            },
            NOW,
        )
        .await,
        Error::StaleConfirmation,
    );
    let token = prepare(&service, &owner, &id, remove.clone()).await;
    rule(
        execute(
            &service,
            &owner,
            Request::Change {
                run_id: id.clone(),
                command: remove.clone(),
                confirmation: Some(token),
            },
            NOW + 5 * 60_000,
        )
        .await,
        Error::StaleConfirmation,
    );
    let token = prepare(&service, &moderator, &id, remove.clone()).await;
    let change = Request::Change {
        run_id: id.clone(),
        command: remove.clone(),
        confirmation: Some(token),
    };
    let invocation = interaction(NOW);
    let result = service
        .execute(&moderator, &invocation, change.clone(), &catalog(), NOW)
        .await
        .unwrap();
    assert_eq!(
        service
            .execute(&moderator, &invocation, change.clone(), &catalog(), NOW)
            .await
            .unwrap(),
        result
    );
    rule(
        execute(&service, &moderator, change, NOW).await,
        Error::StaleConfirmation,
    );
    let removed = service.view(&owner, &id).await.unwrap();
    assert!(!removed.run.assignments.contains_key(&member.user_id));
    assert_eq!(removed.run.owner_id, owner.user_id);
    assert_eq!(removed.moderator_audit.len(), 1);
    assert_eq!(removed.moderator_audit[0].actor_id, moderator.user_id);
    assert_eq!(removed.moderator_audit[0].action, "remove");
    assert_eq!(
        removed.moderator_audit[0].affected_member,
        Some(member.user_id)
    );
}
async fn all_mode_confirmation(db: &Db) {
    let service = RunService::new(db.clone());
    for configured in [false, true] {
        let owner = actor(if configured { "401" } else { "400" });
        let id = create(&service, &owner, RunMode::Organized).await;
        if configured {
            change(
                &service,
                &owner,
                &id,
                Command::EditDraft {
                    name: Some("Organized".into()),
                    allocations: Some(BTreeMap::from([("pebble".into(), 1), ("poppy".into(), 7)])),
                    host_toon: Some("pebble".into()),
                },
            )
            .await
            .unwrap();
        }
        let command = Command::SetMode {
            mode: RunMode::Casual,
            confirmation: None,
        };
        let before = service.view(&owner, &id).await.unwrap();
        let token = prepare(&service, &owner, &id, command.clone()).await;
        // Preparing/cancelling a private review has no effect on the saved setup.
        assert_eq!(service.view(&owner, &id).await.unwrap(), before);
        rule(
            change(&service, &owner, &id, command.clone()).await,
            Error::ConfirmationRequired,
        );
        assert_eq!(service.view(&owner, &id).await.unwrap(), before);
        execute(
            &service,
            &owner,
            Request::Change {
                run_id: id.clone(),
                command,
                confirmation: Some(token),
            },
            NOW,
        )
        .await
        .unwrap();
        let casual = service.view(&owner, &id).await.unwrap();
        assert_eq!(casual.run.mode, RunMode::Casual);
        assert!(casual.run.allocations.is_none());
        assert!(casual.run.host_toon.is_none());
        assert_eq!(casual.run.capacity(), 8);
        change(&service, &owner, &id, Command::Publish)
            .await
            .unwrap();
        change(
            &service,
            &owner,
            &id,
            Command::Rename {
                name: "Organized by name only".into(),
            },
        )
        .await
        .unwrap();
        let published = service.view(&owner, &id).await.unwrap();
        assert_eq!(published.run.mode, RunMode::Casual);
        assert_eq!(published.run.assignments[&owner.user_id].toon, None);
        rule(
            change(
                &service,
                &owner,
                &id,
                Command::SetMode {
                    mode: RunMode::Organized,
                    confirmation: None,
                },
            )
            .await,
            Error::ModeImmutable,
        );
    }
}
async fn exact_cancelled_draft_retention(db: &Db) {
    let service = RunService::new(db.clone());
    let owner = actor("500");
    let id = create(&service, &owner, RunMode::Casual).await;
    let command = Command::Cancel {
        confirmation: dummy(),
    };
    let token = prepare(&service, &owner, &id, command.clone()).await;
    let terminal_at = NOW + 37_000;
    execute(
        &service,
        &owner,
        Request::Change {
            run_id: id.clone(),
            command,
            confirmation: Some(token),
        },
        terminal_at,
    )
    .await
    .unwrap();
    let retained = service.view(&owner, &id).await.unwrap();
    assert!(retained.publication.is_none());
    assert_eq!(retained.run.terminal_at, Some(terminal_at));
    let cutoff = terminal_at + 30 * DAY;
    // Receipts are also expired by this point; bounded maintenance drains them
    // independently without shortening or renewing the aggregate's retention.
    for _ in 0..8 {
        let report = service.cleanup(cutoff - 1, 32).await.unwrap();
        assert_eq!(report.terminal, 0);
        if report.receipts == 0 {
            break;
        }
    }
    assert_eq!(service.view(&owner, &id).await.unwrap(), retained);
    let report = service.cleanup(cutoff, 32).await.unwrap();
    assert_eq!(report.terminal, 1);
    assert!(matches!(
        service.view(&owner, &id).await,
        Err(storage::Error::NotFound)
    ));
    assert!(db.get("runs", &id).await.unwrap().is_none());
    let shard = format!("tombstones/{}", id.as_bytes()[0] % 16);
    let tombstone = db.get("run_index", &shard).await.unwrap().unwrap();
    assert_eq!(tombstone.value[&id].as_u64(), Some(cutoff + 30 * DAY));
    assert!(
        !serde_json::to_string(&tombstone.value)
            .unwrap()
            .contains("owner_id")
    );
}
async fn qualify(config: DatabaseConfig) {
    let db = initialize(config).await;
    lifecycle_races(&db).await;
    allocation_races(&db).await;
    confirmation_bindings(&db).await;
    all_mode_confirmation(&db).await;
    exact_cancelled_draft_retention(&db).await;
    db.storage.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_run_management_races_are_linearizable_and_confirmations_bound() {
    let directory = std::env::temp_dir().join(format!(
        "dw-races-{}-{}",
        std::process::id(),
        interaction(NOW)
    ));
    std::fs::create_dir(&directory).unwrap();
    qualify(DatabaseConfig::Sqlite {
        path: directory.join("state.sqlite"),
    })
    .await;
    std::fs::remove_dir_all(directory).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a fresh disposable PostgreSQL database in DW_TEST_POSTGRES_URL"]
async fn postgres_run_management_races_are_linearizable_and_confirmations_bound() {
    qualify(DatabaseConfig::Postgres {
        url: std::env::var("DW_TEST_POSTGRES_URL").expect("fresh isolated database required"),
    })
    .await;
}
