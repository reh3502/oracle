//! Separate Oracle SDK executable; its only data input is the operator's snapshot directory.
mod presentation;
mod refresh_runtime;
mod run_api;
use async_trait::async_trait;
use dandys_world_core::runs::{
    domain::EligibilitySnapshot,
    eligibility,
    storage::{Limits, RunService},
};
use dandys_world_core::{
    query::{QueryEngine, QueryRequest},
    snapshot::{Snapshot, Store},
};
use oracle_contracts::{
    DocumentWrite, EffectiveConfiguration, GuildEvent, GuildEventKind, GuildId, ModuleDocument,
    ModuleManifest,
};
use oracle_module_sdk::{
    CallContext, GuildContext, Mode, Module, Result, RpcError, RuntimeConfiguration, TaskScope,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RunConfiguration {
    limits: Limits,
    reminders: Option<dandys_world_core::runs::reminders::ReminderConfiguration>,
}
fn run_configuration(values: Value) -> Result<RunConfiguration> {
    let config: RunConfiguration = serde_json::from_value(values)
        .map_err(|_| RpcError::Remote("Invalid run configuration".into()))?;
    config
        .limits
        .validate()
        .map_err(|_| RpcError::Remote("Run limits may only lower the supported ceilings".into()))?;
    if let Some(reminders) = &config.reminders {
        reminders.validate().map_err(|_| {
            RpcError::Remote("Choose a valid DW role and IANA announcement time zone".into())
        })?;
    }
    Ok(config)
}
struct LoadedCatalog {
    id: String,
    eligibility: Option<EligibilitySnapshot>,
    engine: QueryEngine,
    entity_count: usize,
    source_count: usize,
    quality: String,
    oldest_validation_ms: u64,
    latest_validation_ms: u64,
}
impl LoadedCatalog {
    fn new(snapshot: Snapshot) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let eligibility = eligibility::from_catalog(&snapshot.id, &snapshot.data, now).ok();
        Self {
            eligibility,
            quality: {
                use dandys_world_core::model::EvidenceState;
                let facts = snapshot.data.entities.iter().flat_map(|e| &e.facts);
                let unknown = facts
                    .clone()
                    .filter(|f| {
                        matches!(f.state, EvidenceState::Unknown | EvidenceState::Unverified)
                    })
                    .count();
                let conflicting = facts
                    .filter(|f| f.state == EvidenceState::Conflicting)
                    .count();
                format!(
                    "Schema: {}. Entities by kind: {}. Unknown/unverified facts: {}; conflicting facts: {}; coverage warnings: {}; unresolved redirects: {}.",
                    snapshot.data.schema_version,
                    serde_json::to_string(&snapshot.data.coverage.entities_by_kind).unwrap(),
                    unknown,
                    conflicting,
                    snapshot.data.coverage.warnings.len(),
                    snapshot.data.coverage.unresolved_redirects.len()
                )
            },
            id: snapshot.id.clone(),
            entity_count: snapshot.data.entities.len(),
            source_count: snapshot.data.sources.len(),
            oldest_validation_ms: snapshot
                .data
                .sources
                .iter()
                .map(|s| s.validated_at_ms)
                .min()
                .expect("validated sources"),
            latest_validation_ms: snapshot
                .data
                .sources
                .iter()
                .map(|s| s.validated_at_ms)
                .max()
                .expect("validated sources"),
            engine: QueryEngine::new(snapshot.id, snapshot.data),
        }
    }
}
#[derive(Default)]
struct DwModule {
    publication_cursor: RwLock<BTreeMap<GuildId, String>>,
    run_configuration_lock: tokio::sync::RwLock<()>,
    configuration: RwLock<BTreeMap<GuildId, EffectiveConfiguration>>,
    // Metadata and query index are published and retained together.
    engine: Arc<RwLock<Option<Arc<LoadedCatalog>>>>,
    recovery: Arc<RwLock<&'static str>>,
    refresh: refresh_runtime::Diagnostics,
    disk_bytes: Arc<AtomicU64>,
    queries: AtomicU64,
    rejected: AtomicU64,
    query_micros: AtomicU64,
}
fn request(operation: &str, input: Value) -> Result<QueryRequest> {
    if ![
        "search", "lookup", "compare", "ask", "sources", "status", "health",
    ]
    .contains(&operation)
    {
        return Err(RpcError::Remote("Unknown Dandy's World operation".into()));
    }
    let mut input = input
        .as_object()
        .cloned()
        .ok_or_else(|| RpcError::Protocol("Expected typed option object".into()))?;
    if input.contains_key("op") {
        return Err(RpcError::Protocol("Operation is host-selected".into()));
    }
    input.insert(
        "op".into(),
        json!(if operation == "health" {
            "status"
        } else {
            operation
        }),
    );
    serde_json::from_value(Value::Object(input))
        .map_err(|_| RpcError::Protocol("Invalid Dandy's World options".into()))
}
impl DwModule {
    fn query(&self, operation: &str, input: Value, now: u64) -> Result<Value> {
        let started = Instant::now();
        let result = self.query_inner(operation, input, now);
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.query_micros.fetch_add(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if result.is_err() {
            self.rejected.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
    fn query_inner(&self, operation: &str, input: Value, now: u64) -> Result<Value> {
        let request = request(operation, input)?;
        let engine = self
            .engine
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| RpcError::Remote("Wiki answers are unavailable; publish a validated snapshot. Existing runs remain available".into()))?;
        let mut response = engine
            .engine
            .execute(request.clone(), now)
            .map_err(|e| RpcError::Remote(e.to_string()))?;
        if matches!(request, QueryRequest::Status {}) {
            let checks = if operation == "health" {
                format!(
                    "Source validation timestamps (Unix milliseconds): oldest {}; latest {}. Snapshot monitor: {}.",
                    engine.oldest_validation_ms,
                    engine.latest_validation_ms,
                    *self.recovery.read().unwrap()
                )
            } else if engine.latest_validation_ms > now {
                "Some source checks are dated in the future; check the host clock.".into()
            } else {
                format!(
                    "Wiki checks: oldest {} minutes ago; newest {} minutes ago.",
                    (now - engine.oldest_validation_ms) / 60_000,
                    (now - engine.latest_validation_ms) / 60_000
                )
            };
            let refresh = self.refresh.read().unwrap();
            let refresh = if refresh.is_empty() {
                "Disabled"
            } else if operation == "health" {
                refresh.as_str()
            } else {
                refresh.split(':').next().unwrap_or("Unavailable")
            };
            let diagnostics = if operation == "health" {
                format!(
                    "\n{} Last observed disk bytes: {}. Module queries: {}; rejected: {}; mean query time: {} microseconds.",
                    engine.quality,
                    self.disk_bytes.load(Ordering::Relaxed),
                    self.queries.load(Ordering::Relaxed),
                    self.rejected.load(Ordering::Relaxed),
                    self.query_micros.load(Ordering::Relaxed)
                        / self.queries.load(Ordering::Relaxed).max(1)
                )
            } else {
                String::new()
            };
            response.message = format!(
                "Loaded wiki snapshot: {}\nEntities: {}; source pages: {}.\n{}\nNetwork refresh: {}.{}",
                response.snapshot_id,
                engine.entity_count,
                engine.source_count,
                checks,
                refresh,
                diagnostics,
            );
            if operation == "health" {
                return Ok(json!({"reply":{"text":response.message,"citations":[]}}));
            }
            let freshness = if engine.oldest_validation_ms > now
                || engine.latest_validation_ms > now
            {
                "Some wiki checks have an incorrect date. Those answers may be unavailable."
                    .to_owned()
            } else {
                let minutes = (now - engine.oldest_validation_ms) / 60_000;
                if minutes < 60 {
                    "The wiki was checked within the last hour.".to_owned()
                } else if minutes < 24 * 60 {
                    format!(
                        "The oldest wiki check was {} {} ago.",
                        minutes / 60,
                        if minutes / 60 == 1 { "hour" } else { "hours" }
                    )
                } else {
                    format!(
                        "Some wiki information was last checked {} {} ago. Older answers include a warning.",
                        minutes / (24 * 60),
                        if minutes / (24 * 60) == 1 {
                            "day"
                        } else {
                            "days"
                        }
                    )
                }
            };
            response.message = format!(
                "Ask about Toons, Twisteds, floors, items and more.\n\n{freshness}\nAnswers link back to the wiki so you can read more."
            );
        }
        Ok(json!({"reply":presentation::render(&request, &response)}))
    }
}
#[async_trait]
impl Module for DwModule {
    fn manifest(&self) -> ModuleManifest {
        serde_json::from_str(include_str!("../manifest.json")).expect("compiled DW manifest")
    }
    async fn initialize_with_runtime(
        &self,
        mode: Mode,
        global: TaskScope,
        runtime: RuntimeConfiguration,
    ) -> Result<()> {
        if mode == Mode::Migration {
            return Ok(());
        }
        let directory = runtime
            .data_directory
            .filter(|p| p.is_absolute() && p.is_dir())
            .ok_or_else(|| {
                RpcError::Remote(
                    "Configure an existing absolute Dandy's World runtime data_directory".into(),
                )
            })?;
        match Store::new(&directory).and_then(|s| s.load_recovering()) {
            Ok(outcome) => {
                *self.engine.write().unwrap() =
                    Some(Arc::new(LoadedCatalog::new(outcome.snapshot)));
                *self.recovery.write().unwrap() = if outcome.recovered {
                    "Recovered previous snapshot"
                } else {
                    "Ready"
                };
            }
            Err(_) => {
                *self.engine.write().unwrap() = None;
                *self.recovery.write().unwrap() = "Runs available; publish a validated wiki snapshot to restore wiki answers and new setup";
            }
        }
        self.disk_bytes.store(
            Store::new(&directory)
                .and_then(|s| s.disk_usage())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        refresh_runtime::start(directory.clone(), &global, self.refresh.clone())?;
        let engine = self.engine.clone();
        let recovery = self.recovery.clone();
        let disk_bytes = self.disk_bytes.clone();
        let cancel = global.cancellation();
        global
            .spawn("dw-snapshot-monitor", async move {
                let mut ticks = 0u64;
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    let current = engine
                        .read()
                        .unwrap()
                        .as_ref()
                        .map(|e| e.id.clone())
                        .unwrap_or_default();
                    let root = directory.clone();
                    ticks = ticks.wrapping_add(1);
                    let disk_bytes = disk_bytes.clone();
                    // Always join blocking work, even during cancellation. No parsing
                    // or index construction runs on the query executor.
                    let loaded = tokio::task::spawn_blocking(move || {
                        let store = Store::new(root)?;
                        if ticks.is_multiple_of(60)
                            && let Ok(bytes) = store.disk_usage()
                        {
                            disk_bytes.store(bytes, Ordering::Relaxed);
                        }
                        store
                            .load_if_changed(&current)
                            .map(|s| s.map(LoadedCatalog::new))
                    })
                    .await;
                    if cancel.is_cancelled() {
                        break;
                    }
                    match loaded {
                        Ok(Ok(Some(next))) => {
                            *engine.write().unwrap() = Some(Arc::new(next));
                            *recovery.write().unwrap() = "Ready";
                        }
                        Ok(Ok(None)) => {}
                        _ => {
                            *recovery.write().unwrap() = if engine.read().unwrap().is_some() {
                                "Snapshot reload failed; retaining loaded snapshot"
                            }else{ "Runs available; publish a validated wiki snapshot to restore wiki answers and new setup" };
                        }
                    }
                }
                Ok(())
            })
            .map_err(|_| RpcError::Remote("Cannot start Dandy's World snapshot monitor".into()))?;
        Ok(())
    }
    async fn invoke(&self, context: CallContext, operation: &str, input: Value) -> Result<Value> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RpcError::Remote("System clock is unavailable".into()))?
            .as_millis();
        let now = u64::try_from(now)
            .map_err(|_| RpcError::Remote("System clock is out of range".into()))?;
        if operation.starts_with("run_") {
            let _configuration = self.run_configuration_lock.read().await;
            let current = self
                .engine
                .read()
                .unwrap()
                .as_ref()
                .and_then(|c| c.eligibility.clone());
            let values = self
                .configuration
                .read()
                .unwrap()
                .get(context.guild())
                .map(|c| c.values.clone())
                .unwrap_or_else(|| json!({}));
            let limits = run_configuration(values)?.limits;
            return run_api::invoke(context, operation, input, current, limits, now).await;
        }
        self.query(operation, input, now)
    }
    async fn deactivate(&self, guild: &GuildId, _epoch: u64) -> Result<()> {
        self.configuration.write().unwrap().remove(guild);
        self.publication_cursor.write().unwrap().remove(guild);
        Ok(())
    }
    async fn prepare_configuration(
        &self,
        _context: GuildContext,
        _revision: u64,
        values: Value,
    ) -> Result<()> {
        run_configuration(values)?;
        Ok(())
    }
    async fn apply_configuration(
        &self,
        context: GuildContext,
        revision: u64,
        values: Value,
    ) -> Result<()> {
        let _configuration = self.run_configuration_lock.write().await;
        self.prepare_configuration(context.clone(), revision, values.clone())
            .await?;
        self.configuration.write().unwrap().insert(
            context.guild.clone(),
            EffectiveConfiguration { revision, values },
        );
        Ok(())
    }
    async fn effective_configuration(
        &self,
        context: GuildContext,
    ) -> Result<Option<EffectiveConfiguration>> {
        Ok(self
            .configuration
            .read()
            .unwrap()
            .get(&context.guild)
            .cloned())
    }
    async fn event(&self, context: CallContext, event: GuildEvent) -> Result<Value> {
        if event.kind != GuildEventKind::Maintenance || context.actor().is_some() {
            return Err(RpcError::Remote("Unsupported run event".into()));
        }
        // Wall-clock maintenance is independent of snapshot availability and event timestamps.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RpcError::Remote("System clock is unavailable".into()))?
            .as_millis() as u64;
        let service = RunService::new(context.clone());
        let values = self
            .configuration
            .read()
            .unwrap()
            .get(context.guild())
            .map(|c| c.values.clone())
            .unwrap_or_else(|| json!({}));
        if let Some(config) = run_configuration(values)?.reminders {
            let mut work = tokio::task::JoinSet::new();
            let slots = Arc::new(tokio::sync::Semaphore::new(4));
            for id in service
                .reminder_candidates()
                .await
                .map_err(|e| RpcError::Remote(e.to_string()))?
            {
                let context = context.clone();
                let config = config.clone();
                let slots = slots.clone();
                work.spawn(async move {
                    let _slot = slots
                        .acquire_owned()
                        .await
                        .map_err(|_| RpcError::Cancelled)?;
                    let service = RunService::new(context.clone());
                    let Some(revision) = service
                        .prepare_reminder(&id, &config, now)
                        .await
                        .map_err(|e| RpcError::Remote(e.to_string()))?
                    else {
                        return Ok::<_, RpcError>(());
                    };
                    let evidence = context.run_reminder(&id, revision).await?;
                    let settled_now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| RpcError::Remote("System clock is unavailable".into()))?
                        .as_millis() as u64;
                    service
                        .settle_attendance(&id, &evidence, settled_now)
                        .await
                        .map_err(|e| RpcError::Remote(e.to_string()))?;
                    Ok(())
                });
            }
            while let Some(result) = work.join_next().await {
                if !matches!(result, Ok(Ok(()))) {
                    eprintln!("Run reminder processing deferred; saved run retained");
                }
            }
        }
        if let Ok(pending) = service.pending_projections().await {
            let cursor = self
                .publication_cursor
                .read()
                .unwrap()
                .get(context.guild())
                .cloned()
                .unwrap_or_default();
            let ordered: Vec<_> = pending
                .iter()
                .filter(|id| *id > &cursor)
                .chain(pending.iter().filter(|id| *id <= &cursor))
                .take(16)
                .cloned()
                .collect();
            for id in ordered {
                let _ = run_api::reconcile_one(&context, &service, &id).await;
                self.publication_cursor
                    .write()
                    .unwrap()
                    .insert(context.guild().clone(), id);
            }
        }
        serde_json::to_value(
            service
                .cleanup(now, 32)
                .await
                .map_err(|e| RpcError::Remote(e.to_string()))?,
        )
        .map_err(|_| RpcError::Remote("Invalid cleanup result".into()))
    }
    async fn migrate(
        &self,
        operation: &str,
        from: u32,
        to: u32,
        documents: Vec<ModuleDocument>,
    ) -> Result<Vec<DocumentWrite>> {
        if operation == "migrate_run_reminders" && from == 4 && to == 5 {
            return documents
                .into_iter()
                .map(|document| {
                    dandys_world_core::runs::storage::migrate_v4_document(document)
                        .map_err(|error| RpcError::Remote(error.to_string()))
                })
                .collect();
        }
        if operation == "migrate_run_schedule" && from == 3 && to == 4 {
            return documents
                .into_iter()
                .map(|document| {
                    dandys_world_core::runs::storage::migrate_v3_document(document)
                        .map_err(|error| RpcError::Remote(error.to_string()))
                })
                .collect();
        }
        // Version one had no module document collections. Never reinterpret unexpected data.
        if operation == "migrate_run_cards" && from == 2 && to == 3 {
            return documents
                .into_iter()
                .map(|document| {
                    dandys_world_core::runs::storage::migrate_v2_document(document)
                        .map_err(|error| RpcError::Remote(error.to_string()))
                })
                .collect();
        }
        if operation != "migrate_runs" || from != 1 || to != 2 || !documents.is_empty() {
            return Err(RpcError::Remote(
                "Unsupported Dandy's World migration".into(),
            ));
        }
        Ok(vec![])
    }
    async fn shutdown(&self) -> Result<()> {
        self.engine.write().unwrap().take();
        self.configuration.write().unwrap().clear();
        Ok(())
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if oracle_module_sdk::serve_stdio(Arc::new(DwModule::default()))
        .await
        .is_err()
    {
        eprintln!("Dandy's World module stopped after an RPC or lifecycle error");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "../tests/module/mod.rs"]
mod tests;
