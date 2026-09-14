use oracle_core::{ErrorCode, GuildId, ModuleId};
use oracle_operations::shared_cards::*;
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use std::sync::Arc;

fn intent(revision: u64) -> SharedIntent {
    serde_json::from_value(json!({"key":"abcd1234","desired_revision":revision,"destination":"runs","created_at":1,"card":{"title":"Run","description":"Any Toon","fields":[],"footer":""},"actions":[]})).unwrap()
}
fn effect(revision: u64, message: Option<&str>) -> SharedEffect {
    SharedEffect {
        effect_id: format!("effect-{revision}"),
        guild: GuildId::new("123").unwrap(),
        module: ModuleId::new("test.runs").unwrap(),
        run_id: "abcd1234".into(),
        target: SharedTarget {
            channel_id: "456".into(),
            application_id: "789".into(),
            bot_id: "789".into(),
        },
        message_id: message.map(str::to_owned),
        desired_revision: revision,
        marker: format!("marker-{revision}"),
        payload: json!({"content":format!("Revision {revision}"),"allowed_mentions":{"parse":[]}}),
    }
}
#[tokio::test]
async fn restart_preserves_unknown_and_coalesces_newer_intent() {
    let temp = tempfile::tempdir().unwrap();
    let config = DatabaseConfig::Sqlite {
        path: temp.path().join("shared.sqlite"),
    };
    let store = Storage::open(config.clone()).await.unwrap();
    let guild = GuildId::new("123").unwrap();
    let module = ModuleId::new("test.runs").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let journal = SharedCardJournal::new(Arc::new(store.clone()));
    journal
        .enqueue(&guild, &module, "abcd1234", intent(1))
        .await
        .unwrap();
    let create = effect(1, None);
    journal.prepare(create.clone()).await.unwrap();
    journal
        .enqueue(&guild, &module, "abcd1234", intent(2))
        .await
        .unwrap();
    assert_eq!(
        journal.prepare(effect(2, None)).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    drop(journal);
    store.close().await.unwrap();
    drop(store);
    let store = Storage::open(config).await.unwrap();
    let journal = SharedCardJournal::new(Arc::new(store.clone()));
    let unknown = journal
        .settle(&create, SharedObservation::Unknown)
        .await
        .unwrap();
    assert_eq!(unknown.phase, SharedPhase::Unknown);
    assert_eq!(unknown.desired.desired_revision, 2);
    assert!(unknown.identity.is_none());
    assert_eq!(
        journal
            .repost(&guild, &module, "abcd1234")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let confirmed = journal
        .settle(
            &create,
            SharedObservation::Confirmed {
                message_id: "1000".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(confirmed.phase, SharedPhase::Pending);
    assert_eq!(confirmed.confirmed_revision, Some(1));
    let edit = effect(2, Some("1000"));
    journal.prepare(edit.clone()).await.unwrap();
    journal
        .enqueue(&guild, &module, "abcd1234", intent(3))
        .await
        .unwrap();
    let confirmed = journal
        .settle(
            &edit,
            SharedObservation::Confirmed {
                message_id: "1000".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(confirmed.phase, SharedPhase::Pending);
    assert_eq!(confirmed.identity.unwrap().control_version, "effect-1");
    assert_eq!(
        journal
            .prepare(effect(2, Some("1000")))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let edit = effect(3, Some("1000"));
    journal.prepare(edit.clone()).await.unwrap();
    let missing = journal
        .settle(&edit, SharedObservation::Missing)
        .await
        .unwrap();
    assert_eq!(missing.phase, SharedPhase::Missing);
    journal
        .enqueue(&guild, &module, "abcd1234", intent(4))
        .await
        .unwrap();
    assert_eq!(
        journal.prepare(effect(4, None)).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    journal.repost(&guild, &module, "abcd1234").await.unwrap();
    let repost = effect(4, None);
    journal.prepare(repost.clone()).await.unwrap();
    let restored = journal
        .settle(
            &repost,
            SharedObservation::Confirmed {
                message_id: "2000".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.phase, SharedPhase::Confirmed);
    assert_eq!(restored.identity.unwrap().control_version, "effect-4");
    assert_eq!(
        journal
            .settle(
                &create,
                SharedObservation::Confirmed {
                    message_id: "1000".into()
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    drop(journal);
    store.close().await.unwrap();
}

#[tokio::test]
async fn only_one_concurrent_effect_can_be_prepared() {
    let temp = tempfile::tempdir().unwrap();
    let store = Storage::open(DatabaseConfig::Sqlite {
        path: temp.path().join("race.sqlite"),
    })
    .await
    .unwrap();
    let guild = GuildId::new("123").unwrap();
    let module = ModuleId::new("test.runs").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let journal = Arc::new(SharedCardJournal::new(Arc::new(store.clone())));
    journal
        .enqueue(&guild, &module, "abcd1234", intent(1))
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let journal = journal.clone();
        tasks.spawn(async move { journal.prepare(effect(1, None)).await });
    }
    let mut wins = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(()) => wins += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(wins, 1);
    let mut same_revision = intent(1);
    same_revision.destination = "other".into();
    assert_eq!(
        journal
            .enqueue(&guild, &module, "abcd1234", same_revision)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let other = GuildId::new("999").unwrap();
    assert!(
        journal
            .get(&other, &module, "abcd1234")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        journal
            .settle(&effect(1, None), SharedObservation::Missing)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Integrity
    );
    drop(journal);
    store.close().await.unwrap();
}
