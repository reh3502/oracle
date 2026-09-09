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
    agent_records(&store).await;
    store.close().await.unwrap();
    let store = Storage::open(config.clone()).await.unwrap();
    assert_eq!(
        store.workflow_get(&g, kind, "plan").await.unwrap(),
        Some(saved)
    );
    agent_records_survive(&store).await;
    store
}
pub(super) async fn restored(store: &Storage) {
    agent_records_survive(store).await;
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

const AGENT_KINDS: [(WorkflowKind, &str); 3] = [
    (WorkflowKind::AgentRun, "agent_run"),
    (WorkflowKind::AgentCall, "agent_call"),
    (WorkflowKind::AgentSpend, "agent_spend"),
];

pub(super) async fn agent_records(store: &Storage) {
    let guild = GuildId::new("123").unwrap();
    let other = GuildId::new("456").unwrap();
    store
        .initialize_guilds(&[guild.clone(), other.clone()])
        .await
        .unwrap();
    for (kind, name) in AGENT_KINDS {
        assert_eq!(kind.as_str(), name);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
        assert_eq!(
            serde_json::from_value::<WorkflowKind>(json!(name)).unwrap(),
            kind
        );
        assert!(
            store
                .workflow_get(&guild, kind, "agent")
                .await
                .unwrap()
                .is_none()
        );
        let saved = store
            .workflow_put(&guild, kind, "agent", None, &json!(name))
            .await
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert!(
            store
                .workflow_get(&other, kind, "agent")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .workflow_list(&other, kind, None, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .workflow_put(&other, kind, "agent", Some(1), &json!(null))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let store = store.clone();
            let guild = guild.clone();
            tasks.spawn(async move {
                store
                    .workflow_put(&guild, kind, "agent", Some(1), &json!({"kind":name}))
                    .await
            });
        }
        let mut wins = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(record) => {
                    wins += 1;
                    assert_eq!(record.revision, 2);
                }
                Err(error) => assert_eq!(error.code, ErrorCode::Conflict),
            }
        }
        assert_eq!(wins, 1);
        assert_eq!(
            store
                .workflow_put(&guild, kind, "agent", None, &json!(null))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // JSON quotes count toward the unchanged 64 KiB per-record bound.
        store
            .workflow_put(&guild, kind, "boundary", None, &json!("x".repeat(65534)))
            .await
            .unwrap();
        assert_eq!(
            store
                .workflow_put(&guild, kind, "oversize", None, &json!("x".repeat(65535)))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidInput
        );
    }
    // Calls remain separate records; updating one never rewrites another call.
    store
        .workflow_put(
            &guild,
            WorkflowKind::AgentCall,
            "call-2",
            None,
            &json!({"run":"agent"}),
        )
        .await
        .unwrap();
}

pub(super) async fn agent_records_survive(store: &Storage) {
    let guild = GuildId::new("123").unwrap();
    for (kind, name) in AGENT_KINDS {
        let record = store
            .workflow_get(&guild, kind, "agent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.revision, 2);
        assert_eq!(record.value, json!({"kind":name}));
        let page = store.workflow_list(&guild, kind, None, 100).await.unwrap();
        assert_eq!(page[0], record);
        assert_eq!(
            store
                .workflow_get(&guild, kind, "boundary")
                .await
                .unwrap()
                .unwrap()
                .value,
            json!("x".repeat(65534))
        );
    }
    let call = store
        .workflow_get(&guild, WorkflowKind::AgentCall, "call-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(call.revision, 1);
    assert_eq!(call.value, json!({"run":"agent"}));
}
