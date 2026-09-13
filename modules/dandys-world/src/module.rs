//! Separate Oracle SDK executable; its only data input is the operator's snapshot directory.
mod presentation;
use async_trait::async_trait;
use dandys_world_core::{
    query::{QueryEngine, QueryRequest},
    snapshot::Store,
};
use oracle_contracts::ModuleManifest;
use oracle_module_sdk::{
    CallContext, Mode, Module, Result, RpcError, RuntimeConfiguration, TaskScope,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

struct LoadedCatalog {
    engine: QueryEngine,
    entity_count: usize,
    source_count: usize,
    oldest_validation_ms: u64,
    latest_validation_ms: u64,
}
#[derive(Default)]
struct DwModule {
    // Metadata and query index are published and retained together.
    engine: RwLock<Option<Arc<LoadedCatalog>>>,
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
                    "Source validation timestamps (Unix milliseconds): oldest {}; latest {}.",
                    engine.oldest_validation_ms, engine.latest_validation_ms
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
            response.message = format!(
                "Loaded wiki snapshot: {}\nEntities: {}; source pages: {}.\n{}\nNetwork refresh: disabled.",
                response.snapshot_id, engine.entity_count, engine.source_count, checks,
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
        _global: TaskScope,
        runtime: RuntimeConfiguration,
    ) -> Result<()> {
        if mode != Mode::Normal {
            return Err(RpcError::Remote(
                "Dandy's World does not provide migrations".into(),
            ));
        }
        let directory = runtime.data_directory.filter(|p| p.is_absolute()).ok_or_else(|| RpcError::Remote("Configure an absolute Dandy's World runtime data_directory containing a published snapshot".into()))?;
        let snapshot = Store::new(directory).and_then(|s| s.load()).map_err(|_| RpcError::Remote("Cannot load Dandy's World snapshot; publish a validated catalog into its configured data directory".into()))?;
        let engine = Arc::new(LoadedCatalog {
            entity_count: snapshot.data.entities.len(),
            source_count: snapshot.data.sources.len(),
            // Store validation requires at least one source with a positive timestamp.
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
        });
        *self.engine.write().unwrap() = Some(engine);
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
