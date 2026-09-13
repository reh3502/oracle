//! Separate Oracle SDK executable; its only data input is the operator's snapshot directory.
mod presentation;
mod refresh_runtime;
use async_trait::async_trait;
use dandys_world_core::{
    query::{QueryEngine, QueryRequest},
    snapshot::{Snapshot, Store},
};
use oracle_contracts::ModuleManifest;
use oracle_module_sdk::{
    CallContext, Mode, Module, Result, RpcError, RuntimeConfiguration, TaskScope,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct LoadedCatalog {
    id: String,
    engine: QueryEngine,
    entity_count: usize,
    source_count: usize,
    quality: String,
    oldest_validation_ms: u64,
    latest_validation_ms: u64,
}
impl LoadedCatalog {
    fn new(snapshot: Snapshot) -> Self {
        Self {
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
            .ok_or_else(|| RpcError::Remote("Dandy's World snapshot is not loaded".into()))?;
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
        if mode != Mode::Normal {
            return Err(RpcError::Remote(
                "Dandy's World does not provide migrations".into(),
            ));
        }
        let directory = runtime.data_directory.filter(|p| p.is_absolute()).ok_or_else(|| RpcError::Remote("Configure an absolute Dandy's World runtime data_directory containing a published snapshot".into()))?;
        let outcome = Store::new(&directory).and_then(|s| s.load_recovering()).map_err(|_| RpcError::Remote("Cannot load Dandy's World snapshot; publish a validated catalog into its configured data directory".into()))?;
        *self.engine.write().unwrap() = Some(Arc::new(LoadedCatalog::new(outcome.snapshot)));
        *self.recovery.write().unwrap() = if outcome.recovered {
            "Recovered previous snapshot"
        } else {
            "Ready"
        };
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
                            *recovery.write().unwrap() =
                                "Snapshot reload failed; retaining loaded snapshot";
                        }
                    }
                }
                Ok(())
            })
            .map_err(|_| RpcError::Remote("Cannot start Dandy's World snapshot monitor".into()))?;
        Ok(())
    }
    async fn invoke(&self, _context: CallContext, operation: &str, input: Value) -> Result<Value> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RpcError::Remote("System clock is unavailable".into()))?
            .as_millis();
        self.query(
            operation,
            input,
            u64::try_from(now)
                .map_err(|_| RpcError::Remote("System clock is out of range".into()))?,
        )
    }
    async fn shutdown(&self) -> Result<()> {
        self.engine.write().unwrap().take();
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
