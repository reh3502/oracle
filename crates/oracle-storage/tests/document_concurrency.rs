//! Real repository qualification for bounded aggregates, indexes and receipts.
//! No Discord or game-specific implementation is involved.
use oracle_core::{DocumentWrite, ErrorCode, GuildId, ModuleId, ModuleRepository};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Barrier;

fn write(key: &str, revision: Option<u64>, value: Value) -> DocumentWrite {
    DocumentWrite {
        collection: "aggregates".into(),
        key: key.into(),
        expected_revision: revision,
        value: Some(value),
    }
}

async fn qualify(config: DatabaseConfig) {
    let store = Storage::open(config.clone()).await.unwrap();
    let module = ModuleId::new("fixture.aggregates").unwrap();
    let guild = GuildId::new("123").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
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

    // Create-only collision must roll back an index write earlier in the batch.
    store
        .document_batch(
            &module,
            &guild,
            1,
            &[
                write("index", None, json!({"ids":["one"]})),
                write("one", None, json!({"capacity":1,"members":[],"desired":1})),
            ],
        )
        .await
        .unwrap();
    let collision = store
        .document_batch(
            &module,
            &guild,
            1,
            &[
                write("index", Some(1), json!({"ids":["one","collision"]})),
                write("one", None, json!({"overwritten":true})),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(collision.code, ErrorCode::Conflict);
    assert_eq!(
        store
            .document_get(&module, &guild, "aggregates", "index")
            .await
            .unwrap()
            .unwrap()
            .value,
        json!({"ids":["one"]})
    );

    // Every contender reads the same final empty place before any may write.
    // The receipt comes first: losers must not leave a successful receipt behind.
    let barrier = Arc::new(Barrier::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    for actor in 1..=16 {
        let (store, module, guild, barrier) = (
            store.clone(),
            module.clone(),
            guild.clone(),
            barrier.clone(),
        );
        tasks.spawn(async move {
            let before = store
                .document_get(&module, &guild, "aggregates", "one")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(before.value["members"], json!([]));
            barrier.wait().await;
            (
                actor,
                store
                    .document_batch(
                        &module,
                        &guild,
                        1,
                        &[
                            write(
                                &format!("receipt-{actor}"),
                                None,
                                json!({"actor":actor,"outcome":"joined"}),
                            ),
                            write(
                                "one",
                                Some(before.revision),
                                json!({"capacity":1,"members":[actor],"desired":2}),
                            ),
                        ],
                    )
                    .await,
            )
        });
    }
    let mut winners = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (actor, result) = result.unwrap();
        match result {
            Ok(_) => winners.push(actor),
            Err(error) => assert_eq!(error.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(winners.len(), 1);
    for actor in 1..=16 {
        assert_eq!(
            store
                .document_get(&module, &guild, "aggregates", &format!("receipt-{actor}"))
                .await
                .unwrap()
                .is_some(),
            winners.contains(&actor)
        );
    }
    let aggregate = store
        .document_get(&module, &guild, "aggregates", "one")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aggregate.revision, 2);
    assert_eq!(aggregate.value["members"], json!(winners));

    // Duplicate delivery cannot create a second receipt or change the aggregate.
    let replay = store
        .document_batch(
            &module,
            &guild,
            1,
            &[
                write("one", Some(2), json!({"members":[]})),
                write(
                    &format!("receipt-{}", winners[0]),
                    None,
                    json!({"outcome":"replayed"}),
                ),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(replay.code, ErrorCode::Conflict);
    assert_eq!(
        store
            .document_get(&module, &guild, "aggregates", "one")
            .await
            .unwrap()
            .unwrap()
            .value,
        aggregate.value
    );

    // Another scope cannot discover the roster or its idempotency record.
    assert!(
        store
            .document_get(&module, &GuildId::new("456").unwrap(), "aggregates", "one")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .document_get(
                &ModuleId::new("fixture.other").unwrap(),
                &guild,
                "aggregates",
                "one"
            )
            .await
            .unwrap()
            .is_none()
    );

    // Two independently generated IDs compete for the last index place. Both
    // create their aggregate first; the losing index CAS must remove that create.
    let barrier = Arc::new(Barrier::new(2));
    for key in ["two", "three"] {
        let (store, module, guild, barrier) = (
            store.clone(),
            module.clone(),
            guild.clone(),
            barrier.clone(),
        );
        tasks.spawn(async move {
            let index = store
                .document_get(&module, &guild, "aggregates", "index")
                .await
                .unwrap()
                .unwrap();
            barrier.wait().await;
            (
                0,
                store
                    .document_batch(
                        &module,
                        &guild,
                        1,
                        &[
                            write(key, None, json!({"members":[]})),
                            write("index", Some(index.revision), json!({"ids":["one",key]})),
                        ],
                    )
                    .await,
            )
        });
    }
    let mut index_wins = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap().1 {
            Ok(_) => index_wins += 1,
            Err(error) => assert_eq!(error.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(index_wins, 1);
    let index = store
        .document_get(&module, &guild, "aggregates", "index")
        .await
        .unwrap()
        .unwrap();
    for key in ["two", "three"] {
        assert_eq!(
            store
                .document_get(&module, &guild, "aggregates", key)
                .await
                .unwrap()
                .is_some(),
            index.value["ids"].as_array().unwrap().contains(&json!(key))
        );
    }

    // A desired projection and its pending intent survive an actual connection
    // close/reopen. A stale worker cannot overwrite the newer projection intent.
    store
        .document_batch(
            &module,
            &guild,
            1,
            &[
                write(
                    "one",
                    Some(2),
                    json!({"capacity":1,"members":winners,"desired":3}),
                ),
                write(
                    "projection",
                    None,
                    json!({"desired":3,"state":"pending","message":null}),
                ),
            ],
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);
    let store = Storage::open(config).await.unwrap();
    let intent = store
        .document_get(&module, &guild, "aggregates", "projection")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.value["state"], "pending");
    let latest = store
        .document_get(&module, &guild, "aggregates", "one")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.value["desired"], 3);
    store
        .document_batch(
            &module,
            &guild,
            1,
            &[write(
                "projection",
                Some(intent.revision),
                json!({"desired":3,"state":"confirmed","message":"900"}),
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .document_batch(
                &module,
                &guild,
                1,
                &[write(
                    "projection",
                    Some(intent.revision),
                    json!({"desired":2,"state":"confirmed","message":"899"})
                ),]
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        store
            .document_get(&module, &guild, "aggregates", "projection")
            .await
            .unwrap()
            .unwrap()
            .value["message"],
        "900"
    );
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_aggregate_concurrency() {
    let directory = std::env::temp_dir().join(format!("oracle-aggregate-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    qualify(DatabaseConfig::Sqlite {
        path: directory.join("test.sqlite"),
    })
    .await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a new disposable PostgreSQL database"]
async fn postgres_aggregate_concurrency() {
    let url = std::env::var("ORACLE_TEST_AGGREGATE_POSTGRES_URL")
        .expect("set a fresh disposable database URL");
    qualify(DatabaseConfig::Postgres { url }).await;
}
