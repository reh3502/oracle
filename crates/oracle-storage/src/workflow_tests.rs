use super::*;
use oracle_core::WorkflowRepository;
use serde_json::json;

pub(super) async fn exercise(config: &DatabaseConfig, store: Storage) -> Storage {
    let g = GuildId::new("123").unwrap();
    let other = GuildId::new("456").unwrap();
    let kind = WorkflowKind::StructurePlan;
    assert!(
        store
            .workflow_get(&g, kind, "plan")
            .await
            .unwrap()
            .is_none()
    );
    let saved = store.workflow_put(&g, kind, "plan", None, &json!({"deployment":store.status(None).await.unwrap().deployment,"steps":["minecraft"]})).await.unwrap();
    assert_eq!(saved.revision, 1);
    assert!(
        store
            .workflow_get(&other, kind, "plan")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .workflow_get(&g, WorkflowKind::Configuration, "plan")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .workflow_put(&g, kind, "plan", None, &json!(null))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..8 {
        let store = store.clone();
        let guild = g.clone();
        tasks.spawn(async move {
            store
                .workflow_put(&guild, WorkflowKind::Configuration, "cas", None, &json!(i))
                .await
        });
    }
    let mut wins = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => wins += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(
        store
            .workflow_put(&g, WorkflowKind::Configuration, "cas", Some(1), &json!(9))
            .await
            .unwrap()
            .revision,
        2
    );
    assert_eq!(
        store
            .workflow_put(&g, WorkflowKind::Configuration, "cas", Some(1), &json!(10))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    for key in ["", "bad\nkey", &"x".repeat(129)] {
        assert_eq!(
            store
                .workflow_put(&g, kind, key, None, &json!(0))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidInput
        );
    }
    assert!(
        store
            .workflow_put(&g, kind, "zero", Some(0), &json!(0))
            .await
            .is_err()
    );
    assert!(
        store
            .workflow_put(&g, kind, "big", None, &json!("x".repeat(65536)))
            .await
            .is_err()
    );
    for key in ["z", "A", "a", "é"] {
        store
            .workflow_put(&g, WorkflowKind::ResourceBinding, key, None, &json!(key))
            .await
            .unwrap();
    }
    let first = store
        .workflow_list(&g, WorkflowKind::ResourceBinding, None, 2)
        .await
        .unwrap();
    assert_eq!(
        first.iter().map(|v| v.key.as_str()).collect::<Vec<_>>(),
        vec!["A", "a"]
    );
    let second = store
        .workflow_list(&g, WorkflowKind::ResourceBinding, Some("a"), 2)
        .await
        .unwrap();
    assert_eq!(
        second.iter().map(|v| v.key.as_str()).collect::<Vec<_>>(),
        vec!["z", "é"]
    );
    for i in 0..10 {
        store
            .workflow_put(
                &g,
                WorkflowKind::CommandBinding,
                &format!("large{i}"),
                None,
                &json!("x".repeat(65000)),
            )
            .await
            .unwrap();
    }
    let page = store
        .workflow_list(&g, WorkflowKind::CommandBinding, None, 100)
        .await
        .unwrap();
    assert!(!page.is_empty() && page.len() < 10);
    assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
    assert!(store.workflow_list(&g, kind, None, 101).await.is_err());
    store.close().await.unwrap();
    let store = Storage::open(config.clone()).await.unwrap();
    assert_eq!(
        store.workflow_get(&g, kind, "plan").await.unwrap(),
        Some(saved)
    );
    store
}
pub(super) async fn restored(store: &Storage) {
    let g = GuildId::new("123").unwrap();
    let record = store
        .workflow_get(&g, WorkflowKind::StructurePlan, "plan")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.revision, 1);
    assert_eq!(record.value["steps"], json!(["minecraft"]));
    assert_ne!(
        record.value["deployment"],
        json!(store.status(None).await.unwrap().deployment)
    );
    assert_eq!(
        store
            .workflow_get(&g, WorkflowKind::Configuration, "cas")
            .await
            .unwrap()
            .unwrap()
            .value,
        json!(9)
    );
}
