//! Adversarial TEST-ONLY native fixture. Deliberately uses raw RPC, not SDK leases.
#![forbid(unsafe_code)]
use async_trait::async_trait;
use oracle_contracts::{DocumentWrite, ModuleDocument};
use oracle_rpc::{RpcError, RpcHandler, RpcPeer};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
fn denied() -> RpcError {
    RpcError::Remote("invalid authority probe request".into())
}
fn current(value: &mut Value, handle: &Value) {
    match value {
        Value::String(text) if text == "$current" => *value = handle.clone(),
        Value::Array(items) => {
            for value in items {
                current(value, handle)
            }
        }
        Value::Object(items) => {
            for value in items.values_mut() {
                current(value, handle)
            }
        }
        _ => {}
    }
}
struct Probe;
#[async_trait]
impl RpcHandler for Probe {
    async fn handle(
        &self,
        peer: RpcPeer,
        method: String,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method.as_str() {
            "hello" => {
                let manifest: Value = serde_json::from_str(if cfg!(feature = "v2") {
                    include_str!("../manifest-v2.json")
                } else {
                    include_str!("../manifest.json")
                })
                .unwrap();
                Ok(json!({"protocol_major":1,"protocol_minor":0,"manifest":manifest}))
            }
            "initialize" => Ok(json!({"initialized":true})),
            "activate" => Ok(json!({"activated":true})),
            "deactivate" => Ok(json!({"deactivated":true})),
            "quiesce" => Ok(json!({"quiesced":true})),
            "health" => Ok(json!({"test_only":true})),
            "shutdown" => Ok(json!({"acknowledged":true})),
            "operation.invoke" => {
                if params["operation"] != "probe" {
                    return Err(denied());
                }
                let handle = params.get("invocation").ok_or_else(denied)?.clone();
                let input = &params["input"];
                let mut output = if let Some(method) = input["method"].as_str() {
                    let mut arguments = input.get("params").cloned().ok_or_else(denied)?;
                    current(&mut arguments, &handle);
                    match peer.call(method, arguments, Duration::from_secs(3)).await {
                        Ok(result) => json!({"accepted":true,"result":result}),
                        Err(error) => json!({"accepted":false,"error":error.to_string()}),
                    }
                } else {
                    json!({"accepted":true})
                };
                if input["capture"] == true {
                    output["handle"] = handle;
                }
                Ok(output)
            }
            "migration.transform" => {
                if !cfg!(feature = "v2")
                    || params["operation"] != "authority_transform"
                    || params["from"] != 1
                    || params["to"] != 2
                {
                    return Err(denied());
                }
                let documents: Vec<ModuleDocument> =
                    serde_json::from_value(params["documents"].clone()).map_err(|_| denied())?;
                let mut writes = Vec::new();
                for document in documents {
                    let old = document
                        .value
                        .get("old_handle")
                        .cloned()
                        .ok_or_else(denied)?;
                    let storage=peer.call("host.document_batch",json!({"invocation":old,"writes":[{"collection":"probes","key":"migration-escape","expected_revision":null,"value":{"escaped":true}}]}),Duration::from_secs(3)).await;
                    let echo=peer.call("host.echo",json!({"invocation":old,"purpose":"migration-escape","body":{"escaped":true}}),Duration::from_secs(3)).await;
                    if storage.is_ok() || echo.is_ok() {
                        return Err(RpcError::Remote("migration authority escaped".into()));
                    }
                    let mut value = document.value;
                    value["migration_authority_denied"] = json!(true);
                    writes.push(DocumentWrite {
                        collection: document.collection,
                        key: document.key,
                        expected_revision: Some(document.revision),
                        value: Some(value),
                    });
                }
                serde_json::to_value(writes).map_err(|_| denied())
            }
            _ => Err(denied()),
        }
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), Arc::new(Probe));
    peer.wait_closed().await;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholder_preserves_unrelated_fields() {
        let mut value = json!({"invocation":"$current","forged":{"guild":"other","value":["$current","prefix$current"]}});
        current(&mut value, &json!("opaque"));
        assert_eq!(
            value,
            json!({"invocation":"opaque","forged":{"guild":"other","value":["opaque","prefix$current"]}})
        );
    }
}
