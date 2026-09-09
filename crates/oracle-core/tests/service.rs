//! Policy/effect orchestration checks through public service and repository ports.
//! This fake supplies observable scoped records; database durability is verified
//! independently by the concrete storage integration suite.
use async_trait::async_trait;
use oracle_core::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn guild_a() -> GuildId {
    GuildId::new("100").unwrap()
}
fn guild_b() -> GuildId {
    GuildId::new("200").unwrap()
}
fn operator() -> UserId {
    UserId::new("10").unwrap()
}
fn context() -> PolicyContext {
    PolicyContext::Discord {
        guild: guild_a(),
        user: operator(),
        manage_guild: true,
    }
}

struct Records {
    guilds: BTreeMap<GuildId, GuildState>,
    operations: BTreeMap<OperationId, Operation>,
    effects: BTreeMap<EffectId, Effect>,
}
struct MemoryRepository {
    records: Mutex<Records>,
    calls: Mutex<Vec<&'static str>>,
    fail_sent_cas: AtomicBool,
    deployment: DeploymentId,
}
impl MemoryRepository {
    fn new() -> Self {
        Self {
            records: Mutex::new(Records {
                guilds: [guild_a(), guild_b()]
                    .into_iter()
                    .map(|guild| {
                        (
                            guild.clone(),
                            GuildState {
                                guild,
                                paused: false,
                                revision: 0,
                            },
                        )
                    })
                    .collect(),
                operations: BTreeMap::new(),
                effects: BTreeMap::new(),
            }),
            calls: Mutex::new(Vec::new()),
            fail_sent_cas: AtomicBool::new(false),
            deployment: DeploymentId::generate(),
        }
    }
    fn called(&self, name: &'static str) {
        self.calls.lock().unwrap().push(name);
    }
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn effect_records(&self) -> Vec<Effect> {
        self.records
            .lock()
            .unwrap()
            .effects
            .values()
            .cloned()
            .collect()
    }
}
#[async_trait]
impl Repository for MemoryRepository {
    async fn status(&self, guild: Option<&GuildId>) -> Result<Status> {
        self.called("status");
        let records = self.records.lock().unwrap();
        Ok(Status {
            deployment: self.deployment.clone(),
            guilds: records
                .guilds
                .values()
                .filter(|g| guild.is_none_or(|scope| &g.guild == scope))
                .cloned()
                .collect(),
            modules_loaded: 0,
            recovery_required: records
                .effects
                .values()
                .filter(|e| matches!(e.state, EffectState::Sent | EffectState::Unknown))
                .count() as u64,
            ai_available: false,
        })
    }
    async fn set_paused(
        &self,
        guild: &GuildId,
        paused: bool,
        expected_revision: u64,
        actor: &str,
        operation: &OperationId,
    ) -> Result<ControlReceipt> {
        self.called("set_paused");
        let mut records = self.records.lock().unwrap();
        let current = records
            .guilds
            .get_mut(guild)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if current.revision != expected_revision {
            return Err(Error::new(ErrorCode::Conflict));
        }
        current.paused = paused;
        current.revision += 1;
        let guild_state = current.clone();
        records.operations.insert(
            operation.clone(),
            Operation {
                id: operation.clone(),
                guild: guild.clone(),
                actor: actor.into(),
                state: OperationState::Succeeded,
                revision: 0,
            },
        );
        Ok(ControlReceipt {
            operation: operation.clone(),
            guild: guild_state,
        })
    }
    async fn begin_operation(&self, operation: &Operation) -> Result<()> {
        self.called("begin_operation");
        self.records
            .lock()
            .unwrap()
            .operations
            .insert(operation.id.clone(), operation.clone());
        Ok(())
    }
    async fn reserve_effect(&self, effect: &Effect) -> Result<Effect> {
        self.called("reserve_effect");
        let mut records = self.records.lock().unwrap();
        if let Some(existing) = records
            .effects
            .values()
            .find(|existing| existing.guild == effect.guild && existing.purpose == effect.purpose)
        {
            return Ok(existing.clone());
        }
        records.effects.insert(effect.id.clone(), effect.clone());
        Ok(effect.clone())
    }
    async fn effect(&self, guild: &GuildId, id: &EffectId) -> Result<Effect> {
        self.called("effect");
        self.records
            .lock()
            .unwrap()
            .effects
            .get(id)
            .filter(|effect| &effect.guild == guild)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound))
    }
    async fn transition_effect(
        &self,
        guild: &GuildId,
        id: &EffectId,
        expected_revision: u64,
        next: EffectState,
        receipt: Option<Value>,
    ) -> Result<Effect> {
        self.called("transition_effect");
        let mut records = self.records.lock().unwrap();
        let effect = records
            .effects
            .get_mut(id)
            .filter(|effect| &effect.guild == guild)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        if effect.revision != expected_revision
            || (next == EffectState::Sent && self.fail_sent_cas.load(Ordering::SeqCst))
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        effect.state = next;
        effect.revision += 1;
        effect.receipt = receipt;
        Ok(effect.clone())
    }
    async fn finish_operation(
        &self,
        guild: &GuildId,
        id: &OperationId,
        state: OperationState,
    ) -> Result<()> {
        self.called("finish_operation");
        let mut records = self.records.lock().unwrap();
        let operation = records
            .operations
            .get_mut(id)
            .filter(|operation| &operation.guild == guild)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        operation.state = state;
        operation.revision += 1;
        Ok(())
    }
    async fn recovery(&self, guild: &GuildId, limit: u32) -> Result<Vec<Effect>> {
        self.called("recovery");
        Ok(self
            .records
            .lock()
            .unwrap()
            .effects
            .values()
            .filter(|effect| {
                &effect.guild == guild
                    && matches!(effect.state, EffectState::Sent | EffectState::Unknown)
            })
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

#[derive(Clone, Copy)]
enum AdapterMode {
    Success,
    Error,
    Pending,
}
struct Adapter {
    repository: Arc<MemoryRepository>,
    mode: AdapterMode,
    calls: AtomicUsize,
    entered: Notify,
    dropped: AtomicBool,
}
struct Dropped<'a>(&'a AtomicBool);
impl Drop for Dropped<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl EffectAdapter for Adapter {
    async fn apply(&self, effect: &Effect) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stored = self
            .repository
            .records
            .lock()
            .unwrap()
            .effects
            .get(&effect.id)
            .unwrap()
            .clone();
        assert_eq!(
            stored.state,
            EffectState::Sent,
            "adapter ran before the send was journaled"
        );
        assert_eq!(stored.revision, effect.revision);
        assert_eq!(stored.guild, effect.guild);
        let _guard = Dropped(&self.dropped);
        self.entered.notify_one();
        match self.mode {
            AdapterMode::Success => Ok(json!({"verified_resource":"fixture-channel"})),
            AdapterMode::Error => Err(Error::new(ErrorCode::Io)),
            AdapterMode::Pending => std::future::pending().await,
        }
    }
}
fn setup(mode: AdapterMode) -> (CoreService, Arc<MemoryRepository>, Adapter) {
    let repository = Arc::new(MemoryRepository::new());
    let service = CoreService::new(
        repository.clone(),
        vec![
            GuildPolicy {
                guild: guild_a(),
                operators: vec![operator()],
            },
            GuildPolicy {
                guild: guild_b(),
                operators: vec![operator()],
            },
        ],
    );
    let adapter = Adapter {
        repository: repository.clone(),
        mode,
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        dropped: AtomicBool::new(false),
    };
    (service, repository, adapter)
}

