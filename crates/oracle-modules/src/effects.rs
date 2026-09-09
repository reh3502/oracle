//! Durable host effects with an atomic generation-authority check at every actual send.
use crate::admission::Admission;
use async_trait::async_trait;
use oracle_core::{
    CoreService, Effect, EffectAdapter, Error, ErrorCode, GuildId, PolicyContext, Result,
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Opaque host-issued permit. Modules cannot construct a permit or replace its handle.
#[derive(Clone)]
pub struct DispatchPermit {
    gate: Arc<Admission>,
    handle: String,
}
impl DispatchPermit {
    pub(crate) fn new(gate: Arc<Admission>, handle: String) -> Self {
        Self { gate, handle }
    }
    /// The closure must perform the actual immediate, nonblocking send admission.
    /// It runs under the same lock as fencing; never return an unpolled async send here.
    pub fn dispatch<T>(&self, send: impl FnOnce() -> T) -> Result<T> {
        self.gate.dispatch(&self.handle, send)
    }
}
#[derive(Debug)]
pub enum Observation {
    Verified(Value),
    /// A definite rejection with no effect applied, so another attempt is safe.
    /// Ambiguous outcomes must be errors and are never automatically retried.
    RetryAfter(Duration),
}
/// A trusted host transport; module code never supplies this implementation.
#[async_trait]
pub trait SendTransport: Send + Sync {
    /// Await connection/rate-limit readiness without submitting the effect.
    async fn ready(&self) -> Result<()>;
    /// Submit synchronously without waiting or blocking. Returns an observation receipt.
    fn dispatch(&self, body: &Value) -> Result<Value>;
    async fn observe(&self, receipt: Value) -> Result<Observation>;
}
#[derive(Default)]
pub struct EchoTransport;
#[async_trait]
impl SendTransport for EchoTransport {
    async fn ready(&self) -> Result<()> {
        Ok(())
    }
    fn dispatch(&self, body: &Value) -> Result<Value> {
        Ok(body.clone())
    }
    async fn observe(&self, receipt: Value) -> Result<Observation> {
        Ok(Observation::Verified(receipt))
    }
}
struct Adapter {
    body: Value,
    permit: DispatchPermit,
    cancel: CancellationToken,
    transport: Arc<dyn SendTransport>,
}
#[async_trait]
impl EffectAdapter for Adapter {
    async fn apply(&self, _effect: &Effect) -> Result<Value> {
        for attempt in 0..3 {
            tokio::select! {biased;
                _=self.cancel.cancelled()=>return Err(Error::new(ErrorCode::Cancelled)),
                result=self.transport.ready()=>result?,
            }
            // Readiness can have waited through revocation. Every attempt checks the
            // opaque lease under the fence lock at the real send, including retries.
            let receipt = self.permit.dispatch(|| {
                if self.cancel.is_cancelled() {
                    return Err(Error::new(ErrorCode::Cancelled));
                }
                self.transport.dispatch(&self.body)
            })??;
            let observed = tokio::select! {biased;
                _=self.cancel.cancelled()=>return Err(Error::new(ErrorCode::Cancelled)),
                result=self.transport.observe(receipt)=>result?,
            };
            match observed {
                Observation::Verified(value) => return Ok(value),
                Observation::RetryAfter(delay) => {
                    if attempt == 2 {
                        return Err(Error::new(ErrorCode::UnknownOutcome));
                    }
                    if delay > Duration::from_secs(30) {
                        return Err(Error::new(ErrorCode::InvalidInput));
                    }
                    tokio::select! {biased;
                        _=self.cancel.cancelled()=>return Err(Error::new(ErrorCode::Cancelled)),
                        _=tokio::time::sleep(delay)=>{},
                    }
                }
            }
        }
        Err(Error::new(ErrorCode::UnknownOutcome))
    }
}
/// CoreService owns journaling and cancellation recovery. Do not wrap this future
/// in an outer cancellation select that could drop it before Unknown is recorded.
#[allow(clippy::too_many_arguments)] // Explicit policy, durable purpose, permit and transport inputs.
pub(crate) async fn execute(
    core: &CoreService,
    actor: &PolicyContext,
    guild: &GuildId,
    purpose: &str,
    body: Value,
    permit: DispatchPermit,
    cancel: CancellationToken,
    transport: Arc<dyn SendTransport>,
) -> Result<Value> {
    let adapter = Adapter {
        body,
        permit,
        cancel: cancel.clone(),
        transport,
    };
    let effect = core
        .execute(actor, guild, purpose, &adapter, &cancel)
        .await?;
    effect
        .receipt
        .ok_or_else(|| Error::new(ErrorCode::Integrity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{Authority, Lease};
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicUsize, Ordering},
    };
    fn permit() -> (Arc<Admission>, Lease, DispatchPermit) {
        let gate = Arc::new(Admission::default());
        let guild: GuildId = "123".parse().unwrap();
        gate.activate(guild.clone(), 1).unwrap();
        let lease = gate
            .admit(Authority {
                guild,
                epoch: 1,
                actor: PolicyContext::LocalOperator,
                capabilities: BTreeSet::new(),
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                depth: 0,
                configuration_revision: None,
                cancel: CancellationToken::new(),
            })
            .unwrap();
        let permit = DispatchPermit::new(gate.clone(), lease.handle.clone());
        (gate, lease, permit)
    }
    struct Retry {
        gate: Arc<Admission>,
        sent: AtomicUsize,
    }
    #[async_trait]
    impl SendTransport for Retry {
        async fn ready(&self) -> Result<()> {
            Ok(())
        }
        fn dispatch(&self, body: &Value) -> Result<Value> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(body.clone())
        }
        async fn observe(&self, _receipt: Value) -> Result<Observation> {
            self.gate.fence(None);
            Ok(Observation::RetryAfter(Duration::from_millis(1)))
        }
    }
    fn effect() -> Effect {
        Effect {
            id: oracle_core::EffectId::generate(),
            operation: oracle_core::OperationId::generate(),
            guild: "123".parse().unwrap(),
            purpose: "test".into(),
            state: oracle_core::EffectState::Sent,
            revision: 1,
            receipt: None,
        }
    }
    #[tokio::test]
    async fn retry_rechecks_fence_before_actual_second_dispatch() {
        let (gate, _lease, permit) = permit();
        let transport = Arc::new(Retry {
            gate,
            sent: AtomicUsize::new(0),
        });
        let adapter = Adapter {
            body: Value::Null,
            permit,
            cancel: CancellationToken::new(),
            transport: transport.clone(),
        };
        assert!(adapter.apply(&effect()).await.is_err());
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn revoked_permit_never_runs_send_closure() {
        let (gate, _lease, permit) = permit();
        gate.fence(None);
        assert!(permit.dispatch(|| panic!("revoked send ran")).is_err());
    }
}
