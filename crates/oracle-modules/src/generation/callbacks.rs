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
        match method {
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
