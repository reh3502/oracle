//! Native fixture for reminder callback authority; it never contacts Discord.
use async_trait::async_trait;
use oracle_contracts::{DocumentWrite, EffectiveConfiguration, GuildEvent, ModuleManifest};
use oracle_module_sdk::{CallContext, GuildContext, Module, Result};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Probe(Mutex<Option<EffectiveConfiguration>>);
fn outcome(value: Result<Value>) -> Value {
    match value {
        Ok(value) => json!({"accepted":true,"result":value}),
        Err(error) => json!({"accepted":false,"error":error.to_string()}),
    }
}
#[async_trait]
impl Module for Probe {
    fn manifest(&self) -> ModuleManifest {
        serde_json::from_str(include_str!("../manifest.json")).unwrap()
    }
    async fn invoke(&self, context: CallContext, _: &str, _: Value) -> Result<Value> {
        Ok(outcome(context.run_reminder("run", 1).await))
    }
    async fn event(&self, context: CallContext, event: GuildEvent) -> Result<Value> {
        let revision = if event.id == "stale" { 2 } else { 1 };
        let value = outcome(context.run_reminder("run", revision).await);
        context
            .document_batch(vec![DocumentWrite {
                collection: "results".into(),
                key: event.id,
                expected_revision: None,
                value: Some(value.clone()),
            }])
            .await?;
        Ok(value)
    }
    async fn prepare_configuration(&self, _: GuildContext, _: u64, _: Value) -> Result<()> {
        Ok(())
    }
    async fn apply_configuration(
        &self,
        _: GuildContext,
        revision: u64,
        values: Value,
    ) -> Result<()> {
        *self.0.lock().unwrap() = Some(EffectiveConfiguration { revision, values });
        Ok(())
    }
    async fn effective_configuration(
        &self,
        _: GuildContext,
    ) -> Result<Option<EffectiveConfiguration>> {
        Ok(self.0.lock().unwrap().clone())
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    oracle_module_sdk::serve_stdio(Arc::new(Probe::default()))
        .await
        .unwrap();
}
