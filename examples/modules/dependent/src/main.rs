//! A separate executable exercising a host-resolved contract dependency.
use async_trait::async_trait;
use oracle_contracts::ModuleManifest;
use oracle_module_sdk::{CallContext, Module, Result, RpcError};
use serde_json::Value;
use std::sync::Arc;
struct Dependent;
#[async_trait]
impl Module for Dependent {
    fn manifest(&self) -> ModuleManifest {
        serde_json::from_str(include_str!("../manifest.json")).expect("checked-in module manifest")
    }
    async fn invoke(&self, context: CallContext, operation: &str, input: Value) -> Result<Value> {
        match operation {
            "read_counter" => context.contract_invoke("counter/v1", input).await,
            _ => Err(RpcError::Remote("unknown dependent operation".into())),
        }
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    if oracle_module_sdk::serve_stdio(Arc::new(Dependent))
        .await
        .is_err()
    {
        eprintln!("dependent module transport failed");
        std::process::exit(1);
    }
}
