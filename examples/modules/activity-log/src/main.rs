//! Separately installed metadata logger. Host events and host effects are the only IO ports.
#[cfg(any(feature = "typed-config-fixture", feature = "no-config-fixture"))]
mod configuration_fixture;
mod domain;
#[cfg(feature = "injection-fixture")]
mod injection_fixture;
use async_trait::async_trait;
use domain::*;
use oracle_contracts::{
    DocumentWrite, EffectiveConfiguration, GuildEvent, GuildEventKind, GuildId, ModuleManifest,
};
use oracle_module_sdk::{CallContext, GuildContext, Module, Result, RpcError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
fn invalid() -> RpcError {
    RpcError::Remote("invalid activity-log state or configuration".into())
}
#[derive(Default)]
struct Logger {
    config: Mutex<BTreeMap<GuildId, EffectiveConfiguration>>,
    locks: std::sync::Mutex<BTreeMap<GuildId, Arc<Mutex<()>>>>,
}
impl Logger {
    fn lock(&self, guild: &GuildId) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(guild.clone())
            .or_default()
            .clone()
    }
    async fn state(context: &CallContext) -> Result<(State, Option<u64>)> {
        let doc = context.document_get("logging", "state").await?;
        Ok(match doc {
            Some(doc) => (
                serde_json::from_value(doc.value).map_err(|_| invalid())?,
                Some(doc.revision),
            ),
            None => (State::default(), None),
        })
    }
    async fn save(context: &CallContext, state: &State, revision: Option<u64>) -> Result<()> {
        context
            .document_batch(vec![DocumentWrite {
                collection: "logging".into(),
                key: "state".into(),
                expected_revision: revision,
                value: Some(serde_json::to_value(state).map_err(|_| invalid())?),
            }])
            .await?;
        Ok(())
    }
    async fn process(&self, context: CallContext, event: GuildEvent) -> Result<Value> {
        let lock = self.lock(context.guild());
        let _lock = lock.lock().await;
        let config = self
            .config
            .lock()
            .await
            .get(context.guild())
            .cloned()
            .ok_or_else(invalid)?;
        let destination = config.values["destination"].as_str().ok_or_else(invalid)?;
        let (mut state, revision) = Self::state(&context).await?;
        let now = state.clock.max(event.occurred_at_ms);
        state.clock = now;
        if matches!(event.kind, GuildEventKind::Maintenance) {
            // Fixed bounded ring; no unscoped scan or module SQL access is needed.
            let mut writes = vec![];
            for index in 0..337 {
                let key = format!("hour:{index}");
                if let Some(doc) = context.document_get("logging", &key).await? {
                    let mut bucket: Bucket =
                        serde_json::from_value(doc.value).map_err(|_| invalid())?;
                    if bucket.expire(now) {
                        writes.push(DocumentWrite {
                            collection: "logging".into(),
                            key,
                            expected_revision: Some(doc.revision),
                            value: if bucket.records.is_empty() {
                                None
                            } else {
                                Some(serde_json::to_value(bucket).map_err(|_| invalid())?)
                            },
                        });
                    }
                    if writes.len() == 100 {
                        context.document_batch(std::mem::take(&mut writes)).await?;
                    }
                }
            }
            if !writes.is_empty() {
                context.document_batch(writes).await?;
            }
            state
                .pending
                .retain(|p| now.saturating_sub(p.created) < RETENTION);
            if state
                .membership_start
                .is_some_and(|start| now.saturating_sub(start) >= RETENTION)
            {
                state.membership_start = None;
                state.joined = 0;
                state.left = 0;
            }
            state.retention_last_run = Some(now);
            state.maintenance(now);
            Self::save(&context, &state, revision).await?;
        } else {
            let key = format!("hour:{}", event.occurred_at_ms / HOUR % 337);
            let document = context.document_get("logging", &key).await?;
            let mut bucket: Bucket = match &document {
                Some(doc) => serde_json::from_value(doc.value.clone()).map_err(|_| invalid())?,
                None => Bucket::default(),
            };
            state.maintenance(now);
            state.ingest(&event, now, &mut bucket);
            context
                .document_batch(vec![
                    DocumentWrite {
                        collection: "logging".into(),
                        key,
                        expected_revision: document.as_ref().map(|d| d.revision),
                        value: Some(serde_json::to_value(bucket).map_err(|_| invalid())?),
                    },
                    DocumentWrite {
                        collection: "logging".into(),
                        key: "state".into(),
                        expected_revision: revision,
                        value: Some(serde_json::to_value(&state).map_err(|_| invalid())?),
                    },
                ])
                .await?;
        }
        // Pending notifications were committed before dispatch. Uncertain effects retain
        // their same purpose so the host journal forbids an unsafe blind resend.
        for notification in state
            .pending
            .clone()
            .into_iter()
            .filter(|p| p.due <= now)
            .take(16)
        {
            let purpose = format!("log:{:x}", Sha256::digest(notification.purpose.as_bytes()));
            let text = format!(
                "{} ({} event{})",
                notification.text,
                notification.count,
                if notification.count == 1 { "" } else { "s" }
            );
            match context.notify(&purpose, destination, &text).await {
                Ok(_) => {
                    state.pending.retain(|p| p.purpose != notification.purpose);
                    state.delivered = state.delivered.saturating_add(1);
                    state.last_error = None;
                }
                Err(_) => {
                    state.last_error = Some(
                        "notification delivery unverified; inspect host effect journal".into(),
                    );
                }
            }
            let (_, revision) = Self::state(&context).await?;
            Self::save(&context, &state, revision).await?;
            if state.last_error.is_some() {
                break;
            }
        }
        Ok(json!({"accepted":true,"pending":state.pending.len(),"dropped":state.dropped}))
    }
}
#[async_trait]
impl Module for Logger {
    fn manifest(&self) -> ModuleManifest {
        let manifest: ModuleManifest = serde_json::from_str(include_str!("../manifest.json"))
            .expect("checked module manifest");
        #[cfg(feature = "injection-fixture")]
        let manifest = {
            let mut fixture = manifest;
            fixture
                .operations
                .iter_mut()
                .find(|operation| operation.name == "status")
                .expect("status operation")
                .description = injection_fixture::DESCRIPTION.into();
            fixture
        };
        #[cfg(feature = "typed-config-fixture")]
        let manifest: ModuleManifest =
            serde_json::from_value(configuration_fixture::configuration_variant(
                serde_json::to_value(manifest).expect("fixture manifest"),
                true,
            ))
            .expect("typed configuration fixture");
        #[cfg(feature = "no-config-fixture")]
        let manifest: ModuleManifest =
            serde_json::from_value(configuration_fixture::configuration_variant(
                serde_json::to_value(manifest).expect("fixture manifest"),
                false,
            ))
            .expect("missing configuration fixture");
        manifest
    }
    async fn prepare_configuration(&self, _: GuildContext, _: u64, values: Value) -> Result<()> {
        if !valid_config(&values) {
            return Err(invalid());
        }
        Ok(())
    }
    async fn apply_configuration(
        &self,
        context: GuildContext,
        revision: u64,
        values: Value,
    ) -> Result<()> {
        if !valid_config(&values) {
            return Err(invalid());
        }
        let lock = self.lock(&context.guild);
        let _lock = lock.lock().await;
        self.config.lock().await.insert(
            context.guild.clone(),
            EffectiveConfiguration { revision, values },
        );
        Ok(())
    }
    async fn effective_configuration(
        &self,
        context: GuildContext,
    ) -> Result<Option<EffectiveConfiguration>> {
        Ok(self.config.lock().await.get(&context.guild).cloned())
    }
    async fn deactivate(&self, guild: &GuildId, _: u64) -> Result<()> {
        self.config.lock().await.remove(guild);
        Ok(())
    }
    async fn event(&self, context: CallContext, event: GuildEvent) -> Result<Value> {
        self.process(context, event).await
    }
    async fn invoke(&self, context: CallContext, operation: &str, _: Value) -> Result<Value> {
        let lock = self.lock(context.guild());
        let _lock = lock.lock().await;
        let config = self.config.lock().await.get(context.guild()).cloned();
        let (mut state, revision) = Self::state(&context).await?;
        match operation {
            "status" => {
                let host = context.host_health().await;
                let (
                    host,
                    ready,
                    effective_subscriptions,
                    missing_intents,
                    destination_permissions,
                ) = match host {
                    Ok(host) => {
                        let ready = host.configuration.verified
                            && host.configuration.stored.as_ref() == config.as_ref()
                            && host.subscriptions.ready
                            && host.destination.verified;
                        let effective = host.subscriptions.effective.clone();
                        let missing = host.subscriptions.missing_intents.clone();
                        let destination =
                            serde_json::to_value(&host.destination).map_err(|_| invalid())?;
                        (
                            serde_json::to_value(host).map_err(|_| invalid())?,
                            ready,
                            effective,
                            missing,
                            destination,
                        )
                    }
                    Err(_) => (
                        json!({"state":"unavailable","error":"host_health_readback_failed"}),
                        false,
                        vec![],
                        vec![],
                        json!({"verified":false,"error":"host_health_readback_failed"}),
                    ),
                };
                Ok(
                    json!({"state":if ready&&state.last_error.is_none(){"ready"}else{"degraded"},"host":host,
                    "effective_configuration":config,"configured":config.is_some(),"requested_event_subscriptions":self.manifest().subscriptions,
                    "effective_event_subscriptions":effective_subscriptions,"missing_intents":missing_intents,"destination_permissions":destination_permissions,
                    "delivery_backlog":state.pending.len(),"delivered":state.delivered,"dropped":state.dropped,"observed_metadata_records":state.observed,
                    "observed_metadata_events":state.observed,"unknown_origin_observations":state.unknown_origin_observations,
                    "unattributed_administrative_observations":state.unattributed_administrative_observations,"unknown_audit_actor_events":state.unknown_audit_actor_events,
                    "attribution_state":if state.unknown_audit_actor_events==0{"no_audit_actor_gap_observed"}else{"audit_actor_gap_observed"},
                    "last_error":state.last_error,"retention_days":14,"retention_last_run_ms":state.retention_last_run,
                    "end_to_end_probe_verified":state.probe_verified&&config.as_ref().is_some_and(|c|Some(c.revision)==state.probe_revision&&c.values["destination"].as_str()==state.probe_destination.as_deref()),
                    "real_moderation_event_observed":state.moderation_observed>0}),
                )
            }
            "probe" => {
                let config = config.ok_or_else(invalid)?;
                let destination = config.values["destination"].as_str().ok_or_else(invalid)?;
                let purpose = format!(
                    "probe:{:x}",
                    Sha256::digest(format!("{destination}:{}", config.revision).as_bytes())
                );
                let receipt=context.notify(&purpose,destination,"Oracle activity-log synthetic delivery probe. No moderation action was performed.").await?;
                state.probe_verified = true;
                state.probe_destination = Some(destination.to_owned());
                state.probe_revision = Some(config.revision);
                Self::save(&context, &state, revision).await?;
                Ok(json!({"verified":true,"receipt":receipt}))
            }
            _ => Err(invalid()),
        }
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    if oracle_module_sdk::serve_stdio(Arc::new(Logger::default()))
        .await
        .is_err()
    {
        eprintln!("activity-log transport failed");
        std::process::exit(1);
    }
}
