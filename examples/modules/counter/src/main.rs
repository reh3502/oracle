//! Test-only native module: document CAS, a forward migration, and lifecycle probes.
use async_trait::async_trait;
use oracle_contracts::{DocumentWrite, ModuleDocument, ModuleManifest};
use oracle_module_sdk::{CallContext, GuildContext, Mode, Module, Result, RpcError, TaskScope};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
struct Counter;
fn invalid() -> RpcError {
    RpcError::Remote("invalid counter request".into())
}
const FIELD: &str = if cfg!(feature = "v2") {
    "total"
} else {
    "count"
};
#[async_trait]
impl Module for Counter {
    fn manifest(&self) -> ModuleManifest {
        serde_json::from_str(if cfg!(feature = "v2") {
            include_str!("../manifest-v2.json")
        } else {
            include_str!("../manifest.json")
        })
        .expect("checked-in module manifest")
    }
    async fn initialize(&self, mode: Mode, global: TaskScope) -> Result<()> {
        if mode == Mode::Normal {
            let cancel = global.cancellation();
            global
                .spawn("counter_global", async move {
                    cancel.cancelled().await;
                    Ok(())
                })
                .map_err(|_| invalid())?;
        }
        Ok(())
    }
    async fn activate(&self, context: GuildContext) -> Result<()> {
        let cancel = context.tasks.cancellation();
        context
            .tasks
            .spawn("counter_guild", async move {
                cancel.cancelled().await;
                Ok(())
            })
            .map_err(|_| invalid())?;
        Ok(())
    }
    async fn invoke(&self, context: CallContext, operation: &str, input: Value) -> Result<Value> {
        match operation {
            "echo" => {
                let purpose = input
                    .get("purpose")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                let body = input.get("body").ok_or_else(invalid)?.clone();
                context.echo(purpose, body).await
            }
            "get" | "increment" => {
                let previous = context.document_get("counters", "main").await?;
                let current = match &previous {
                    Some(doc) => doc
                        .value
                        .get(FIELD)
                        .and_then(Value::as_u64)
                        .ok_or_else(invalid)?,
                    None => 0,
                };
                let value = if operation == "increment" {
                    let amount = input.get("amount").and_then(Value::as_u64).unwrap_or(1);
                    let next = current.checked_add(amount).ok_or_else(invalid)?;
                    context
                        .document_batch(vec![DocumentWrite {
                            collection: "counters".into(),
                            key: "main".into(),
                            expected_revision: previous.as_ref().map(|d| d.revision),
                            value: Some(json!({FIELD:next})),
                        }])
                        .await?;
                    next
                } else {
                    current
                };
                Ok(json!({"value":value,"generation":context.generation(),"epoch":context.epoch()}))
            }
            "wait" => {
                context.cancellation().cancelled().await;
                Err(RpcError::Cancelled)
            }
            // Deliberately violates cooperative scheduling, proving the host's native
            // process deadline and SIGKILL path. Never use this pattern in a module.
            "uncooperative" => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            _ => Err(invalid()),
        }
    }
    async fn migrate(
        &self,
        operation: &str,
        from: u32,
        to: u32,
        documents: Vec<ModuleDocument>,
    ) -> Result<Vec<DocumentWrite>> {
        if !cfg!(feature = "v2") || operation != "count_to_total" || from != 1 || to != 2 {
            return Err(invalid());
        }
        documents
            .into_iter()
            .map(|doc| {
                if doc.collection != "counters" {
                    return Err(invalid());
                }
                let count = doc
                    .value
                    .get("count")
                    .and_then(Value::as_u64)
                    .ok_or_else(invalid)?;
                Ok(DocumentWrite {
                    collection: doc.collection,
                    key: doc.key,
                    expected_revision: Some(doc.revision),
                    value: Some(json!({"total":count})),
                })
            })
            .collect()
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    if oracle_module_sdk::serve_stdio(Arc::new(Counter))
        .await
        .is_err()
    {
        eprintln!("counter module transport failed");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_matches_compiled_data_version() {
        let manifest = Counter.manifest();
        assert_eq!(manifest.id.as_str(), "fixture.counter");
        assert_eq!(
            manifest.data_version,
            if cfg!(feature = "v2") { 2 } else { 1 }
        );
        assert_eq!(manifest.collections[0].schema["required"][0], FIELD);
        assert!(manifest.capabilities.contains(&"host.echo".to_owned()));
        let echo = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "echo")
            .unwrap();
        assert_eq!(echo.capabilities, vec!["host.echo"]);
        assert_eq!(echo.input_schema["required"], json!(["purpose", "body"]));
        assert_eq!(echo.input_schema["properties"]["purpose"]["maxLength"], 128);
        assert_eq!(echo.input_schema["additionalProperties"], false);
        assert_eq!(echo.output_schema, json!(true));
    }
    #[tokio::test]
    async fn migration_preserves_key_revision_and_value_or_rejects_profile() {
        let input = vec![ModuleDocument {
            collection: "counters".into(),
            key: "other".into(),
            value: json!({"count":42}),
            revision: 7,
        }];
        let result = Counter.migrate("count_to_total", 1, 2, input).await;
        if cfg!(feature = "v2") {
            let writes = result.unwrap();
            assert_eq!(writes[0].key, "other");
            assert_eq!(writes[0].expected_revision, Some(7));
            assert_eq!(writes[0].value, Some(json!({"total":42})));
        } else {
            assert!(result.is_err());
        }
    }
}
