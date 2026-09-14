use async_trait::async_trait;
use dandys_world_core::runs::{
    domain::*,
    storage::{self, DAY, Documents, Request, Response, RunService},
};
use oracle_core::{DocumentWrite, ErrorCode, GuildId, ModuleDocument, ModuleId, ModuleRepository};
use oracle_storage::{DatabaseConfig, PgTools, Storage};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Barrier;
const NOW: u64 = 1_800_000_000_000;
static INTERACTION: AtomicU64 = AtomicU64::new(1);
fn interaction(now: u64) -> String {
    (((now - 1_420_070_400_000) << 22) + INTERACTION.fetch_add(1, Ordering::Relaxed)).to_string()
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
        toons: BTreeMap::from([
            ("pebble".into(), "Pebble".into()),
            ("poppy".into(), "Poppy".into()),
            ("rudie".into(), "Rudie".into()),
        ]),
        observed_at: NOW - 1,
        fresh_until: NOW + 7 * DAY,
        disputed: false,
    }
}
#[derive(Clone)]
struct Db {
    store: Arc<Storage>,
    guild: GuildId,
    module: ModuleId,
    barrier: Option<Arc<Barrier>>,
    raced: String,
    first: Arc<AtomicBool>,
    lose_ack: Arc<AtomicBool>,
}
impl Db {
    fn new(store: Arc<Storage>) -> Self {
        Self {
            store,
            guild: "123".parse().unwrap(),
            module: "community.dandys-world".parse().unwrap(),
            barrier: None,
            raced: String::new(),
            first: Arc::new(AtomicBool::new(true)),
            lose_ack: Arc::new(AtomicBool::new(false)),
        }
    }
}
fn map_error(e: oracle_core::Error) -> storage::Error {
    if e.code == ErrorCode::Conflict {
        storage::Error::Conflict
    } else {
        panic!("unexpected database error: {e:?}")
    }
}
#[async_trait]
impl Documents for Db {
    fn guild(&self) -> &str {
        self.guild.as_str()
    }
    async fn get(&self, c: &str, k: &str) -> storage::Result<Option<ModuleDocument>> {
        let v = self
            .store
            .document_get(&self.module, &self.guild, c, k)
            .await
            .map_err(map_error)?;
        if matches!(c, "runs" | "run_index")
            && k == self.raced
            && self.first.swap(false, Ordering::SeqCst)
            && let Some(b) = &self.barrier
        {
            b.wait().await;
        }
        Ok(v)
    }
    async fn batch(&self, w: Vec<DocumentWrite>) -> storage::Result<()> {
        self.store
            .document_batch(&self.module, &self.guild, storage::DATA_VERSION, &w)
            .await
            .map_err(map_error)?;
        if self.lose_ack.swap(false, Ordering::SeqCst) {
            return Err(storage::Error::Unavailable);
        }
        Ok(())
    }
}
async fn initialize(config: DatabaseConfig) -> Arc<Storage> {
    let store = Arc::new(Storage::open(config).await.unwrap());
    let module: ModuleId = "community.dandys-world".parse().unwrap();
    let guild: GuildId = "123".parse().unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    // Version 1 was wiki-only and had no module documents. Upgrade namespace
    // independently of the immutable filesystem wiki catalog.
    let digest = "a".repeat(64);
    store
        .begin_migration(&module, &guild, 0, 1, &digest)
        .await
        .unwrap();
    store.migration_page(&module, &guild, 100).await.unwrap();
    store
        .commit_migration_page(&module, &guild, &digest, None, &[], None, true)
        .await
        .unwrap();
    store
        .begin_migration(&module, &guild, 1, storage::DATA_VERSION, &digest)
        .await
        .unwrap();
    let page = store.migration_page(&module, &guild, 100).await.unwrap();
    assert!(page.documents.is_empty());
    store
        .commit_migration_page(&module, &guild, &digest, None, &[], None, true)
        .await
        .unwrap();
    assert_eq!(
        store
            .migration_status(&module, &guild)
            .await
            .unwrap()
            .data_version,
        storage::DATA_VERSION
    );
    store
}
async fn create(service: &RunService<Db>, owner: &Actor, mode: RunMode) -> String {
    let id = interaction(NOW);
    let request = Request::Create {
        mode,
        name: Some("casual is a name".into()),
    };
    let result = service
        .execute(owner, &id, request.clone(), &catalog(), NOW)
        .await
        .unwrap();
    assert_eq!(
        service
            .execute(owner, &id, request, &catalog(), NOW)
            .await
            .unwrap(),
        result
    );
    match result {
        Response::Applied { result } => result.run_id,
        _ => panic!(),
    }
}
async fn change(
    service: &RunService<Db>,
    who: &Actor,
    id: &str,
    command: Command,
) -> storage::Result<Response> {
    service
        .execute(
            who,
            &interaction(NOW),
            Request::Change {
                run_id: id.into(),
                command,
                confirmation: None,
                expected_revision: None,
            },
            &catalog(),
            NOW,
        )
        .await
}
async fn confirmed(
    service: &RunService<Db>,
    who: &Actor,
    id: &str,
    command: Command,
) -> storage::Result<Response> {
    let prepared = service
        .execute(
            who,
            &interaction(NOW),
            Request::Prepare {
                run_id: id.into(),
                command: command.clone(),
            },
            &catalog(),
            NOW,
        )
        .await?;
    let Response::Confirm { confirmation, .. } = prepared else {
        panic!()
    };
    service
        .execute(
            who,
            &interaction(NOW),
            Request::Change {
                run_id: id.into(),
                command,
                confirmation: Some(confirmation),
                expected_revision: None,
            },
            &catalog(),
            NOW,
        )
        .await
}
fn dummy() -> Confirmation {
    Confirmation {
        actor_id: "forged".into(),
        revision: 999,
    }
}
async fn qualify(config: DatabaseConfig, restore: DatabaseConfig, root: &std::path::Path) {
    let store = initialize(config.clone()).await;
    let db = Db::new(store.clone());
    let service = RunService::new(db.clone());
    let owner = actor("100");
    let lost_id = interaction(NOW);
    let lost_request = Request::Create {
        mode: RunMode::Organized,
        name: Some("Restored after lost reply".into()),
    };
    db.lose_ack.store(true, Ordering::SeqCst);
    assert!(matches!(
        service
            .execute(&owner, &lost_id, lost_request.clone(), &catalog(), NOW)
            .await,
        Err(storage::Error::Unavailable)
    ));
    let recovered = service
        .execute(&owner, &lost_id, lost_request.clone(), &catalog(), NOW)
        .await
        .unwrap();
    let Response::Applied { result } = recovered.clone() else {
        panic!()
    };
    let id = result.run_id;
    change(
        &service,
        &owner,
        &id,
        Command::EditDraft {
            name: None,
            allocations: Some(BTreeMap::from([("pebble".into(), 1), ("poppy".into(), 1)])),
            host_toon: Some("pebble".into()),
        },
    )
    .await
    .unwrap();
    let before = service.view(&owner, &id).await.unwrap();
    assert!(matches!(
        change(
            &service,
            &actor("999"),
            &id,
            Command::Rename {
                name: "stolen".into()
            }
        )
        .await,
        Err(storage::Error::Rule(Error::Forbidden))
    ));
    let mut cross = owner.clone();
    cross.guild_id = "456".into();
    assert!(service.view(&cross, &id).await.is_err());
    assert_eq!(before, service.view(&owner, &id).await.unwrap());
    publish(&service, &owner, &id).await.unwrap();
    let published = service.view(&owner, &id).await.unwrap();
    assert_eq!(published.run.assignments.len(), 1);
    assert_eq!(published.run.mode, RunMode::Organized);
    assert_eq!(published.run.capacity(), 2);
    assert!(published.publication.is_some());
    // Deterministically synchronize real reads, not merely spawning tasks quickly.
    let barrier = Arc::new(Barrier::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    for user in 200..216 {
        let mut d = db.clone();
        d.barrier = Some(barrier.clone());
        d.raced = id.clone();
        d.first = Arc::new(AtomicBool::new(true));
        let id = id.clone();
        tasks.spawn(async move {
            (
                user,
                change(
                    &RunService::new(d),
                    &actor(&user.to_string()),
                    &id,
                    Command::Join {
                        toon: Some("poppy".into()),
                    },
                )
                .await,
            )
        });
    }
    let mut winners = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (user, result) = result.unwrap();
        match result {
            Ok(_) => winners.push(user),
            Err(storage::Error::Rule(Error::Full) | storage::Error::Busy) => {}
            Err(error) => panic!("unexpected race result {error:?}"),
        }
    }
    assert_eq!(winners.len(), 1);
    let winner = actor(&winners[0].to_string());
    let full = service.view(&owner, &id).await.unwrap();
    assert_eq!(full.run.assignments.len(), 2);
    // A failed switch must retain the original assignment and prior desired output.
    assert!(matches!(
        change(
            &service,
            &winner,
            &id,
            Command::Switch {
                toon: Some("pebble".into())
            }
        )
        .await,
        Err(storage::Error::Rule(Error::Full))
    ));
    assert_eq!(full, service.view(&owner, &id).await.unwrap());
    let rejected_id = interaction(NOW);
    let rejected = Request::Change {
        run_id: id.clone(),
        command: Command::Join {
            toon: Some("poppy".into()),
        },
        confirmation: None,
        expected_revision: None,
    };
    assert!(matches!(
        service
            .execute(
                &actor("999"),
                &rejected_id,
                rejected.clone(),
                &catalog(),
                NOW
            )
            .await,
        Err(storage::Error::Rule(Error::Full))
    ));
    let leave_id = interaction(NOW);
    let leave = Request::Change {
        run_id: id.clone(),
        command: Command::Leave,
        confirmation: None,
        expected_revision: None,
    };
    let left = service
        .execute(&winner, &leave_id, leave.clone(), &catalog(), NOW)
        .await
        .unwrap();
    assert_eq!(
        service
            .execute(&winner, &leave_id, leave, &catalog(), NOW)
            .await
            .unwrap(),
        left
    );
    // Replaying a previous Full result must not take the newly freed place.
    assert!(matches!(
        service
            .execute(&actor("999"), &rejected_id, rejected, &catalog(), NOW)
            .await,
        Err(storage::Error::Rule(Error::Full))
    ));
    assert_eq!(
        service
            .view(&owner, &id)
            .await
            .unwrap()
            .run
            .assignments
            .len(),
        1
    );
    // A stale catalog never rewrites published eligibility; leaving/rejoining uses its pin.
    let mut stale = catalog();
    stale.disputed = true;
    stale.toons.remove("poppy");
    service
        .execute(
            &winner,
            &interaction(NOW),
            Request::Change {
                run_id: id.clone(),
                command: Command::Join {
                    toon: Some("poppy".into()),
                },
                confirmation: None,
                expected_revision: None,
            },
            &stale,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        service.view(&owner, &id).await.unwrap().run.eligibility,
        catalog()
    );
    // Raw nested confirmation is never enough; only server-issued token authorizes it.
    assert!(matches!(
        change(
            &service,
            &owner,
            &id,
            Command::Cancel {
                confirmation: dummy()
            }
        )
        .await,
        Err(storage::Error::Rule(Error::ConfirmationRequired))
    ));
    let mut moderator = actor("555");
    moderator.manage_all_runs = true;
    confirmed(
        &service,
        &moderator,
        &id,
        Command::Remove {
            member_id: winner.user_id.clone(),
            confirmation: dummy(),
        },
    )
    .await
    .unwrap();
    change(&service, &owner, &id, Command::Lock).await.unwrap();
    assert!(
        change(
            &service,
            &winner,
            &id,
            Command::Join {
                toon: Some("poppy".into())
            }
        )
        .await
        .is_err()
    );
    change(&service, &owner, &id, Command::Reopen)
        .await
        .unwrap();
    let casual = create(&service, &owner, RunMode::Casual).await;
    publish(&service, &owner, &casual).await.unwrap();
    for user in 300..307 {
        change(
            &service,
            &actor(&user.to_string()),
            &casual,
            Command::Join {
                toon: Some("rudie".into()),
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        service
            .view(&owner, &casual)
            .await
            .unwrap()
            .run
            .assignments
            .len(),
        8
    );
    assert!(
        service
            .view(&owner, &casual)
            .await
            .unwrap()
            .run
            .allocations
            .is_none()
    );
    assert!(matches!(
        change(
            &service,
            &actor("400"),
            &casual,
            Command::Join { toon: None }
        )
        .await,
        Err(storage::Error::Rule(Error::Full))
    ));
    change(&service, &owner, &casual, Command::Lock)
        .await
        .unwrap();
    change(&service, &owner, &casual, Command::Complete)
        .await
        .unwrap();
    let completed = service.view(&owner, &casual).await.unwrap();
    assert_eq!(completed.run.state, RunState::Completed);
    assert_eq!(service.list(&winner, None).await.unwrap().len(), 1);
    // Draft expiry is based on owner edits, not reads; unresolved posted runs survive retention.
    let draft = create(&service, &actor("888"), RunMode::Casual).await;
    service.view(&actor("888"), &draft).await.unwrap();
    let cleaned = service.cleanup(NOW + DAY + 1, 32).await.unwrap();
    assert_eq!(cleaned.drafts, 1);
    assert!(service.view(&actor("888"), &draft).await.is_err());
    service.cleanup(NOW + 31 * DAY, 32).await.unwrap();
    assert_eq!(service.view(&owner, &casual).await.unwrap(), completed);
    // Native backup/restore, not a copy of fixture JSON, preserves every assignment/intent.
    let tools = std::env::var_os("ORACLE_TEST_PG_BIN")
        .map(|p| {
            let p = PathBuf::from(p);
            PgTools {
                pg_dump: p.join("pg_dump"),
                pg_restore: p.join("pg_restore"),
            }
        })
        .unwrap_or_default();
    let backup = root.join("backup");
    let active = service.view(&owner, &id).await.unwrap();
    assert_eq!(active.run.state, RunState::Open);
    store.backup(&backup, &tools).await.unwrap();
    // A backup is a point in time. Never roll it over newer committed signups.
    change(
        &service,
        &actor("777"),
        &id,
        Command::Join {
            toon: Some("poppy".into()),
        },
    )
    .await
    .unwrap();
    let newer = service.view(&owner, &id).await.unwrap();
    assert!(newer.run.assignments.contains_key("777"));
    assert!(
        Storage::restore(config.clone(), &backup, &tools)
            .await
            .is_err()
    );
    assert_eq!(service.view(&owner, &id).await.unwrap(), newer);
    let restored = Arc::new(Storage::restore(restore, &backup, &tools).await.unwrap());
    assert_eq!(
        RunService::new(Db::new(restored.clone()))
            .view(&owner, &id)
            .await
            .unwrap(),
        active
    );
    assert_eq!(
        RunService::new(Db::new(restored.clone()))
            .view(&owner, &casual)
            .await
            .unwrap(),
        completed
    );
    restored.close().await.unwrap();
    drop(restored);
    store.close().await.unwrap();
    drop(service);
    drop(db);
    drop(store);
    let reopened = Arc::new(Storage::open(config).await.unwrap());
    assert_eq!(
        RunService::new(Db::new(reopened.clone()))
            .view(&owner, &id)
            .await
            .unwrap(),
        newer
    );
    assert_eq!(
        RunService::new(Db::new(reopened.clone()))
            .view(&owner, &casual)
            .await
            .unwrap(),
        completed
    );
    reopened.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runs_are_atomic_durable_and_bounded() {
    let root = std::env::temp_dir().join(format!(
        "dw-runs-{}-{}",
        std::process::id(),
        interaction(NOW)
    ));
    std::fs::create_dir(&root).unwrap();
    qualify(
        DatabaseConfig::Sqlite {
            path: root.join("state.sqlite"),
        },
        DatabaseConfig::Sqlite {
            path: root.join("restored.sqlite"),
        },
        &root,
    )
    .await;
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires two fresh disposable PostgreSQL databases and native tools"]
async fn postgres_runs_are_atomic_durable_and_bounded() {
    let root = std::env::temp_dir().join(format!(
        "dw-runs-pg-{}-{}",
        std::process::id(),
        interaction(NOW)
    ));
    std::fs::create_dir(&root).unwrap();
    qualify(
        DatabaseConfig::Postgres {
            url: std::env::var("DW_TEST_POSTGRES_URL").expect("fresh isolated source database"),
        },
        DatabaseConfig::Postgres {
            url: std::env::var("DW_TEST_POSTGRES_RESTORE_URL")
                .expect("fresh isolated restore database"),
        },
        &root,
    )
    .await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn lowered_limits_preserve_reads_and_low_sequence_receipts_use_multiple_shards() {
    let root = std::env::temp_dir().join(format!("dw-run-limits-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let store = initialize(DatabaseConfig::Sqlite {
        path: root.join("state.sqlite"),
    })
    .await;
    let db = Db::new(store.clone());
    let service = RunService::new(db.clone());
    let owner = actor("100");
    let id = create(&service, &owner, RunMode::Casual).await;
    let limits = storage::Limits {
        drafts_per_guild: 0,
        ..Default::default()
    };
    let limited = RunService::with_limits(db.clone(), limits).unwrap();
    assert_eq!(
        limited.view(&owner, &id).await.unwrap(),
        service.view(&owner, &id).await.unwrap()
    );
    assert!(matches!(
        limited
            .execute(
                &actor("200"),
                &interaction(NOW),
                Request::Create {
                    mode: RunMode::Casual,
                    name: None
                },
                &catalog(),
                NOW
            )
            .await,
        Err(storage::Error::Limit)
    ));
    // More than one shard's capacity of legitimate snowflakes with sequence zero.
    // This catches selecting a shard from low snowflake bits alone.
    for tick in 1..=270 {
        let now = NOW;
        let iid = ((NOW - 1000 + tick - 1_420_070_400_000) << 22).to_string();
        service
            .execute(
                &owner,
                &iid,
                Request::Change {
                    run_id: id.clone(),
                    command: Command::Rename {
                        name: format!("Run {tick}"),
                    },
                    confirmation: None,
                    expected_revision: None,
                },
                &catalog(),
                now,
            )
            .await
            .unwrap();
    }
    let before = service.view(&owner, &id).await.unwrap();
    let saturated = RunService::with_limits(
        db.clone(),
        storage::Limits {
            receipts: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        change(
            &saturated,
            &owner,
            &id,
            Command::Rename {
                name: "not committed".into()
            }
        )
        .await,
        Err(storage::Error::Limit)
    ));
    assert_eq!(service.view(&owner, &id).await.unwrap(), before);
    let mut mod_actor = actor("500");
    mod_actor.manage_all_runs = true;
    for tick in 0..127 {
        change(
            &service,
            &mod_actor,
            &id,
            Command::Rename {
                name: format!("Moderator edit {tick}"),
            },
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        change(
            &service,
            &mod_actor,
            &id,
            Command::Rename {
                name: "Audit full".into()
            }
        )
        .await,
        Err(storage::Error::Limit)
    ));
    confirmed(
        &service,
        &mod_actor,
        &id,
        Command::Cancel {
            confirmation: dummy(),
        },
    )
    .await
    .unwrap();
    let cancelled = service.view(&owner, &id).await.unwrap();
    assert_eq!(cancelled.moderator_audit.len(), 128);
    assert_eq!(cancelled.moderator_audit[0].actor_id, "500");
    assert_eq!(cancelled.moderator_audit[127].action, "cancel");
    assert!(matches!(
        service
            .execute(
                &owner,
                &interaction(NOW),
                Request::Create {
                    mode: RunMode::Casual,
                    name: None
                },
                &catalog(),
                NOW + DAY
            )
            .await,
        Err(storage::Error::Interaction)
    ));
    // Receipt removal is bounded per sweep; it never changes the retained audit.
    assert_eq!(
        service.cleanup(NOW + 2 * DAY, 32).await.unwrap().receipts,
        32
    );
    assert_eq!(service.view(&owner, &id).await.unwrap(), cancelled);
    store.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn configured_limits_cannot_increase_storage_ceilings() {
    let mut limits = storage::Limits::default();
    limits.receipts += 1;
    assert!(limits.validate().is_err());
    assert!(serde_json::from_value::<storage::Limits>(serde_json::json!({"players":9})).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_and_publication_limits_are_checked_in_the_atomic_batch() {
    let root = std::env::temp_dir().join(format!("dw-admission-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let store = initialize(DatabaseConfig::Sqlite {
        path: root.join("state.sqlite"),
    })
    .await;
    let db = Db::new(store.clone());
    let barrier = Arc::new(Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for user in 700..708 {
        let mut d = db.clone();
        d.barrier = Some(barrier.clone());
        d.raced = "live".into();
        d.first = Arc::new(AtomicBool::new(true));
        tasks.spawn(async move {
            let service = RunService::with_limits(
                d,
                storage::Limits {
                    drafts_per_guild: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            (
                actor(&user.to_string()),
                service
                    .execute(
                        &actor(&user.to_string()),
                        &interaction(NOW),
                        Request::Create {
                            mode: RunMode::Casual,
                            name: None,
                        },
                        &catalog(),
                        NOW,
                    )
                    .await,
            )
        });
    }
    let mut winners = Vec::new();
    while let Some(task) = tasks.join_next().await {
        let (owner, result) = task.unwrap();
        match result {
            Ok(Response::Applied { result }) => winners.push((owner, result.run_id)),
            Err(storage::Error::Limit | storage::Error::Busy) => (),
            other => panic!("unexpected create race {other:?}"),
        }
    }
    assert_eq!(winners.len(), 1);
    let service = RunService::new(db.clone());
    let other = actor("900");
    let second = create(&service, &other, RunMode::Casual).await;
    winners.push((other, second));
    let barrier = Arc::new(Barrier::new(2));
    let mut tasks = tokio::task::JoinSet::new();
    for (owner, id) in winners.clone() {
        let mut d = db.clone();
        d.barrier = Some(barrier.clone());
        d.raced = "live".into();
        d.first = Arc::new(AtomicBool::new(true));
        tasks.spawn(async move {
            let s = RunService::with_limits(
                d,
                storage::Limits {
                    published_per_guild: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            publish(&s, &owner, &id).await
        });
    }
    let mut count = 0;
    while let Some(task) = tasks.join_next().await {
        match task.unwrap() {
            Ok(_) => count += 1,
            Err(storage::Error::Limit | storage::Error::Busy) => (),
            other => panic!("unexpected publish race {other:?}"),
        }
    }
    assert_eq!(count, 1);
    let mut published = 0;
    for (owner, id) in winners {
        let stored = service.view(&owner, &id).await.unwrap();
        if stored.run.state == RunState::Open {
            published += 1;
            assert_eq!(stored.run.assignments.len(), 1);
            assert!(stored.publication.is_some());
        } else {
            assert_eq!(stored.run.state, RunState::Draft);
            assert!(stored.run.assignments.is_empty());
            assert!(stored.publication.is_none());
        }
    }
    assert_eq!(published, 1);
    store.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn serialized_capacity_reserves_space_for_moderator_cancellation() {
    let root = std::env::temp_dir().join(format!("dw-size-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let store = initialize(DatabaseConfig::Sqlite {
        path: root.join("state.sqlite"),
    })
    .await;
    let service = RunService::new(Db::new(store.clone()));
    let owner = actor("100");
    let mut large = catalog();
    large.toons = (0..80)
        .map(|i| (format!("toon{i}"), format!("{i:02}{}", "🟢".repeat(88))))
        .collect();
    let Response::Applied { result } = service
        .execute(
            &owner,
            &interaction(NOW),
            Request::Create {
                mode: RunMode::Casual,
                name: None,
            },
            &large,
            NOW,
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let id = result.run_id;
    let mut moderator = actor("18446744073709551615");
    moderator.manage_all_runs = true;
    let mut edits = 0;
    for tick in 0..127 {
        let result = service
            .execute(
                &moderator,
                &interaction(NOW),
                Request::Change {
                    run_id: id.clone(),
                    command: Command::Rename {
                        name: format!("Edit {tick}"),
                    },
                    confirmation: None,
                    expected_revision: None,
                },
                &large,
                NOW,
            )
            .await;
        match result {
            Ok(_) => edits += 1,
            Err(storage::Error::Limit) => break,
            other => panic!("unexpected size result {other:?}"),
        }
    }
    assert!(
        edits > 0 && edits < 127,
        "must exercise serialized size, not entry count"
    );
    let live = service.view(&owner, &id).await.unwrap();
    let live_bytes = serde_json::to_vec(&live).unwrap().len();
    assert!(live_bytes <= 40 * 1024 - 512);
    confirmed(
        &service,
        &moderator,
        &id,
        Command::Cancel {
            confirmation: dummy(),
        },
    )
    .await
    .unwrap();
    let terminal = service.view(&owner, &id).await.unwrap();
    assert_eq!(terminal.run.state, RunState::Cancelled);
    assert_eq!(terminal.moderator_audit.len(), edits + 1);
    let bytes = serde_json::to_vec(&terminal).unwrap().len();
    assert!(bytes > live_bytes && bytes <= 40 * 1024);
    store.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn wizard_rows_resume_revision_projection_and_explicit_repost_are_atomic() {
    let root = std::env::temp_dir().join(format!("dw-wizard-{}", interaction(NOW)));
    std::fs::create_dir(&root).unwrap();
    let database = initialize(DatabaseConfig::Sqlite {
        path: root.join("state.sqlite"),
    })
    .await;
    let service = RunService::new(Db::new(database.clone()));
    let owner = actor("7");
    let id = create(&service, &owner, RunMode::Organized).await;
    change(
        &service,
        &owner,
        &id,
        Command::SetAllocation {
            toon: "pebble".into(),
            count: 2,
        },
    )
    .await
    .unwrap();
    change(
        &service,
        &owner,
        &id,
        Command::SetAllocation {
            toon: "poppy".into(),
            count: 3,
        },
    )
    .await
    .unwrap();
    let stored = service.view(&owner, &id).await.unwrap();
    assert_eq!(stored.run.allocations.as_ref().unwrap().len(), 2);
    assert_eq!(stored.run.capacity(), 5);
    let stale = service
        .execute(
            &owner,
            &interaction(NOW),
            Request::Change {
                run_id: id.clone(),
                command: Command::SetAllocation {
                    toon: "pebble".into(),
                    count: 1,
                },
                confirmation: None,
                expected_revision: Some(1),
            },
            &catalog(),
            NOW,
        )
        .await;
    assert!(matches!(
        stale,
        Err(storage::Error::Rule(Error::StaleRevision))
    ));
    let mut unavailable = catalog();
    unavailable.disputed = true;
    let resumed = service
        .execute(
            &owner,
            &interaction(NOW),
            Request::Create {
                mode: RunMode::Casual,
                name: Some("different".into()),
            },
            &unavailable,
            NOW,
        )
        .await
        .unwrap();
    assert!(
        matches!(resumed,Response::Applied{result} if result.run_id==id && !result.outcome.changed)
    );
    assert_eq!(service.view(&owner, &id).await.unwrap(), stored);
    change(
        &service,
        &owner,
        &id,
        Command::SetHostToon {
            toon: "pebble".into(),
        },
    )
    .await
    .unwrap();
    publish(&service, &owner, &id).await.unwrap();
    let published = service.view(&owner, &id).await.unwrap();
    let projection = published.publication.as_ref().unwrap();
    assert_eq!(projection.repost_generation, 0);
    let (card, actions) = dandys_world_core::runs::ui::public_projection(&published.run);
    assert_eq!(projection.card, card);
    assert_eq!(projection.actions, actions);
    assert_eq!(published.run.assignments.len(), 1);
    assert!(
        !service
            .release_confirmed(&id, projection.desired_revision - 1)
            .await
            .unwrap()
    );
    assert!(
        service
            .release_confirmed(&id, projection.desired_revision)
            .await
            .unwrap()
    );
    assert!(service.pending_projections().await.unwrap().is_empty());
    let request = Request::Repost {
        run_id: id.clone(),
        expected_revision: published.run.desired_card_revision,
    };
    let click = interaction(NOW);
    let response = service
        .execute(&owner, &click, request.clone(), &unavailable, NOW)
        .await
        .unwrap();
    assert_eq!(
        service
            .execute(&owner, &click, request, &unavailable, NOW)
            .await
            .unwrap(),
        response
    );
    let reposted = service.view(&owner, &id).await.unwrap();
    assert_eq!(reposted.publication.as_ref().unwrap().repost_generation, 1);
    assert_eq!(
        reposted.run.desired_card_revision,
        published.run.desired_card_revision + 1
    );
    assert!(
        !service
            .release_confirmed(&id, projection.desired_revision)
            .await
            .unwrap()
    );
    assert_eq!(
        service.pending_projections().await.unwrap(),
        vec![id.clone()]
    );
    let denied = service
        .execute(
            &actor("8"),
            &interaction(NOW),
            Request::Repost {
                run_id: id.clone(),
                expected_revision: reposted.run.desired_card_revision,
            },
            &unavailable,
            NOW,
        )
        .await;
    assert!(matches!(
        denied,
        Err(storage::Error::Rule(Error::Forbidden))
    ));
    database.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn v2_projection_migration_retains_full_source_audit_and_receipts() {
    let owner = actor("7");
    let mut source = catalog();
    source.toons = (0..80)
        .map(|i| (format!("toon{i:02}"), format!("{i:02}{}", "🌟".repeat(76))))
        .collect();
    let run = Run::new(
        "abcdefgh".into(),
        &owner,
        RunMode::Casual,
        None,
        source,
        NOW,
    )
    .unwrap();
    let mut run = run;
    run.schedule = Some(dandys_world_core::runs::domain::RunSchedule {
        starts_at: 4_070_908_800,
        duration_minutes: 90,
        timezone: Some("UTC".into()),
    });
    let (run, _) = apply(&run, &owner, &Command::Publish, &run.eligibility, NOW).unwrap();
    let audit: Vec<_> = (0..80)
        .map(|_| storage::AuditEntry {
            actor_id: "8".into(),
            action: "rename".into(),
            at: NOW,
            affected_member: None,
        })
        .collect();
    let old = serde_json::json!({"schema_version":2,"run":run,"moderator_audit":audit,"publication":{"key":run.id,"desired_revision":run.desired_card_revision,"destination":"runs","created_at":NOW}});
    let old_bytes = serde_json::to_vec(&old).unwrap().len();
    assert!(
        old_bytes > 28 * 1024 && old_bytes < 32 * 1024,
        "{old_bytes}"
    );
    let write = storage::migrate_v2_document(ModuleDocument {
        collection: "runs".into(),
        key: run.id.clone(),
        revision: 9,
        value: old.clone(),
    })
    .unwrap();
    let new = write.value.unwrap();
    assert_eq!(write.expected_revision, Some(9));
    assert_eq!(new["run"], old["run"]);
    assert_eq!(new["moderator_audit"], old["moderator_audit"]);
    assert_eq!(new["publication"]["repost_generation"], 0);
    assert!(new["publication"]["card"].is_object());
    assert!(serde_json::to_vec(&new).unwrap().len() < 40 * 1024);
    let receipt = serde_json::json!({"opaque":"unchanged receipt hash and result"});
    let copied = storage::migrate_v2_document(ModuleDocument {
        collection: "run_receipts".into(),
        key: "123".into(),
        revision: 4,
        value: receipt.clone(),
    })
    .unwrap();
    assert_eq!(copied.value, Some(receipt));
}

async fn publish(service: &RunService<Db>, owner: &Actor, id: &str) -> storage::Result<Response> {
    change(
        service,
        owner,
        id,
        Command::SetSchedule {
            schedule: dandys_world_core::runs::domain::RunSchedule {
                starts_at: 4_070_908_800,
                duration_minutes: 90,
                timezone: Some("UTC".into()),
            },
        },
    )
    .await?;
    change(service, owner, id, Command::Publish).await
}
