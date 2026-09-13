//! TEST-ONLY adversarial native module. Intentionally bypasses SDK callback restrictions.
#![forbid(unsafe_code)]
use async_trait::async_trait;
use oracle_rpc::{RpcError, RpcHandler, RpcPeer};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
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
            "hello" => Ok(
                json!({"protocol_major":1,"protocol_minor":1,"manifest":serde_json::from_str::<Value>(include_str!("../manifest.json")).unwrap()}),
            ),
            "initialize" => Ok(json!({"initialized":true})),
            "activate" => Ok(json!({"activated":true})),
            "deactivate" => Ok(json!({"deactivated":true})),
            "quiesce" => Ok(json!({"quiesced":true})),
            "health" => Ok(json!({"test_only":true})),
            "shutdown" => Ok(json!({"acknowledged":true})),
            "operation.invoke"
                if matches!(
                    params["operation"].as_str(),
                    Some("member_probe" | "operator_probe")
                ) =>
            {
                let input = &params["input"];
                let callback = input["method"]
                    .as_str()
                    .ok_or_else(|| RpcError::Protocol("missing callback method".into()))?;
                let mut args = input["params"]
                    .as_object()
                    .cloned()
                    .ok_or_else(|| RpcError::Protocol("missing callback args".into()))?;
                // Use the real active invocation lease, not a forged/expired token.
                args.insert("invocation".into(), params["invocation"].clone());
                Ok(
                    match peer
                        .call(callback, Value::Object(args), Duration::from_secs(3))
                        .await
                    {
                        Ok(result) => json!({"accepted":true,"result":result}),
                        Err(error) => json!({"accepted":false,"error":error.to_string()}),
                    },
                )
            }
            _ => Err(RpcError::Remote("unknown test fixture method".into())),
        }
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), Arc::new(Probe));
    peer.wait_closed().await;
}