#[tokio::test]
async fn cross_guild_and_unscoped_discord_requests_never_reach_repository_or_adapter() {
    let (service, repository, adapter) = setup(AdapterMode::Success);
    let cancel = CancellationToken::new();
    assert_eq!(
        service.status(&context(), None).await.unwrap_err().code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(
        service
            .status(&context(), Some(&guild_b()))
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(
        service
            .control(&context(), &guild_b(), true, 0)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(
        service
            .recovery(&context(), &guild_b(), 10)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(
        service
            .execute(&context(), &guild_b(), "other-guild", &adapter, &cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    let unconfigured = GuildId::new("300").unwrap();
    assert_eq!(
        service
            .status(&PolicyContext::LocalOperator, Some(&unconfigured))
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    assert_eq!(repository.call_count(), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    let local = service
        .status(&PolicyContext::LocalOperator, None)
        .await
        .unwrap();
    assert_eq!(local.guilds.len(), 2);
    let scoped = service.status(&context(), Some(&guild_a())).await.unwrap();
    assert_eq!(scoped.guilds.len(), 1);
    assert_eq!(scoped.guilds[0].guild, guild_a());
}

#[tokio::test]
async fn writes_require_both_operator_policy_and_current_manage_guild() {
    let (service, repository, adapter) = setup(AdapterMode::Success);
    let cancel = CancellationToken::new();
    for (user, manage_guild) in [(operator(), false), (UserId::new("20").unwrap(), true)] {
        let unauthorized = PolicyContext::Discord {
            guild: guild_a(),
            user,
            manage_guild,
        };
        assert_eq!(
            service
                .control(&unauthorized, &guild_a(), true, 0)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
        assert_eq!(
            service
                .recovery(&unauthorized, &guild_a(), 10)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
        assert_eq!(
            service
                .execute(&unauthorized, &guild_a(), "setup", &adapter, &cancel)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
    }
    assert_eq!(repository.call_count(), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn paused_guild_and_pre_cancelled_request_never_reserve_or_dispatch() {
    let (service, repository, adapter) = setup(AdapterMode::Success);
    let cancel = CancellationToken::new();
    service
        .control(&context(), &guild_a(), true, 0)
        .await
        .unwrap();
    repository.calls.lock().unwrap().clear();
    assert_eq!(
        service
            .execute(&context(), &guild_a(), "paused", &adapter, &cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenPermission
    );
    assert_eq!(*repository.calls.lock().unwrap(), ["status"]);
    assert!(repository.effect_records().is_empty());
    repository.calls.lock().unwrap().clear();
    cancel.cancel();
    assert_eq!(
        service
            .execute(&context(), &guild_a(), "cancelled", &adapter, &cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    assert_eq!(repository.call_count(), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn verified_purpose_reuses_effect_and_does_not_send_twice() {
    let (service, repository, adapter) = setup(AdapterMode::Success);
    let cancel = CancellationToken::new();
    let first = service
        .execute(&context(), &guild_a(), "setup-minecraft", &adapter, &cancel)
        .await
        .unwrap();
    let second = service
        .execute(&context(), &guild_a(), "setup-minecraft", &adapter, &cancel)
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.state, EffectState::Verified);
    assert_eq!(
        second.receipt,
        Some(json!({"verified_resource":"fixture-channel"}))
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(repository.effect_records().len(), 1);
    assert_eq!(
        repository.records.lock().unwrap().operations[&first.operation].state,
        OperationState::Succeeded
    );
    assert!(
        service
            .recovery(&context(), &guild_a(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn adapter_error_is_durable_unknown_and_requires_reconciliation() {
    let (service, repository, adapter) = setup(AdapterMode::Error);
    let cancel = CancellationToken::new();
    assert_eq!(
        service
            .execute(&context(), &guild_a(), "error", &adapter, &cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::UnknownOutcome
    );
    let records = service.recovery(&context(), &guild_a(), 10).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, EffectState::Unknown);
    assert!(records[0].receipt.is_none());
    assert_eq!(
        repository.records.lock().unwrap().operations[&records[0].operation].state,
        OperationState::RecoveryRequired
    );
    assert_eq!(
        service
            .execute(&context(), &guild_a(), "error", &adapter, &cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert!(adapter.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_during_admitted_send_journals_unknown_and_drops_adapter_future() {
    let (service, repository, adapter) = setup(AdapterMode::Pending);
    let cancel = CancellationToken::new();
    let ctx = context();
    let guild = guild_a();
    let execute = service.execute(&ctx, &guild, "cancel-after-send", &adapter, &cancel);
    let trigger = async {
        adapter.entered.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(execute, trigger)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::UnknownOutcome);
    assert!(adapter.dropped.load(Ordering::SeqCst));
    let records = service.recovery(&context(), &guild_a(), 10).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, EffectState::Unknown);
    assert_eq!(
        repository.records.lock().unwrap().operations[&records[0].operation].state,
        OperationState::RecoveryRequired
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn repository_cas_failures_propagate_without_adapter_dispatch() {
    let (service, repository, adapter) = setup(AdapterMode::Success);
    assert_eq!(
        service
            .control(&context(), &guild_a(), true, 99)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    {
        let records = repository.records.lock().unwrap();
        let state = &records.guilds[&guild_a()];
        assert!(!state.paused);
        assert_eq!(state.revision, 0);
    }
    repository.fail_sent_cas.store(true, Ordering::SeqCst);
    assert_eq!(
        service
            .execute(
                &context(),
                &guild_a(),
                "cas",
                &adapter,
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    let effects = repository.effect_records();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].state, EffectState::Prepared);
}
