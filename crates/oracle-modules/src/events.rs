//! Host event queues and journaled notifications. No token or raw endpoint crosses RPC.
use super::*;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{Mutex, atomic::AtomicUsize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct NotificationRequest {
    pub actor: PolicyContext,
    pub guild: GuildId,
    pub module: ModuleId,
    pub configuration_revision: u64,
    pub destination: String,
    pub text: String,
}
#[async_trait]
pub trait NotificationCheck: Send + Sync {
    /// Re-read approved configuration and current host prerequisites. Call after
    /// rate-limit readiness and again for each retry before the actual dispatch.
    async fn validate(&self) -> Result<()>;
}
#[async_trait]
pub trait NotificationTransport: Send + Sync {
    /// Host implementation must call check after every wait/retry, then use permit
    /// at the immediate send boundary. Return independent remote readback evidence.
    async fn send(
        &self,
        request: &NotificationRequest,
        permit: &crate::DispatchPermit,
        check: &dyn NotificationCheck,
        cancel: CancellationToken,
    ) -> Result<Value>;
}
#[derive(Clone)]
pub(super) struct EventServices {
    intents: BTreeSet<String>,
    transport: Arc<dyn NotificationTransport>,
}
#[derive(Default)]
struct QueueStats {
    delivered: AtomicUsize,
    dropped: AtomicUsize,
    last_error: Mutex<Option<ErrorCode>>,
}
pub(super) struct EventQueue {
    generation: Arc<Generation>,
    guild: GuildId,
    epoch: u64,
    sender: mpsc::Sender<GuildEvent>,
    cancel: CancellationToken,
    stats: Arc<QueueStats>,
}
#[derive(Clone, Debug, Serialize)]
pub struct EventHealth {
    pub module: ModuleId,
    pub guild: GuildId,
    pub generation: u64,
    pub epoch: u64,
    pub accepting: bool,
    pub queued: usize,
    pub delivered: usize,
    pub dropped: usize,
    pub last_error: Option<ErrorCode>,
    pub missing_intents: Vec<String>,
}
#[derive(Default, Clone, Debug, Serialize)]
pub struct EventDispatch {
    pub accepted: usize,
    pub dropped: usize,
    pub unavailable: usize,
}
struct FreshCheck {
    manager: Weak<ModuleManager>,
    request: NotificationRequest,
}
#[async_trait]
impl NotificationCheck for FreshCheck {
    async fn validate(&self) -> Result<()> {
        let manager = self.manager.upgrade().ok_or_else(unavailable)?;
        manager
            .core
            .authorize_module(&self.request.actor, &self.request.guild)
            .await?;
        let generation = manager.get(&self.request.module)?;
        manager.validate_dependencies(&generation, &self.request.guild)?;
        manager.require_event_intents(&generation)?;
        let configuration = manager
            .configuration_verified(&self.request.actor, &self.request.guild, &generation)
            .await?;
        if configuration.revision != self.request.configuration_revision {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let values = configuration.values;
        if values.get("destination").and_then(Value::as_str)
            != Some(self.request.destination.as_str())
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(())
    }
}
struct NotificationAdapter {
    request: NotificationRequest,
    permit: crate::DispatchPermit,
    check: FreshCheck,
    transport: Arc<dyn NotificationTransport>,
    cancel: CancellationToken,
}
#[async_trait]
impl EffectAdapter for NotificationAdapter {
    async fn apply(&self, _: &Effect) -> Result<Value> {
        self.check.validate().await?;
        let delivery = self
            .transport
            .send(
                &self.request,
                &self.permit,
                &self.check,
                self.cancel.clone(),
            )
            .await?;
        Ok(
            json!({"destination":self.request.destination, "text_digest":text_digest(&self.request.text), "delivery":delivery}),
        )
    }
}
fn text_digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
impl ModuleManager {
    pub fn set_event_services(
        &self,
        available_intents: BTreeSet<String>,
        transport: Arc<dyn NotificationTransport>,
    ) -> Result<()> {
        if available_intents
            .iter()
            .any(|v| !matches!(v.as_str(), "guilds" | "guild_members" | "guild_moderation"))
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let mut services = self.event_services.write().unwrap();
        if services.is_some() {
            return Err(Error::new(ErrorCode::Conflict));
        }
        *services = Some(EventServices {
            intents: available_intents,
            transport,
        });
        Ok(())
    }
    /// Host Gateway connection state is authoritative. Clear these on loss of
    /// coverage; setting services does not manufacture privileged intent access.
    pub fn set_event_intents(&self, available_intents: BTreeSet<String>) -> Result<()> {
        if available_intents
            .iter()
            .any(|v| !matches!(v.as_str(), "guilds" | "guild_members" | "guild_moderation"))
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        self.event_services
            .write()
            .unwrap()
            .as_mut()
            .ok_or_else(unavailable)?
            .intents = available_intents;
        Ok(())
    }
    fn event_services(&self) -> Result<EventServices> {
        self.event_services
            .read()
            .unwrap()
            .clone()
            .ok_or_else(unavailable)
    }
    pub(super) fn require_event_intents(&self, generation: &Generation) -> Result<()> {
        let services = self.event_services()?;
        if generation
            .installed
            .package
            .manifest
            .required_intents
            .iter()
            .any(|i| !services.intents.contains(i))
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(())
    }
    pub fn event_intents(&self) -> BTreeSet<String> {
        self.event_services
            .read()
            .unwrap()
            .as_ref()
            .map(|services| services.intents.clone())
            .unwrap_or_default()
    }
    pub fn event_health(&self) -> Vec<EventHealth> {
        let intents = self
            .event_services
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.intents.clone())
            .unwrap_or_default();
        self.event_queues
            .lock()
            .unwrap()
            .values()
            .map(|q| {
                let missing_intents: Vec<_> = q
                    .generation
                    .installed
                    .package
                    .manifest
                    .required_intents
                    .iter()
                    .filter(|v| !intents.contains(*v))
                    .cloned()
                    .collect();
                EventHealth {
                    module: q.generation.installed.package.manifest.id.clone(),
                    guild: q.guild.clone(),
                    generation: q.generation.number,
                    epoch: q.epoch,
                    accepting: !q.sender.is_closed()
                        && !q.cancel.is_cancelled()
                        && q.generation.process().is_alive()
                        && q.generation.gate.is_active(&q.guild, q.epoch)
                        && missing_intents.is_empty(),
                    queued: q.sender.max_capacity() - q.sender.capacity(),
                    delivered: q.stats.delivered.load(Ordering::Relaxed),
                    dropped: q.stats.dropped.load(Ordering::Relaxed),
                    last_error: *q.stats.last_error.lock().unwrap(),
                    missing_intents,
                }
            })
            .collect()
    }
    pub(super) fn prune_event_queues(&self) {
        self.event_queues.lock().unwrap().retain(|_, queue| {
            let live = queue.generation.process().is_alive()
                && queue.generation.gate.is_active(&queue.guild, queue.epoch);
            if !live {
                queue.cancel.cancel();
            }
            live
        });
    }
    /// Only the trusted Gateway/scheduler calls this method. The supplied guild is
    /// checked against operator policy; modules cannot publish host event envelopes.
    pub async fn deliver_event(
        self: &Arc<Self>,
        guild: &GuildId,
        event: GuildEvent,
    ) -> Result<EventDispatch> {
        self.core
            .authorize_module(&PolicyContext::LocalOperator, guild)
            .await?;
        validate_event(&event)?;
        let generations: Vec<_> = self.registry.read().unwrap().values().cloned().collect();
        let mut result = EventDispatch::default();
        self.prune_event_queues();
        for generation in generations {
            if !generation
                .installed
                .package
                .manifest
                .subscriptions
                .contains(&event.kind)
            {
                continue;
            }
            let activation = generation.activations.lock().unwrap().get(guild).cloned();
            let Some(activation) = activation else {
                continue;
            };
            if !generation.gate.is_active(guild, activation.epoch)
                || self.require_event_intents(&generation).is_err()
                || !activation.grants.contains("events.guild")
                || !activation.grants.contains("config.own")
            {
                result.unavailable += 1;
                continue;
            }
            let key = (
                generation.installed.package.manifest.id.clone(),
                guild.clone(),
            );
            let mut queues = self.event_queues.lock().unwrap();
            if let Some(old) = queues.get(&key)
                && (old.generation.session != generation.session || old.epoch != activation.epoch)
            {
                old.cancel.cancel();
                queues.remove(&key);
            }
            if !queues.contains_key(&key) {
                if queues.len() >= 256 {
                    result.dropped += 1;
                    continue;
                }
                let (sender, receiver) = mpsc::channel(128);
                let cancel = self.event_tasks.token().child_token();
                let stats = Arc::new(QueueStats::default());
                let weak = Arc::downgrade(self);
                let worker_generation = generation.clone();
                let worker_guild = guild.clone();
                let worker_cancel = cancel.clone();
                let worker_stats = stats.clone();
                self.event_tasks
                    .spawn("module_event_queue", async move {
                        event_worker(
                            weak,
                            worker_generation,
                            worker_guild,
                            activation.epoch,
                            receiver,
                            worker_cancel,
                            worker_stats,
                        )
                        .await;
                        Ok(())
                    })
                    .map_err(|_| unavailable())?;
                queues.insert(
                    key.clone(),
                    EventQueue {
                        generation: generation.clone(),
                        guild: guild.clone(),
                        epoch: activation.epoch,
                        sender,
                        cancel,
                        stats,
                    },
                );
            }
            let queue = &queues[&key];
            if queue.sender.try_send(event.clone()).is_ok() {
                result.accepted += 1;
            } else {
                queue.stats.dropped.fetch_add(1, Ordering::Relaxed);
                result.dropped += 1;
            }
        }
        Ok(result)
    }
    pub(super) async fn notify(
        self: &Arc<Self>,
        request: NotificationRequest,
        purpose: String,
        authority: Authority,
        permit: crate::DispatchPermit,
        rpc_cancel: CancellationToken,
    ) -> Result<Value> {
        let services = self.event_services()?;
        let check = FreshCheck {
            manager: Arc::downgrade(self),
            request: request.clone(),
        };
        check.validate().await?;
        let slot = self
            .effect_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::new(ErrorCode::QuotaExceeded))?;
        let core = self.core.clone();
        let shutdown = self.effect_tasks.token();
        let key = format!(
            "module:{}:notify:{:.32}",
            request.module,
            text_digest(&purpose)
        );
        let expected_destination = request.destination.clone();
        let expected_text = text_digest(&request.text);
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.effect_tasks.spawn("module_notification",async move {
            let _slot=slot; let cancel=authority.cancel.child_token();
            let adapter = NotificationAdapter { request,permit,check,transport:services.transport,cancel:cancel.clone() };
            let operation=core.execute(&authority.actor,&authority.guild,&key,&adapter,&cancel);
            tokio::pin!(operation);
            let result=tokio::select! {biased;
                _=rpc_cancel.cancelled()=>{cancel.cancel();operation.await},
                _=shutdown.cancelled()=>{cancel.cancel();operation.await},
                _=tokio::time::sleep_until(authority.deadline)=>{cancel.cancel();operation.await},
                result=&mut operation=>result,
            }.and_then(|effect|effect.receipt.ok_or_else(||Error::new(ErrorCode::Integrity)))
                .and_then(|value| {
                    if value["destination"] != expected_destination || value["text_digest"] != expected_text { return Err(Error::new(ErrorCode::Conflict)); }
                    Ok(value["delivery"].clone())
                });
            let _=reply.send(result); Ok(())
        }).map_err(|_|unavailable())?;
        receive.await.map_err(|_| unavailable())?
    }
}
fn validate_event(event: &GuildEvent) -> Result<()> {
    if event.id.is_empty()
        || event.id.len() > 128
        || event.id.chars().any(char::is_control)
        || event.occurred_at_ms == 0
        || event.kind == GuildEventKind::Unknown
        || [&event.subject_id, &event.actor_id, &event.related_id]
            .iter()
            .any(|id| {
                id.as_ref().is_some_and(|id| {
                    id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_digit())
                })
            })
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn event_worker(
    manager: Weak<ModuleManager>,
    generation: Arc<Generation>,
    guild: GuildId,
    epoch: u64,
    mut receiver: mpsc::Receiver<GuildEvent>,
    cancel: CancellationToken,
    stats: Arc<QueueStats>,
) {
    let mut check = tokio::time::interval(Duration::from_millis(250));
    loop {
        let event = tokio::select! {biased;
            _=cancel.cancelled()=>break,
            _=check.tick()=>{if !generation.process().is_alive() || !generation.gate.is_active(&guild,epoch) { break; } continue;},
            event=receiver.recv()=>match event {Some(event)=>event,None=>break},
        };
        let Some(manager) = manager.upgrade() else {
            break;
        };
        let outcome = async {
            manager
                .core
                .authorize_module(&PolicyContext::LocalOperator, &guild)
                .await?;
            manager.require_event_intents(&generation)?;
            let configuration = manager
                .configuration_ready(&PolicyContext::LocalOperator, &guild, &generation)
                .await?;
            generation
                .event(&guild, event, configuration.revision, cancel.child_token())
                .await
        };
        let result = tokio::select! {biased;_=cancel.cancelled()=>Err(Error::new(ErrorCode::Cancelled)),result=outcome=>result};
        match result {
            Ok(_) => {
                stats.delivered.fetch_add(1, Ordering::Relaxed);
                *stats.last_error.lock().unwrap() = None;
            }
            Err(error) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                *stats.last_error.lock().unwrap() = Some(error.code);
            }
        }
    }
    receiver.close();
    while receiver.try_recv().is_ok() {
        stats.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
