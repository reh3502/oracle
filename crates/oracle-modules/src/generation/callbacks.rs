//! Host-service callbacks authenticated by the generation's invocation leases.
use super::{Generation, decode, error, unavailable};
use crate::{admission::Authority, package::schema_validator};
use async_trait::async_trait;
use oracle_core::{DocumentWrite, ErrorCode, Result};
use oracle_rpc::{RpcError, RpcHandler, RpcPeer};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeSet, future::Future, sync::Weak};
use tokio_util::sync::CancellationToken;

fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| error(ErrorCode::InvalidInput))
}

impl Generation {
    fn collection(&self, name: &str) -> Result<&Value> {
        self.installed
            .package
            .manifest
            .collections
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.schema)
            .ok_or_else(|| error(ErrorCode::ForbiddenPermission))
    }
    fn storage_authority(&self, handle: &str) -> Result<Authority> {
        let authority = self.gate.authority(handle)?;
        if !authority.capabilities.contains("storage.own") {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        Ok(authority)
    }
    async fn bounded<T>(
        &self,
        handle: &str,
        authority: &Authority,
        cancel: &CancellationToken,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        // Re-check after all validation and immediately before polling host dispatch.
        self.gate.authority(handle)?;
        tokio::select! {biased;
            _=authority.cancel.cancelled()=>Err(error(ErrorCode::Cancelled)),
            _=cancel.cancelled()=>Err(error(ErrorCode::Cancelled)),
            _=tokio::time::sleep_until(authority.deadline)=>Err(unavailable()),
            result=future=>result,
        }
    }
    async fn callback(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if !self.normal {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        // Reject every callback from a public read lease before dispatch, including
        // future callback names. Even operator callers of member routes get no callbacks.
        let handle = params
            .get("invocation")
            .and_then(Value::as_str)
            .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?;
        if self.gate.authority(handle)?.audience == oracle_core::ModuleAudience::MemberRead {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        let authority = self.gate.authority(handle)?;
        if let Some(methods) = &authority.callback_methods
            && (!matches!(
                method,
                "host.document_get"
                    | "host.document_batch"
                    | "host.shared_card_enqueue"
                    | "host.shared_card_status"
            ) || !methods.contains(method))
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        match method {
            "host.shared_card_enqueue" | "host.shared_card_status" => {
                let request: SharedRequest = decode(params)?;
                let authority = self.gate.authority(&request.invocation)?;
                if !authority.capabilities.contains("shared_cards.publish")
                    || authority
                        .callback_methods
                        .as_ref()
                        .is_none_or(|methods| !methods.contains(method))
                {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                let source = self
                    .installed
                    .package
                    .manifest
                    .shared_cards
                    .as_ref()
                    .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?;
                if authority
                    .callback_collections
                    .as_ref()
                    .is_none_or(|collections| !collections.contains(&source.collection))
                {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                valid_key(&request.intent_key)?;
                let intent = if method == "host.shared_card_enqueue" {
                    let expected = request
                        .expected_revision
                        .filter(|r| *r > 0)
                        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
                    let document = self
                        .bounded(
                            &request.invocation,
                            &authority,
                            &cancel,
                            self.repository.document_get(
                                &self.installed.package.manifest.id,
                                &authority.guild,
                                &source.collection,
                                &request.intent_key,
                            ),
                        )
                        .await?
                        .ok_or_else(|| error(ErrorCode::NotFound))?;
                    if document.revision != expected {
                        return Err(error(ErrorCode::Conflict));
                    }
                    schema_validator(self.collection(&source.collection)?)?
                        .validate(&document.value)
                        .map_err(|_| error(ErrorCode::SchemaInvalid))?;
                    let intent = document
                        .value
                        .pointer(&source.pointer)
                        .ok_or_else(|| error(ErrorCode::InvalidInput))?
                        .clone();
                    validate_shared_intent(
                        &intent,
                        &request.intent_key,
                        &self.installed.package.manifest,
                    )?;
                    Some(intent)
                } else {
                    if request.expected_revision.is_some() {
                        return Err(error(ErrorCode::InvalidInput));
                    }
                    None
                };
                self.bounded(
                    &request.invocation,
                    &authority,
                    &cancel,
                    self.router.shared_card(
                        &self.installed.package.manifest.id,
                        &self.session,
                        self.number,
                        authority.clone(),
                        intent,
                        &request.intent_key,
                    ),
                )
                .await
            }
            "host.health" => {
                let request: HealthRequest = decode(params)?;
                let authority = self.gate.authority(&request.invocation)?;
                if !["config.own", "events.guild"]
                    .iter()
                    .all(|cap| authority.capabilities.contains(*cap))
                {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                let health = self
                    .bounded(
                        &request.invocation,
                        &authority,
                        &cancel,
                        self.router.host_health(
                            &self.installed.package.manifest.id,
                            &self.session,
                            self.number,
                            authority.clone(),
                        ),
                    )
                    .await?;
                self.gate.authority(&request.invocation)?;
                encode(health)
            }
            "host.notify" => {
                let request: Notification = decode(params)?;
                let authority = self.gate.authority(&request.invocation)?;
                if !authority.capabilities.contains("discord.notify") {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                if request.purpose.is_empty()
                    || request.purpose.len() > 128
                    || request.purpose.chars().any(char::is_control)
                    || request.destination.is_empty()
                    || request.destination.len() > 32
                    || !request.destination.bytes().all(|b| b.is_ascii_digit())
                    || request.text.is_empty()
                    || request.text.chars().count() > 1800
                {
                    return Err(error(ErrorCode::InvalidInput));
                }
                self.router
                    .notify(
                        &self.installed.package.manifest.id,
                        request.purpose,
                        request.destination,
                        request.text,
                        authority,
                        crate::DispatchPermit::new(self.gate.clone(), request.invocation),
                        cancel,
                    )
                    .await
            }
            "host.echo" => {
                let request: Echo = decode(params)?;
                let authority = self.gate.authority(&request.invocation)?;
                if !authority.capabilities.contains("host.echo") {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                if request.purpose.is_empty()
                    || request.purpose.len() > 128
                    || request.purpose.chars().any(char::is_control)
                {
                    return Err(error(ErrorCode::InvalidInput));
                }
                self.router
                    .echo(
                        &self.installed.package.manifest.id,
                        request.purpose,
                        request.body,
                        authority,
                        crate::DispatchPermit::new(self.gate.clone(), request.invocation),
                        cancel,
                    )
                    .await
            }
            "host.document_get" => {
                let request: Get = decode(params)?;
                let authority = self.storage_authority(&request.invocation)?;
                if authority
                    .callback_collections
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&request.collection))
                {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                self.collection(&request.collection)?;
                valid_key(&request.key)?;
                let result = self
                    .bounded(
                        &request.invocation,
                        &authority,
                        &cancel,
                        self.repository.document_get(
                            &self.installed.package.manifest.id,
                            &authority.guild,
                            &request.collection,
                            &request.key,
                        ),
                    )
                    .await?;
                if let Some(document) = &result {
                    schema_validator(self.collection(&request.collection)?)?
                        .validate(&document.value)
                        .map_err(|_| error(ErrorCode::SchemaInvalid))?;
                }
                encode(result)
            }
            "host.document_batch" => {
                let request: Batch = decode(params)?;
                let authority = self.storage_authority(&request.invocation)?;
                if request.writes.is_empty()
                    || request.writes.len() > 100
                    || serde_json::to_vec(&request.writes)
                        .map_err(|_| error(ErrorCode::InvalidInput))?
                        .len()
                        > 512 * 1024
                {
                    return Err(error(ErrorCode::QuotaExceeded));
                }
                let mut keys = BTreeSet::new();
                for write in &request.writes {
                    if authority
                        .callback_collections
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(&write.collection))
                    {
                        return Err(error(ErrorCode::ForbiddenPermission));
                    }
                    valid_key(&write.key)?;
                    let schema = self.collection(&write.collection)?;
                    if !keys.insert((&write.collection, &write.key))
                        || write.expected_revision == Some(0)
                        || (write.value.is_none() && write.expected_revision.is_none())
                    {
                        return Err(error(ErrorCode::InvalidInput));
                    }
                    if let Some(value) = &write.value {
                        if serde_json::to_vec(value)
                            .map_err(|_| error(ErrorCode::InvalidInput))?
                            .len()
                            > 64 * 1024
                        {
                            return Err(error(ErrorCode::QuotaExceeded));
                        }
                        schema_validator(schema)?
                            .validate(value)
                            .map_err(|_| error(ErrorCode::SchemaInvalid))?;
                    }
                }
                let result = self
                    .bounded(
                        &request.invocation,
                        &authority,
                        &cancel,
                        self.repository.document_batch(
                            &self.installed.package.manifest.id,
                            &authority.guild,
                            self.installed.package.manifest.data_version,
                            &request.writes,
                        ),
                    )
                    .await?;
                encode(result)
            }
            "host.contract_invoke" => {
                let request: Contract = decode(params)?;
                let authority = self.gate.authority(&request.invocation)?;
                if !authority.capabilities.contains("contracts.invoke") {
                    return Err(error(ErrorCode::ForbiddenPermission));
                }
                if !self
                    .installed
                    .package
                    .manifest
                    .consumes
                    .iter()
                    .any(|c| c.name == request.contract)
                {
                    return Err(error(ErrorCode::DependencyUnavailable));
                }
                let active = self
                    .activations
                    .lock()
                    .unwrap()
                    .get(&authority.guild)
                    .cloned()
                    .filter(|a| a.epoch == authority.epoch)
                    .ok_or_else(unavailable)?;
                let provider = active
                    .bindings
                    .get(&request.contract)
                    .ok_or_else(|| error(ErrorCode::DependencyUnavailable))?;
                self.bounded(
                    &request.invocation,
                    &authority,
                    &cancel,
                    self.router.invoke(
                        provider,
                        &request.contract,
                        request.input,
                        authority.clone(),
                    ),
                )
                .await
            }
            _ => Err(error(ErrorCode::ForbiddenPermission)),
        }
    }
}
fn valid_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 128 || key.contains('\0') {
        Err(error(ErrorCode::InvalidInput))
    } else {
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedRequest {
    invocation: String,
    intent_key: String,
    #[serde(default)]
    expected_revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedIntent {
    key: String,
    desired_revision: u64,
    destination: String,
    created_at: u64,
    #[serde(default)]
    repost_generation: u64,
    card: Value,
    actions: Vec<SharedAction>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedAction {
    name: String,
    label: String,
    operation: String,
    input: serde_json::Map<String, Value>,
}
fn validate_shared_intent(
    value: &Value,
    key: &str,
    manifest: &oracle_core::ModuleManifest,
) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|_| error(ErrorCode::InvalidInput))?
        .len()
        > 6 * 1024
    {
        return Err(error(ErrorCode::QuotaExceeded));
    }
    let intent: SharedIntent =
        serde_json::from_value(value.clone()).map_err(|_| error(ErrorCode::InvalidInput))?;
    let alias = |v: &str| {
        !v.is_empty()
            && v.len() <= 32
            && v.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    };
    if intent.key != key
        || intent.desired_revision == 0
        || !alias(&intent.destination)
        || !intent.card.is_object()
        || intent.actions.len() > 5
        || intent.created_at == 0
        || intent.repost_generation > intent.desired_revision
    {
        return Err(error(ErrorCode::InvalidInput));
    }
    let mut names = BTreeSet::new();
    for action in intent.actions {
        if !alias(&action.name)
            || !names.insert(action.name)
            || action.label.is_empty()
            || action.label.chars().count() > 80
            || action.label.chars().any(char::is_control)
            || action.input.iter().any(|(key, value)| {
                matches!(
                    key.as_str(),
                    "actor"
                        | "actor_id"
                        | "guild"
                        | "guild_id"
                        | "user_id"
                        | "interaction_id"
                        | "permissions"
                        | "invocation"
                ) || value.is_array()
                    || value.is_object()
            })
        {
            return Err(error(ErrorCode::InvalidInput));
        }
        let operation = manifest
            .operations
            .iter()
            .find(|op| {
                op.name == action.operation
                    && op.audience == oracle_core::ModuleAudience::MemberMutation
            })
            .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?;
        schema_validator(&operation.input_schema)?
            .validate(&Value::Object(action.input))
            .map_err(|_| error(ErrorCode::SchemaInvalid))?;
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    invocation: String,
    collection: String,
    key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    invocation: String,
    writes: Vec<DocumentWrite>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    invocation: String,
    contract: String,
    input: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notification {
    invocation: String,
    purpose: String,
    destination: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Echo {
    invocation: String,
    purpose: String,
    body: Value,
}
pub(super) struct Callbacks(pub(super) Weak<Generation>);
#[async_trait]
impl RpcHandler for Callbacks {
    async fn handle(
        &self,
        _peer: RpcPeer,
        method: String,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, RpcError> {
        let generation = self.0.upgrade().ok_or(RpcError::Closed)?;
        generation
            .callback(&method, params, cancel)
            .await
            .map_err(|error| RpcError::Remote(format!("{:?}", error.code)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthRequest {
    invocation: String,
}

#[cfg(test)]
mod shared_intent_tests {
    use super::*;
    #[test]
    fn terminal_projection_can_remove_every_public_control() {
        let manifest: oracle_core::ModuleManifest = serde_json::from_str(include_str!(
            "../../../../modules/dandys-world/manifest.json"
        ))
        .unwrap();
        crate::package::validate_manifest(&manifest).unwrap();
        let intent = serde_json::json!({"key":"ABCD2345","desired_revision":7,"destination":"runs","created_at":1,"card":{"title":"Completed run"},"actions":[]});
        validate_shared_intent(&intent, "ABCD2345", &manifest).unwrap();
    }
}
