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
        if c == "runs"
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
            .document_batch(&self.module, &self.guild, 2, &w)
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
        .begin_migration(&module, &guild, 1, 2, &digest)
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
        2
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
    change(&service, &owner, &id, Command::Publish)
        .await
        .unwrap();
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
    change(&service, &owner, &casual, Command::Publish)
        .await
        .unwrap();
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
    store.backup(&backup, &tools).await.unwrap();
    let restored = Arc::new(Storage::restore(restore, &backup, &tools).await.unwrap());
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
    assert_eq!(cancelled.moderator_audit.len(), 1);
    assert_eq!(cancelled.moderator_audit[0].actor_id, "500");
    assert_eq!(cancelled.moderator_audit[0].action, "cancel");
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
