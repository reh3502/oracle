use async_trait::async_trait;
use oracle_core::*;
use oracle_operations::{executor::*, permissions::*, structure::*};
use oracle_storage::{DatabaseConfig, Storage};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct World {
    snapshot: Mutex<Snapshot>,
    writes: AtomicUsize,
    lose_response: AtomicUsize,
    wait: tokio::sync::Semaphore,
    entered: tokio::sync::Notify,
    block: AtomicUsize,
}
#[async_trait]
impl StructureBackend for World {
    async fn inspect(&self, context: &PolicyContext, _guild: &GuildId) -> Result<Snapshot> {
        let mut s = self.snapshot.lock().unwrap().clone();
        s.observed_at = now();
        if let PolicyContext::Discord { user, .. } = context {
            s.actor.id = user.to_string();
        }
        Ok(s)
    }
    async fn mutate(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        m: &ChannelMutation,
        guard: &SendGuard,
    ) -> Result<Channel> {
        if self.block.load(Ordering::SeqCst) > 0 {
            self.entered.notify_one();
            let p = self.wait.acquire().await.unwrap();
            p.forget();
        }
        let observed = self.inspect(context, guild).await?;
        if observed.fingerprint()? != m.expected_fingerprint {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let result = guard.dispatch(|| {
            let mut world = self.snapshot.lock().unwrap();
            let mut channel = m.desired.clone();
            if let Some(before) = &m.before {
                world.channels.retain(|c| c.id != before.id);
            } else {
                channel.id = (200 + world.channels.len()).to_string();
            }
            world.channels.push(channel.clone());
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(channel)
        })?;
        if self.lose_response.swap(0, Ordering::SeqCst) > 0 {
            Err(Error::new(ErrorCode::Io))
        } else {
            Ok(result)
        }
    }
}
fn world() -> Arc<World> {
    Arc::new(World {
        snapshot: Mutex::new(Snapshot {
            guild: GuildId::new("100").unwrap(),
            owner: "99".into(),
            actor: Member {
                id: "50".into(),
                roles: vec![],
                timed_out: false,
            },
            bot: Member {
                id: "60".into(),
                roles: vec![],
                timed_out: false,
            },
            roles: vec![Role {
                id: "100".into(),
                position: 0,
                permissions: MANAGE_CHANNELS | MANAGE_ROLES | VIEW_CHANNEL | SEND_MESSAGES,
                managed: false,
            }],
            channels: vec![],
            complete: true,
            observed_at: now(),
        }),
        writes: AtomicUsize::new(0),
        lose_response: AtomicUsize::new(0),
        wait: tokio::sync::Semaphore::new(0),
        entered: tokio::sync::Notify::new(),
        block: AtomicUsize::new(0),
    })
}
fn request() -> StructureRequest {
    StructureRequest {
        channels: vec![
            DesiredChannel {
                key: "minecraft.category".into(),
                name: "Minecraft".into(),
                kind: ChannelKind::Category,
                parent: None,
                existing_id: None,
                overwrites: None,
            },
            DesiredChannel {
                key: "minecraft.chat".into(),
                name: "minecraft-chat".into(),
                kind: ChannelKind::Text,
                parent: Some("minecraft.category".into()),
                existing_id: None,
                overwrites: None,
            },
        ],
    }
}
fn service(storage: &Storage, world: Arc<World>) -> Arc<StructureExecutor> {
    let core = Arc::new(CoreService::new(
        Arc::new(storage.clone()),
        vec![GuildPolicy {
            guild: GuildId::new("100").unwrap(),
            operators: vec![UserId::new("50").unwrap(), UserId::new("51").unwrap()],
        }],
    ));
    Arc::new(StructureExecutor::new(
        core,
        Arc::new(storage.clone()),
        world,
    ))
}
async fn storage(root: &tempfile::TempDir) -> Storage {
    let storage = Storage::open(DatabaseConfig::Sqlite {
        path: root.path().join("db.sqlite"),
    })
    .await
    .unwrap();
    storage
        .initialize_guilds(&[GuildId::new("100").unwrap()])
        .await
        .unwrap();
    storage
}
#[tokio::test]
async fn saved_plan_survives_restart_and_repeated_setup_is_a_noop() {
    let root = tempfile::tempdir().unwrap();
    let store = storage(&root).await;
    let world = world();
    let executor = service(&store, world.clone());
    let guild = GuildId::new("100").unwrap();
    let context = PolicyContext::LocalOperator;
    let plan = executor.plan(&context, &guild, &request()).await.unwrap();
    drop(executor);
    store.close().await.unwrap();
    drop(store);
    let store = storage(&root).await;
    let executor = service(&store, world.clone());
    let done = executor
        .apply(&context, &guild, &plan.id, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(done.state, PlanState::Complete));
    assert_eq!(done.receipts.len(), 2);
    assert_eq!(world.writes.load(Ordering::SeqCst), 2);
    let repeated = executor.plan(&context, &guild, &request()).await.unwrap();
    assert!(repeated.steps.iter().all(|s| s.change == Change::Reuse));
    let done = executor
        .apply(&context, &guild, &repeated.id, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(done.state, PlanState::Complete));
    assert_eq!(world.writes.load(Ordering::SeqCst), 2);
    store.close().await.unwrap();
}
#[tokio::test]
async fn lost_create_response_keeps_reservation_and_prevents_duplicate_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let store = storage(&root).await;
    let world = world();
    world.lose_response.store(1, Ordering::SeqCst);
    let executor = service(&store, world.clone());
    let guild = GuildId::new("100").unwrap();
    let context = PolicyContext::LocalOperator;
    let plan = executor.plan(&context, &guild, &request()).await.unwrap();
    let partial = executor
        .apply(&context, &guild, &plan.id, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(partial.state, PlanState::Partial));
    assert_eq!(partial.last_error, Some(ErrorCode::UnknownOutcome));
    assert_eq!(world.writes.load(Ordering::SeqCst), 1);
    drop(executor);
    store.close().await.unwrap();
    drop(store);
    let store = storage(&root).await;
    let executor = service(&store, world.clone());
    assert_eq!(
        executor
            .plan(&context, &guild, &request())
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(
        executor
            .apply(&context, &guild, &plan.id, &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 1);
    store.close().await.unwrap();
}
#[tokio::test]
async fn permission_change_approval_is_exact_scoped_and_stale_on_drift() {
    let root = tempfile::tempdir().unwrap();
    let store = storage(&root).await;
    let world = world();
    let guild = GuildId::new("100").unwrap();
    world.snapshot.lock().unwrap().channels.push(Channel {
        id: "200".into(),
        guild: guild.clone(),
        parent: None,
        kind: ChannelKind::Text,
        name: "minecraft-chat".into(),
        overwrites: vec![Overwrite {
            id: "100".into(),
            kind: OverwriteKind::Role,
            allow: 0,
            deny: SEND_MESSAGES,
        }],
    });
    let executor = service(&store, world.clone());
    let context = PolicyContext::Discord {
        guild: guild.clone(),
        user: UserId::new("50").unwrap(),
        manage_guild: true,
    };
    let r = StructureRequest {
        channels: vec![DesiredChannel {
            key: "chat".into(),
            name: "minecraft-chat".into(),
            kind: ChannelKind::Text,
            parent: None,
            existing_id: Some("200".into()),
            overwrites: Some(vec![]),
        }],
    };
    let plan = executor.plan(&context, &guild, &r).await.unwrap();
    assert!(plan.steps[0].approval_required);
    assert_eq!(
        executor
            .apply(&context, &guild, &plan.id, &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenPermission
    );
    assert!(
        executor
            .approve(&context, &guild, &plan.id, "wrong")
            .await
            .is_err()
    );
    let other = PolicyContext::Discord {
        guild: guild.clone(),
        user: UserId::new("51").unwrap(),
        manage_guild: true,
    };
    assert!(
        executor
            .approve(&other, &guild, &plan.id, &plan.hash)
            .await
            .is_err()
    );
    executor
        .approve(&context, &guild, &plan.id, &plan.hash)
        .await
        .unwrap();
    world.snapshot.lock().unwrap().channels[0].name = "changed".into();
    assert_eq!(
        executor
            .apply(&context, &guild, &plan.id, &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    store.close().await.unwrap();
}
#[tokio::test]
async fn cancellation_while_queued_sends_nothing_and_retains_uncertainty() {
    let root = tempfile::tempdir().unwrap();
    let store = storage(&root).await;
    let world = world();
    world.block.store(1, Ordering::SeqCst);
    let executor = service(&store, world.clone());
    let guild = GuildId::new("100").unwrap();
    let context = PolicyContext::LocalOperator;
    let plan = executor.plan(&context, &guild, &request()).await.unwrap();
    let cancel = CancellationToken::new();
    let task = {
        let executor = executor.clone();
        let cancel = cancel.clone();
        let guild = guild.clone();
        tokio::spawn(async move {
            executor
                .apply(&PolicyContext::LocalOperator, &guild, &plan.id, &cancel)
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), world.entered.notified())
        .await
        .unwrap();
    cancel.cancel();
    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.last_error, Some(ErrorCode::UnknownOutcome));
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    world.wait.add_permits(1);
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    store.close().await.unwrap();
}
