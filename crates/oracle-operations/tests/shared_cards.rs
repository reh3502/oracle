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
    let mut forged = create.clone();
    forged.desired_revision = 99;
    assert_eq!(
        journal
            .settle(
                &forged,
                SharedObservation::Confirmed {
                    message_id: "1000".into()
                }
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
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

#[tokio::test]
async fn concurrent_reservations_cannot_exceed_card_limit() {
    use oracle_core::{WorkflowKind, WorkflowRepository};
    let temp = tempfile::tempdir().unwrap();
    let store = Storage::open(DatabaseConfig::Sqlite {
        path: temp.path().join("quota.sqlite"),
    })
    .await
    .unwrap();
    let guild = GuildId::new("123").unwrap();
    let module = ModuleId::new("test.runs").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let runs: Vec<_> = (0..699).map(|n| format!("run{n:05}")).collect();
    store
        .workflow_put(
            &guild,
            WorkflowKind::SharedCard,
            &format!("index:{}", SharedCardJournal::key(&module, "")),
            None,
            &json!(runs),
        )
        .await
        .unwrap();
    let journal = Arc::new(SharedCardJournal::new(Arc::new(store.clone())));
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..8 {
        let journal = journal.clone();
        let guild = guild.clone();
        let module = module.clone();
        tasks.spawn(async move {
            let key = format!("new{n:05}");
            let mut value = intent(1);
            value.key = key.clone();
            journal.enqueue(&guild, &module, &key, value).await
        });
    }
    let mut wins = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => wins += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::QuotaExceeded),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(journal.run_ids(&guild, &module).await.unwrap().len(), 700);
    drop(journal);
    store.close().await.unwrap();
}

#[tokio::test]
async fn reusable_controls_are_bound_to_message_scope_and_repost() {
    let temp = tempfile::tempdir().unwrap();
    let store = Storage::open(DatabaseConfig::Sqlite {
        path: temp.path().join("controls.sqlite"),
    })
    .await
    .unwrap();
    let guild = GuildId::new("123").unwrap();
    let module = ModuleId::new("test.runs").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let journal = SharedCardJournal::new(Arc::new(store.clone()));
    let mut desired = intent(1);
    desired.actions=vec![serde_json::from_value(json!({"name":"join","label":"Join","operation":"run_ui","input":{"action":"join","id":"abcd1234"}})).unwrap()];
    let record = journal
        .enqueue(&guild, &module, "abcd1234", desired.clone())
        .await
        .unwrap();
    let control = record.control_id(&guild, "effect-1", 0).unwrap();
    assert!(control.len() <= 100);
    let create = effect(1, None);
    journal.prepare(create.clone()).await.unwrap();
    let record = journal
        .settle(
            &create,
            SharedObservation::Confirmed {
                message_id: "1000".into(),
            },
        )
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            record
                .verify_control(&guild, "456", "1000", "789", "789", &control)
                .unwrap()
                .name,
            "join"
        );
    }
    for (channel, message, app, author) in [
        ("457", "1000", "789", "789"),
        ("456", "1001", "789", "789"),
        ("456", "1000", "790", "789"),
        ("456", "1000", "789", "790"),
    ] {
        assert!(
            record
                .verify_control(&guild, channel, message, app, author, &control)
                .is_err()
        );
    }
    assert!(
        record
            .verify_control(
                &GuildId::new("124").unwrap(),
                "456",
                "1000",
                "789",
                "789",
                &control
            )
            .is_err()
    );
    let forged = format!(
        "{}{}",
        &control[..control.len() - 1],
        if control.ends_with('0') { '1' } else { '0' }
    );
    assert!(
        record
            .verify_control(&guild, "456", "1000", "789", "789", &forged)
            .is_err()
    );
    desired.desired_revision = 2;
    journal
        .enqueue(&guild, &module, "abcd1234", desired.clone())
        .await
        .unwrap();
    let edit = effect(2, Some("1000"));
    journal.prepare(edit.clone()).await.unwrap();
    journal
        .settle(&edit, SharedObservation::Missing)
        .await
        .unwrap();
    desired.desired_revision = 3;
    desired.repost_generation = 1;
    let record = journal
        .enqueue(&guild, &module, "abcd1234", desired)
        .await
        .unwrap();
    let replacement = record.control_id(&guild, "effect-3", 0).unwrap();
    assert_ne!(replacement, control);
    let repost = effect(3, None);
    journal.prepare(repost.clone()).await.unwrap();
    let record = journal
        .settle(
            &repost,
            SharedObservation::Confirmed {
                message_id: "2000".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        record
            .verify_control(&guild, "456", "2000", "789", "789", &control)
            .is_err()
    );
    assert!(
        record
            .verify_control(&guild, "456", "2000", "789", "789", &replacement)
            .is_ok()
    );
    assert!(journal.resolve_control(&guild, &replacement).await.is_ok());
    drop(journal);
    store.close().await.unwrap();
}

#[tokio::test]
async fn proven_unsent_effect_can_retry_without_losing_newer_intent() {
    let temp = tempfile::tempdir().unwrap();
    let store = Storage::open(DatabaseConfig::Sqlite {
        path: temp.path().join("unsent.sqlite"),
    })
    .await
    .unwrap();
    let guild = GuildId::new("123").unwrap();
    let module = ModuleId::new("test.runs").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let journal = SharedCardJournal::new(Arc::new(store));
    journal
        .enqueue(&guild, &module, "abcd1234", intent(1))
        .await
        .unwrap();
    let send = effect(1, None);
    journal.prepare(send.clone()).await.unwrap();
    journal
        .enqueue(&guild, &module, "abcd1234", intent(2))
        .await
        .unwrap();
    let record = journal
        .settle(
            &send,
            SharedObservation::NotSent {
                reason: ErrorCode::Cancelled,
            },
        )
        .await
        .unwrap();
    assert_eq!(record.phase, SharedPhase::Pending);
    assert_eq!(record.desired.desired_revision, 2);
    assert!(record.effect.is_none());
    assert!(record.identity.is_none());
    journal.prepare(effect(2, None)).await.unwrap();
}
