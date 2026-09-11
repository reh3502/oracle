//! Backend-independent admission, scope and crash-recovery contracts.
//! PostgreSQL requires a disposable, empty database supplied by check-stage4.py.
use oracle_ai::{
    budget::{Limits, PriceTable, Reservation},
    provider::ModelProfile,
    spend::SpendStore,
    state::{CallRecord, CallState, Run, RunStatus, RunStore},
};
use oracle_core::{
    CoreService, ErrorCode, GuildId, GuildPolicy, OperationId, PolicyContext, Repository, UserId,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use std::sync::Arc;

fn actor(guild: &GuildId, user: &str) -> PolicyContext {
    PolicyContext::Discord {
        guild: guild.clone(),
        user: UserId::new(user).unwrap(),
        manage_guild: true,
    }
}

fn stores(storage: &Arc<Storage>, guild: &GuildId) -> (RunStore, SpendStore) {
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![UserId::new("101").unwrap(), UserId::new("102").unwrap()],
        }],
    ));
    (
        RunStore::new(core, storage.clone()),
        SpendStore::new(storage.clone()),
    )
}

fn draft(guild: &GuildId) -> Run {
    Run::new(
        &actor(guild, "101"),
        guild.clone(),
        "Set up Minecraft without changing existing access".into(),
        ModelProfile {
            id: "contract".into(),
            model: "scripted-contract".into(),
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
            max_no_progress_turns: 2,
            deadline_ms: 300001,
        },
        PriceTable {
            revision: "contract/v1".into(),
            micros_per_million_tokens: 1000000,
        },
        1,
    )
}

async fn contract(config: DatabaseConfig) {
    let storage = Arc::new(Storage::open(config.clone()).await.unwrap());
    let guild = GuildId::new("100").unwrap();
    let foreign_guild = GuildId::new("200").unwrap();
    storage
        .initialize_guilds(&[guild.clone(), foreign_guild.clone()])
        .await
        .unwrap();
    let (runs, spend) = stores(&storage, &guild);
    let context = actor(&guild, "101");
    let saved = runs.create(&context, draft(&guild)).await.unwrap();

    // S01/S04: another authorized operator is still not this run's owner.
    assert_eq!(
        runs.inspect(&actor(&guild, "102"), &guild, &saved.run.id)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenPermission
    );
    assert_eq!(
        runs.inspect(&context, &foreign_guild, &saved.run.id)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(
        runs.create(&actor(&guild, "103"), draft(&guild))
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenPermission
    );

    // R03/R05: concurrent duplicate admission releases only one dispatch permit.
    let call = CallRecord {
        run: saved.run.id.clone(),
        call_id: "dispatch-1".into(),
        name: "core_discord_apply_v1".into(),
        binding: "epoch-7:owned-plan".into(),
        arguments: json!({"plan_ref":"owned-plan"}),
        state: CallState::Admitted,
        result: None,
        is_error: false,
    };
    let (a, b) = tokio::join!(
        runs.admit_call(&saved, call.clone()),
        runs.admit_call(&saved, call.clone())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        usize::from(a.newly_admitted) + usize::from(b.newly_admitted),
        1
    );
    let mut changed = call.clone();
    changed.arguments = json!({"plan_ref":"foreign-plan"});
    assert_eq!(
        runs.admit_call(&saved, changed).await.unwrap_err().code,
        ErrorCode::Conflict
    );

    // R06: concurrent runs compete for the same monetary reservation.
    let request = Reservation {
        sequence: 1,
        tokens: 600,
        cost_micros: 60,
        price_revision: "contract/v1".into(),
        micros_per_million_tokens: 100000,
    };
    let other = OperationId::generate();
    let (a, b) = tokio::join!(
        spend.reserve(&guild, &saved.run.id, &request, 0, 100),
        spend.reserve(&guild, &other, &request, 0, 100)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let reservation = a.or(b).unwrap();
    drop(runs);
    drop(spend);
    storage.close().await.unwrap();
    drop(storage);

    // Reopen the real database, not just a new facade over the same connection.
    let storage = Arc::new(Storage::open(config).await.unwrap());
    let (runs, spend) = stores(&storage, &guild);
    let recovered = runs
        .recover(&context, &guild, &saved.run.id, 2)
        .await
        .unwrap();
    assert_eq!(recovered.run.status, RunStatus::Recovering);
    let calls = runs.calls(&recovered).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call.state, CallState::Unknown);
    let repeated = runs.admit_call(&recovered, call).await.unwrap();
    assert!(!repeated.newly_admitted);
    assert_eq!(repeated.call.state, CallState::Unknown);

    // Unknown billing cannot be refunded on restart; midnight is a distinct ledger.
    spend.settle(&guild, &reservation, None).await.unwrap();
    let fresh = OperationId::generate();
    assert_eq!(
        spend
            .reserve(&guild, &fresh, &request, 1, 100)
            .await
            .unwrap_err()
            .code,
        ErrorCode::QuotaExceeded
    );
    assert_eq!(
        spend
            .settle(&guild, &reservation, Some(0))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let tomorrow = spend
        .reserve(&guild, &fresh, &request, 86400000, 100)
        .await
        .unwrap();
    assert_eq!(tomorrow.day, 1);
    spend.settle(&guild, &tomorrow, Some(10)).await.unwrap();

    // Permission is current state, and pause does not erase inspectable recovery facts.
    let revision = storage.status(Some(&guild)).await.unwrap().guilds[0].revision;
    storage
        .set_paused(
            &guild,
            true,
            revision,
            "local_operator",
            &OperationId::generate(),
        )
        .await
        .unwrap();
    assert!(runs.inspect(&context, &guild, &saved.run.id).await.is_ok());
    assert_eq!(
        runs.authorize(&context, &recovered).await.unwrap_err().code,
        ErrorCode::ForbiddenPermission
    );
    storage.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_durable_admission_scope_and_recovery() {
    let folder = tempfile::tempdir().unwrap();
    contract(DatabaseConfig::Sqlite {
        path: folder.path().join("agent.sqlite"),
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database; use scripts/check-stage4.py"]
async fn postgres_durable_admission_scope_and_recovery() {
    let url = std::env::var("ORACLE_TEST_AI_POSTGRES_URL")
        .expect("ORACLE_TEST_AI_POSTGRES_URL must name a disposable empty database");
    contract(DatabaseConfig::Postgres { url }).await;
}
