//! Actual close/reopen recovery without Gemini configuration or reconstructed authority.
use super::*;
use oracle_ai::{budget::Reservation, provider::PreparedTurn, state::CallState};

fn draft(guild: &GuildId, actor: &PolicyContext) -> Run {
    Run::new(
        actor,
        guild.clone(),
        "Recover saved changes without repeating effects".into(),
        ModelProfile {
            id: "restart/v1".into(),
            model: "offline".into(),
            api_version: "offline".into(),
            max_context_tokens: 10000,
            max_output_tokens: 1000,
        },
        Limits {
            max_tokens: 10000,
            verification_tokens: 1000,
            max_cost_micros: 10000,
            max_requests: 10,
            max_tool_calls: 30,
            max_no_progress_turns: 3,
            deadline_ms: 300001,
        },
        PriceTable {
            revision: "restart/v1".into(),
            micros_per_million_tokens: 1_000_000,
        },
        1,
    )
}

#[tokio::test]
async fn host_startup_pauses_interrupted_discord_runs_and_settles_cancelled_spend_without_ai() {
    let root = tempfile::tempdir().unwrap();
    let guild = GuildId::new("881001").unwrap();
    let user = UserId::new("881002").unwrap();
    let context = PolicyContext::Discord {
        guild: guild.clone(),
        user: user.clone(),
        manage_guild: true,
    };
    let config = crate::config::Config {
        version: 1,
        state_dir: root.path().join("state"),
        database: if std::env::var_os("ORACLE_TEST_AI_POSTGRES_URL").is_some() {
            crate::config::Database::Postgres {
                url_env: "ORACLE_TEST_AI_POSTGRES_URL".into(),
            }
        } else {
            crate::config::Database::Sqlite {
                path: root.path().join("restart.sqlite"),
            }
        },
        guilds: vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![user],
        }],
        discord: None,
        ai: None,
    };
    let host = Host::open(&config, oracle_storage::PgTools::default())
        .await
        .unwrap();
    let store = RunStore::new(host.core.clone(), host.storage.clone());
    let spend = SpendStore::new(host.storage.clone());
    let mut ids = Vec::new();
    let prepared = PreparedTurn {
        body: "{}".into(),
        provider_metadata: None,
        input_token_reservation: 10,
        output_token_reservation: 10,
        timeout_ms: 1000,
        max_response_bytes: 1024,
    };
    for status in [RunStatus::Executing, RunStatus::Cancelled] {
        let mut saved = store
            .create(&context, draft(&guild, &context))
            .await
            .unwrap();
        let request = saved
            .run
            .budget
            .reserve(&saved.run.limits, &saved.run.prices, &prepared, 2, false)
            .unwrap();
        saved.run.pending_spend = Some(
            spend
                .reserve(&guild, &saved.run.id, &request, 2, 100)
                .await
                .unwrap(),
        );
        saved.run.status = status;
        saved.run.references.push("structure:existing-plan".into());
        host.storage
            .workflow_put(
                &guild,
                WorkflowKind::AgentRun,
                saved.run.id.as_str(),
                Some(saved.revision),
                &value(&saved.run).unwrap(),
            )
            .await
            .unwrap();
        store
            .admit_call(
                &saved,
                CallRecord {
                    run: saved.run.id.clone(),
                    call_id: "interrupted-apply".into(),
                    name: "core_discord_apply_v1".into(),
                    binding: "core:apply:1".into(),
                    arguments: json!({"reference":"structure:existing-plan"}),
                    state: CallState::Admitted,
                    result: None,
                    is_error: false,
                },
            )
            .await
            .unwrap();
        ids.push(saved.run.id);
    }
    let mut waiting = store
        .create(&context, draft(&guild, &context))
        .await
        .unwrap();
    waiting.run.status = RunStatus::WaitingApproval;
    host.storage
        .workflow_put(
            &guild,
            WorkflowKind::AgentRun,
            waiting.run.id.as_str(),
            Some(waiting.revision),
            &value(&waiting.run).unwrap(),
        )
        .await
        .unwrap();
    drop(store);
    drop(spend);
    host.close().await.unwrap();
    drop(host);

    let host = Host::open(&config, oracle_storage::PgTools::default())
        .await
        .unwrap();
    assert!(
        host.ai.get().is_none(),
        "startup recovery must not need a provider or key"
    );
    let store = RunStore::new(host.core.clone(), host.storage.clone());
    for (id, expected) in ids.iter().zip([RunStatus::Paused, RunStatus::Cancelled]) {
        let recovered = store.inspect(&context, &guild, id).await.unwrap();
        assert_eq!(recovered.run.status, expected);
        assert_eq!(recovered.run.owner, "discord:881002");
        assert_eq!(recovered.run.budget.unknown_attempts, 1);
        assert_eq!(recovered.run.budget.charged_tokens, 20);
        assert_eq!(recovered.run.budget.estimated_cost_micros, 20);
        assert!(recovered.run.budget.pending.is_none());
        assert!(recovered.run.pending_spend.is_none());
        assert_eq!(
            store.calls(&recovered).await.unwrap()[0].call.state,
            CallState::Unknown
        );
    }
    assert_eq!(
        store
            .inspect(&context, &guild, &waiting.run.id)
            .await
            .unwrap()
            .run
            .status,
        RunStatus::WaitingApproval
    );
    let day = host
        .storage
        .workflow_get(&guild, WorkflowKind::AgentSpend, "day:0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(day.value["total_micros"], 40);
    assert!(
        day.value["runs"]
            .as_object()
            .unwrap()
            .values()
            .all(|account| account["pending"].is_null())
    );
    let request = Reservation {
        sequence: 1,
        tokens: 61,
        cost_micros: 61,
        price_revision: "restart/v1".into(),
        micros_per_million_tokens: 1_000_000,
    };
    let spend = SpendStore::new(host.storage.clone());
    assert_eq!(
        spend
            .reserve(&guild, &OperationId::generate(), &request, 2, 100)
            .await
            .unwrap_err()
            .code,
        ErrorCode::QuotaExceeded
    );
    assert!(
        spend
            .reserve(&guild, &OperationId::generate(), &request, 86_400_002, 100)
            .await
            .is_ok()
    );
    let summary = oracle_ai::recovery::recover_guild(
        &store,
        &spend,
        host.storage.as_ref(),
        &guild,
        86_400_003,
    )
    .await
    .unwrap();
    assert_eq!(summary.settled_attempts, 0);
    assert_eq!(summary.interrupted_calls, 0);
    assert_eq!(summary.paused_runs, 0);
    host.close().await.unwrap();
}

