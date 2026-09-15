//! Host composition for durable module projections and reusable public controls.
mod reminders;

use crate::{config::SharedCardDestination, host::Host};
use async_trait::async_trait;
use oracle_core::*;
use oracle_discord::{
    operations::DiscordOperations,
    shared_cards::{DiscordSharedCards, SharedCardAuthority},
};
use oracle_modules::{SharedCardDispatch, SharedCardService};
use oracle_operations::{executor::DispatchFence, published::render_private_card, shared_cards::*};
use serde_json::{Value, json};
use std::time::Duration;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
};
use tokio::{sync::Notify, task::JoinSet};
use tokio_util::sync::CancellationToken;

type CardKey = (GuildId, ModuleId, String);
fn dirty(phase: &SharedPhase) -> bool {
    matches!(phase, SharedPhase::Pending | SharedPhase::Unknown)
}
#[derive(Default)]
struct ReadyCards {
    queue: VecDeque<CardKey>,
}
impl ReadyCards {
    fn push(&mut self, key: CardKey) {
        if !self.queue.contains(&key) {
            self.queue.push_back(key);
        }
    }
    fn prioritize(&mut self, key: CardKey) {
        if let Some(index) = self.queue.iter().position(|pending| pending == &key) {
            self.queue.remove(index);
        }
        self.queue.push_front(key);
    }
    fn pop(&mut self, active: &BTreeSet<CardKey>) -> Option<CardKey> {
        let index = self.queue.iter().position(|key| !active.contains(key))?;
        self.queue.remove(index)
    }
}

#[derive(Default)]
struct PendingCards {
    ready: Mutex<ReadyCards>,
    wake: Notify,
}
impl PendingCards {
    fn push(&self, key: CardKey) {
        self.ready.lock().unwrap().prioritize(key);
        self.wake.notify_one();
    }
    fn recover(&self, key: CardKey) {
        self.ready.lock().unwrap().push(key);
        self.wake.notify_one();
    }
    fn pop(&self, active: &BTreeSet<CardKey>) -> Option<CardKey> {
        self.ready.lock().unwrap().pop(active)
    }
    fn next(
        &self,
        active: &BTreeSet<CardKey>,
        probe: &mut Option<CardKey>,
        probe_active: bool,
    ) -> Option<(CardKey, bool)> {
        self.pop(active).map(|key| (key, false)).or_else(|| {
            if probe_active || active.contains(probe.as_ref()?) {
                return None;
            }
            probe.take().map(|key| (key, true))
        })
    }
}

