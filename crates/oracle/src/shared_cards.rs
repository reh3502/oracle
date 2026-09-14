//! Host composition for durable module projections and reusable public controls.
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
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub(crate) struct SharedCards {
    host: Weak<Host>,
    adapter: Arc<DiscordOperations>,
    pub journal: SharedCardJournal,
    destinations: Vec<SharedCardDestination>,
    cursor: std::sync::Mutex<usize>,
}
impl SharedCards {
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
    pub async fn tick(self: &Arc<Self>, cancel: &CancellationToken) -> Result<()> {
        let mut work = Vec::new();
        let mut scopes = std::collections::BTreeSet::new();
        for binding in &self.destinations {
            if scopes.insert((&binding.guild, &binding.module)) {
                for id in self
                    .journal
                    .run_ids(&binding.guild, &binding.module)
                    .await?
                {
                    work.push((binding.guild.clone(), binding.module.clone(), id));
                }
            }
        }
        if work.is_empty() {
            return Ok(());
        }
        let start = {
            let mut cursor = self.cursor.lock().unwrap();
            let start = *cursor % work.len();
            *cursor = (start + 4) % work.len();
            start
        };
        for offset in 0..work.len().min(4) {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            let (guild, module, id) = &work[(start + offset) % work.len()];
            if let Err(error) = self.reconcile(guild, module, id, cancel).await {
                tracing::debug!(%guild,%module,run=%id,error=?error.code,"shared card remains pending");
            }
        }
        Ok(())
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
            let observed = transport
                .observe(effect, cancel.child_token())
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
        let target = self.adapter.shared_target(guild, channel, cancel).await?;
        let effect_id = uuid::Uuid::new_v4().to_string();
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
        Ok(status(
            &self.journal.enqueue(guild, module, &id, intent).await?,
        ))
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