struct NoProvider(ModelProfile);
#[async_trait::async_trait]
impl oracle_ai::provider::ModelProvider for NoProvider {
    fn profile(&self) -> &ModelProfile {
        &self.0
    }
    fn prepare(
        &self,
        _: oracle_ai::provider::ModelRequest,
    ) -> std::result::Result<PreparedTurn, oracle_ai::provider::ProviderError> {
        panic!("receipt-proven restart must not ask a model")
    }
    async fn send(
        &self,
        _: PreparedTurn,
        _: &CancellationToken,
    ) -> std::result::Result<oracle_ai::provider::ModelTurn, oracle_ai::provider::ProviderError>
    {
        panic!("receipt-proven restart must not send a provider request")
    }
}

#[tokio::test]
async fn agent_restart_resolves_only_owned_apply_with_fresh_complete_receipt_without_replay() {
    use std::sync::atomic::Ordering;
    let (_root, host, world) = super::tests::fixture().await;
    let guild = GuildId::new("100").unwrap();
    let context = PolicyContext::LocalOperator;
    let store = RunStore::new(host.core.clone(), host.storage.clone());
    let mut saved = store
        .create(&context, draft(&guild, &context))
        .await
        .unwrap();
    let executor = host.operations.get().unwrap();
    let request = decode(super::tests::desired()).unwrap();
    let plan = executor.plan(&context, &guild, &request).await.unwrap();
    let reference = format!("structure:{}", plan.id);
    saved.run.references.push(reference.clone());
    saved.run.status = RunStatus::Executing;
    host.storage
        .workflow_put(
            &guild,
            WorkflowKind::AgentRun,
            saved.run.id.as_str(),
            Some(saved.revision),
            &value(&saved.run).unwrap(),
        )
        .await
        .unwrap();
    let call = CallRecord {
        run: saved.run.id.clone(),
        call_id: "lost-apply-reply".into(),
        name: "core_discord_apply_v1".into(),
        binding: "core:apply:1".into(),
        arguments: json!({"reference":reference}),
        state: CallState::Admitted,
        result: None,
        is_error: false,
    };
    store.admit_call(&saved, call).await.unwrap();
    executor
        .apply(&context, &guild, &plan.id, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(world.writes.load(Ordering::SeqCst), 4);
    // Crash occurred after complete effect receipts but before the agent call reply.
    oracle_ai::recovery::recover_guild(
        &store,
        &SpendStore::new(host.storage.clone()),
        host.storage.as_ref(),
        &guild,
        3,
    )
    .await
    .unwrap();
    let ai = super::tests::coordinator_with_provider(
        &host,
        Arc::new(NoProvider(saved.run.profile.clone())),
    );
    let recovered = ai.resume(&context, &guild, &saved.run.id).await.unwrap();
    assert_eq!(recovered.run.status, RunStatus::Succeeded);
    let calls = store.calls(&recovered).await.unwrap();
    assert_eq!(calls[0].call.state, CallState::Finished);
    assert_eq!(
        calls[0].call.result.as_ref().unwrap()["reconciliation"],
        "fresh_host_receipt"
    );
    assert_eq!(
        world.writes.load(Ordering::SeqCst),
        4,
        "recovery must not replay mutations"
    );
    let tools = HostTools {
        host: Arc::downgrade(&host),
    };
    let mut unknown_creation = calls[0].call.clone();
    unknown_creation.state = CallState::Unknown;
    unknown_creation.name = "core_discord_plan_v1".into();
    unknown_creation.binding = "core:plan:1".into();
    let evidence = tools
        .reconcile(&context, &recovered.run, &[unknown_creation])
        .await
        .unwrap();
    assert!(
        evidence.unresolved,
        "unknown creations cannot inherit an apply receipt"
    );
    assert!(evidence.resolved_calls.is_empty());
    host.close().await.unwrap();
}

#[test]
fn catalog_unprojectable_or_colliding_module_tools_cannot_remove_core_operations() {
    let module = |id: &str, name: &str, schema: Value| oracle_modules::ModuleAiCatalogEntry {
        module: ModuleId::new(id).unwrap(),
        version: "1.0.0".into(),
        artifact_digest: "a".repeat(64),
        session: "module-session".into(),
        generation: 1,
        epoch: 1,
        operations: vec![ModuleOperation {
            name: name.into(),
            description: "Inspect unrelated state".into(),
            input_schema: schema,
            output_schema: json!({"type":"object"}),
            timeout_ms: 1000,
            capabilities: vec![],
            ai: Some(ModuleAiOperation {
                kind: ModuleAiOperationKind::Inspection,
                success_pointer: None,
            }),
        }],
        configuration: None,
    };
    let mut unsupported = module(
        "unrelated.schema",
        "inspect",
        json!({"oneOf":[{"type":"string"},{"type":"object","properties":{}}]}),
    );
    unsupported.configuration = Some(ModuleConfiguration {
        schema_version: 1,
        schema: json!({"type":"object","properties":{"value":{"type":["string","null"]}}}),
        presets: Default::default(),
    });
    let (entries, unavailable) = project_catalog(
        vec![
            unsupported,
            module("core.guild", "inspect", object(json!({}), &[])),
            module("core.tools", "search", object(json!({}), &[])),
            module("core.operation", "get", object(json!({}), &[])),
            module("extra.one", "look", object(json!({}), &[])),
            module("extra-one", "look", object(json!({}), &[])),
        ],
        true,
    );
    assert!(unavailable.len() >= 5);
    assert!(
        !entries
            .iter()
            .any(|entry| entry.definition.name == "extra_one_look_v1")
    );
    let catalog = Catalog::new(1, entries).unwrap();
    let selection = catalog
        .select("inspect guild structure channels plan apply", 12)
        .unwrap();
    for alias in [
        "core_tools_search_v1",
        "core_operation_get_v1",
        "core_guild_inspect_v1",
        "core_discord_plan_v1",
        "core_discord_apply_v1",
    ] {
        assert!(
            selection
                .tools
                .iter()
                .any(|tool| tool.definition.name == alias && tool.binding.starts_with("core:")),
            "core tool {alias} lost to module metadata"
        );
    }
    catalog.validate(&selection).unwrap();
}