pub(crate) struct SharedCards {
    host: Weak<Host>,
    adapter: Arc<DiscordOperations>,
    pub journal: SharedCardJournal,
    destinations: Vec<SharedCardDestination>,
    cursor: std::sync::Mutex<usize>,
    pending: PendingCards,
}
impl SharedCards {
    pub async fn run_reminders(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        reminders::run(self, cancel).await
    }
    pub fn new(
        host: &Arc<Host>,
        adapter: Arc<DiscordOperations>,
        destinations: Vec<SharedCardDestination>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host: Arc::downgrade(host),
            adapter,
            journal: SharedCardJournal::new(host.storage.clone()),
            destinations,
            cursor: std::sync::Mutex::new(0),
            pending: PendingCards::default(),
        })
    }
    fn host(&self) -> Result<Arc<Host>> {
        self.host
            .upgrade()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))
    }
    fn destination(&self, guild: &GuildId, module: &ModuleId, name: &str) -> Result<&str> {
        self.destinations
            .iter()
            .find(|binding| {
                &binding.guild == guild && &binding.module == module && binding.destination == name
            })
            .map(|binding| binding.channel.as_str())
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))
    }
    fn queue(&self, key: CardKey) {
        self.pending.push(key);
    }
    /// Scanning the durable journal recovers work after restart or a lost wakeup.
    /// Only one unchanged publication is probed per scan; mutations use separate lanes.
    async fn scan(&self, probe_due: bool) -> Result<Option<CardKey>> {
        let mut probes = Vec::new();
        let mut scopes = BTreeSet::new();
        for binding in &self.destinations {
            if scopes.insert((&binding.guild, &binding.module)) {
                for id in self
                    .journal
                    .run_ids(&binding.guild, &binding.module)
                    .await?
                {
                    let key = (binding.guild.clone(), binding.module.clone(), id);
                    if let Some((_, record)) = self.journal.get(&key.0, &key.1, &key.2).await? {
                        if dirty(&record.phase) {
                            self.pending.recover(key);
                        } else if record.phase == SharedPhase::Confirmed && !record.desired.delete {
                            probes.push(key);
                        }
                    }
                }
            }
        }
        if !probe_due || probes.is_empty() {
            return Ok(None);
        }
        let mut cursor = self.cursor.lock().unwrap();
        let selected = probes[*cursor % probes.len()].clone();
        *cursor = (*cursor + 1) % probes.len();
        Ok(Some(selected))
    }
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut active = BTreeSet::new();
        let mut jobs = JoinSet::new();
        let mut probe = None;
        let mut probe_active = false;
        let mut next_probe = tokio::time::Instant::now();
        loop {
            while jobs.len() < 4 {
                let next = self.pending.next(&active, &mut probe, probe_active);
                let Some((key, background)) = next else {
                    break;
                };
                active.insert(key.clone());
                probe_active |= background;
                let worker = self.clone();
                let stop = cancel.child_token();
                jobs.spawn(async move {
                    let started = std::time::Instant::now();
                    if let Err(error) = worker.reconcile(&key.0, &key.1, &key.2, &stop).await {
                        tracing::debug!(guild=%key.0,module=%key.1,run=%key.2,error=?error.code,"shared card remains pending");
                    }
                    tracing::debug!(guild=%key.0,module=%key.1,run=%key.2,background,elapsed_ms=started.elapsed().as_millis() as u64,"shared card reconciliation finished");
                    (key, background)
                });
            }
            tokio::select! { biased;
                _ = cancel.cancelled() => {
                    // Cancellation is forwarded to each fenced operation before joining.
                    while jobs.join_next().await.is_some() {}
                    return Ok(());
                }
                completed = jobs.join_next(), if !jobs.is_empty() => {
                    let (key, background) = completed
                        .ok_or_else(|| Error::new(ErrorCode::Integrity))?
                        .map_err(|_| Error::new(ErrorCode::Integrity))?;
                    active.remove(&key);
                    if background { probe_active = false; }
                }
                _ = self.pending.wake.notified() => {}
                _ = interval.tick() => {
                    let probe_due = tokio::time::Instant::now() >= next_probe;
                    match self.scan(probe_due).await {
                        Ok(next) => if probe_due {
                            probe = next;
                            next_probe = tokio::time::Instant::now() + Duration::from_secs(60);
                        },
                        Err(error) => tracing::warn!(error=?error.code,"shared card scan deferred"),
                    }
                }
            }
        }
    }
    async fn reconcile(
        self: &Arc<Self>,
        guild: &GuildId,
        module: &ModuleId,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let host = self.host()?;
        let (_, mut record) = self
            .journal
            .get(guild, module, id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let transport = DiscordSharedCards::new(self.adapter.clone(), self.clone());
        if record.phase == SharedPhase::Unknown {
            let effect = record
                .effect
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            // Deleting the same journal-bound message is idempotent. Creates and
            // edits still require read-only recovery and are never blindly replayed.
            let recovery = async {
                if effect.payload.is_null() {
                    transport.execute(effect, cancel.child_token()).await
                } else {
                    transport.observe(effect, cancel.child_token()).await
                }
            };
            let observed = recovery
                .await
                .unwrap_or_else(|error| {
                    tracing::debug!(%guild,%module,run=%id,error=?error.code,"shared card observation unavailable");
                    SharedObservation::Unknown
                });
            self.journal.settle(effect, observed).await?;
            return Ok(());
        }
        // Re-read current module intent before freezing any new remote effect.
        let source = host.modules.shared_card_source(guild, module, id).await?;
        let intent: SharedIntent = serde_json::from_value(source.intent)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        record = self.journal.enqueue(guild, module, id, intent).await?;
        if record.desired.delete {
            if record.phase == SharedPhase::Confirmed {
                return Ok(());
            }
            if record.effect.is_none()
                && (record.identity.is_none() || record.phase == SharedPhase::Missing)
            {
                self.journal
                    .complete_unpublished_delete(guild, module, id)
                    .await?;
                return Ok(());
            }
        }
        if record.phase == SharedPhase::Confirmed {
            let identity = record
                .identity
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            let marker = "oracle-shared:presence-probe".to_string();
            let probe = SharedEffect {
                effect_id: "presence-probe".into(),
                guild: guild.clone(),
                module: module.clone(),
                run_id: id.into(),
                target: identity.target.clone(),
                message_id: Some(identity.message_id.clone()),
                desired_revision: record.desired.desired_revision,
                marker: marker.clone(),
                payload: json!({"embeds":[{"title":"Presence probe","footer":{"text":marker}}],"components":[],"allowed_mentions":{"parse":[]}}),
            };
            if matches!(
                transport.observe(&probe, cancel.child_token()).await?,
                SharedObservation::Missing
            ) {
                self.journal
                    .mark_missing(guild, module, id, &identity.message_id)
                    .await?;
            }
            return Ok(());
        }
        if record.phase != SharedPhase::Pending {
            return Ok(());
        }
        let channel = self.destination(guild, module, &record.desired.destination)?;
        let target = match &record.identity {
            Some(identity) if identity.target.channel_id == channel => identity.target.clone(),
            Some(_) => return Err(Error::new(ErrorCode::ForbiddenScope)),
            None => self.adapter.shared_target(guild, channel, cancel).await?,
        };
        let effect_id = uuid::Uuid::new_v4().to_string();
        if record.desired.delete {
            let identity = record
                .identity
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            let effect = SharedEffect {
                effect_id: effect_id.clone(),
                guild: guild.clone(),
                module: module.clone(),
                run_id: id.into(),
                target,
                message_id: Some(identity.message_id.clone()),
                desired_revision: record.desired.desired_revision,
                marker: format!("oracle-shared:{effect_id}"),
                payload: Value::Null,
            };
            self.journal.prepare(effect.clone()).await?;
            let observed = transport
                .execute(&effect, cancel.child_token())
                .await
                .unwrap_or(SharedObservation::Unknown);
            self.journal.settle(&effect, observed).await?;
            return Ok(());
        }
        let version = record
            .identity
            .as_ref()
            .map(|identity| identity.control_version.as_str())
            .unwrap_or(&effect_id);
        let marker = format!("oracle-shared:{effect_id}");
        let mut card = render_private_card(
            &json!({"reply":{"card":record.desired.card,"choices":[],"buttons":[]}}),
        )?;
        self.adapter
            .resolve_card_members(guild, &mut card, cancel)
            .await?;
        let footer = card.embed["footer"]["text"].as_str().unwrap_or("");
        card.embed["footer"] = json!({"text":format!("{}\n{}",footer,marker).trim()});
        let mut buttons = Vec::new();
        for (index, action) in record.desired.actions.iter().enumerate() {
            buttons.push(json!({"type":2,"style":2,"label":action.label,"custom_id":record.control_id(guild,version,index)?}));
        }
        let components = if buttons.is_empty() {
            json!([])
        } else {
            json!([{"type":1,"components":buttons}])
        };
        let effect = SharedEffect {
            effect_id,
            guild: guild.clone(),
            module: module.clone(),
            run_id: id.into(),
            target,
            message_id: record
                .identity
                .as_ref()
                .map(|identity| identity.message_id.clone()),
            desired_revision: record.desired.desired_revision,
            marker,
            payload: json!({"content":"","embeds":[card.embed],"components":components,"allowed_mentions":{"parse":[]},"attachments":[]}),
        };
        // No subsequent cancellation or timeout permits repeating this effect.
        self.journal.prepare(effect.clone()).await?;
        let observed = transport
            .execute(&effect, cancel.child_token())
            .await
            .unwrap_or(SharedObservation::Unknown);
        if let SharedObservation::NotSent { reason } = &observed {
            tracing::warn!(%guild,%module,run=%id,error=?reason,"shared card deferred before request submission");
        }
        self.journal.settle(&effect, observed).await?;
        Ok(())
    }
}
fn status(record: &SharedRecord) -> Value {
    json!({"state":match record.phase {SharedPhase::Pending=>"pending",SharedPhase::Confirmed=>"confirmed",SharedPhase::Unknown=>"recovery_required",SharedPhase::Missing=>"missing",SharedPhase::Rejected=>"rejected"},"desired_revision":record.desired.desired_revision,"confirmed_revision":record.confirmed_revision})
}
#[async_trait]
impl SharedCardService for SharedCards {
    async fn run_reminder(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        key: &str,
        document: Value,
    ) -> Result<Value> {
        reminders::process(self, module, guild, key, document).await
    }
    async fn enqueue(&self, module: &ModuleId, guild: &GuildId, intent: Value) -> Result<Value> {
        if serde_json::to_vec(&intent)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?
            .len()
            > 6 * 1024
        {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        let intent: SharedIntent =
            serde_json::from_value(intent).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        self.destination(guild, module, &intent.destination)?;
        render_private_card(&json!({"reply":{"card":intent.card,"choices":[],"buttons":[]}}))?;
        let id = intent.key.clone();
        let record = self.journal.enqueue(guild, module, &id, intent).await?;
        if dirty(&record.phase) {
            self.queue((guild.clone(), module.clone(), id));
        }
        Ok(status(&record))
    }
    async fn status(&self, module: &ModuleId, guild: &GuildId, intent_key: &str) -> Result<Value> {
        let record = self.journal.get(guild, module, intent_key).await?;
        Ok(record.map(|(_, record)| status(&record)).unwrap_or_else(
            || json!({"state":"pending","desired_revision":0,"confirmed_revision":null}),
        ))
    }
}
struct WorkerFence(SharedCardDispatch);
impl DispatchFence for WorkerFence {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.0.dispatch(send)?
    }
}
#[async_trait]
impl SharedCardAuthority for SharedCards {
    async fn authorize(&self, effect: &SharedEffect) -> Result<Arc<dyn DispatchFence>> {
        let host = self.host()?;
        let source = host
            .modules
            .shared_card_source(&effect.guild, &effect.module, &effect.run_id)
            .await?;
        let intent: SharedIntent = serde_json::from_value(source.intent)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if effect.payload.is_null() {
            let (_, saved) = self
                .journal
                .get(&effect.guild, &effect.module, &effect.run_id)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
            if !intent.delete
                || !saved.desired.delete
                || saved.phase != SharedPhase::Unknown
                || saved.effect.as_ref() != Some(effect)
                || saved.identity.as_ref().is_none_or(|identity| {
                    effect.message_id.as_ref() != Some(&identity.message_id)
                        || effect.target != identity.target
                })
            {
                return Err(Error::new(ErrorCode::ForbiddenScope));
            }
        }
        if self.destination(&effect.guild, &effect.module, &intent.destination)?
            != effect.target.channel_id
        {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        let lease = host
            .modules
            .shared_card_dispatch(
                &effect.guild,
                &effect.module,
                &source.session,
                source.generation,
                source.epoch,
            )
            .await?;
        Ok(Arc::new(WorkerFence(lease)))
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    fn key(run: &str) -> CardKey {
        (
            "123".parse().unwrap(),
            "test.runs".parse().unwrap(),
            run.into(),
        )
    }
    #[test]
    fn durable_mutations_and_recovery_have_priority_over_confirmed_probes() {
        assert!(dirty(&SharedPhase::Pending));
        assert!(dirty(&SharedPhase::Unknown));
        for phase in [
            SharedPhase::Confirmed,
            SharedPhase::Missing,
            SharedPhase::Rejected,
        ] {
            assert!(!dirty(&phase));
        }
    }
    #[test]
    fn queued_signup_runs_before_presence_probe_and_other_probes_cannot_fill_lanes() {
        let queue = PendingCards::default();
        let active = BTreeSet::new();
        let mut probe = Some(key("unchanged"));
        queue.recover(key("older-recovery"));
        queue.push(key("signup"));
        assert_eq!(
            queue.next(&active, &mut probe, false),
            Some((key("signup"), false))
        );
        assert_eq!(probe, Some(key("unchanged")));
        assert_eq!(
            queue.next(&active, &mut probe, false),
            Some((key("older-recovery"), false))
        );
        assert!(queue.next(&active, &mut probe, true).is_none());
        assert_eq!(
            queue.next(&active, &mut probe, false),
            Some((key("unchanged"), true))
        );
    }
    #[test]
    fn rapid_changes_coalesce_but_changes_during_dispatch_remain_queued() {
        let queue = PendingCards::default();
        let first = key("first");
        let other = key("other");
        queue.push(first.clone());
        queue.push(first.clone());
        let mut active = BTreeSet::new();
        assert_eq!(queue.pop(&active), Some(first.clone()));
        assert!(queue.pop(&active).is_none());
        active.insert(first.clone());
        queue.push(first.clone());
        queue.push(other.clone());
        assert_eq!(queue.pop(&active), Some(other));
        assert!(queue.pop(&active).is_none());
        active.clear();
        assert_eq!(queue.pop(&active), Some(first));
    }
    #[tokio::test]
    async fn enqueue_wakes_idle_worker_without_waiting_for_five_second_scan() {
        let queue = PendingCards::default();
        // An enqueue that races just before the worker starts waiting retains a permit.
        queue.push(key("before-wait"));
        tokio::time::timeout(Duration::from_millis(100), queue.wake.notified())
            .await
            .unwrap();
        assert_eq!(queue.pop(&BTreeSet::new()), Some(key("before-wait")));
        let waiting = queue.wake.notified();
        tokio::pin!(waiting);
        waiting.as_mut().enable();
        queue.push(key("during-wait"));
        tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap();
        assert_eq!(queue.pop(&BTreeSet::new()), Some(key("during-wait")));
    }
}
