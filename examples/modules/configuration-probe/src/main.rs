//! Test fixture only: exercises configuration hooks in a real separate process.
use async_trait::async_trait;
use oracle_contracts::{EffectiveConfiguration, GuildId, ModuleManifest};
use oracle_module_sdk::{CallContext, GuildContext, Module, Result, RpcError};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Default)]
struct Probe(Mutex<BTreeMap<GuildId, EffectiveConfiguration>>);
#[async_trait]
impl Module for Probe {
    fn manifest(&self) -> ModuleManifest {
        serde_json::from_str(include_str!("../manifest.json")).unwrap()
    }
    async fn invoke(&self, _: CallContext, _: &str, _: Value) -> Result<Value> {
        Err(RpcError::Remote("no operations".into()))
    }
    async fn prepare_configuration(&self, _: GuildContext, _: u64, values: Value) -> Result<()> {
        if let Some(wait) = values["wait_ms"].as_u64() {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        if values["reject"] == true {
            return Err(RpcError::Remote("rejected".into()));
        }
        Ok(())
    }
    async fn apply_configuration(
        &self,
        context: GuildContext,
        revision: u64,
        values: Value,
    ) -> Result<()> {
        self.0.lock().unwrap().insert(
            context.guild,
            EffectiveConfiguration {
                revision,
                values: values.clone(),
            },
        );
        if values["crash"] == true {
            let first = values["crash_marker"].as_str().is_none_or(|path| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .is_ok()
            });
            if first {
                std::process::exit(73);
            }
        }
        Ok(())
    }
    async fn effective_configuration(
        &self,
        context: GuildContext,
    ) -> Result<Option<EffectiveConfiguration>> {
        let mut value = self.0.lock().unwrap().get(&context.guild).cloned();
        if let Some(effective) = &mut value
            && effective.values["mismatch"] == true
        {
            effective.revision += 1;
        }
        Ok(value)
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    oracle_module_sdk::serve_stdio(Arc::new(Probe::default()))
        .await
        .unwrap();
}
