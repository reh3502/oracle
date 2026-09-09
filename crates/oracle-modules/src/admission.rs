//! Generation-bound invocation authority. Closing admission preserves existing leases;
//! fencing revokes them, including dispatches waking after a rate-limit wait.
use oracle_core::{Error, ErrorCode, GuildId, PolicyContext, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Authority {
    pub guild: GuildId,
    pub epoch: u64,
    pub actor: PolicyContext,
    pub capabilities: BTreeSet<String>,
    pub deadline: Instant,
    pub depth: u8,
    pub configuration_revision: Option<u64>,
    pub cancel: CancellationToken,
}
struct Activation {
    epoch: u64,
    accepting: bool,
}
#[derive(Default)]
struct State {
    closed: bool,
    activations: BTreeMap<GuildId, Activation>,
    leases: BTreeMap<String, Authority>,
}
#[derive(Default)]
pub(crate) struct Admission {
    state: Mutex<State>,
    changed: Notify,
}
pub(crate) struct Lease {
    pub handle: String,
    pub authority: Authority,
    gate: Arc<Admission>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        if let Some(authority) = state.leases.remove(&self.handle) {
            authority.cancel.cancel();
        }
        self.gate.changed.notify_waiters();
    }
}
fn unavailable() -> Error {
    Error::new(ErrorCode::ModuleUnavailable)
}
impl Admission {
    pub fn activate(&self, guild: GuildId, epoch: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.activations.contains_key(&guild) {
            return Err(unavailable());
        }
        state.activations.insert(
            guild,
            Activation {
                epoch,
                accepting: true,
            },
        );
        Ok(())
    }
    pub fn admit(self: &Arc<Self>, mut authority: Authority) -> Result<Lease> {
        let mut state = self.state.lock().unwrap();
        let active = state
            .activations
            .get(&authority.guild)
            .ok_or_else(unavailable)?;
        if state.closed
            || !active.accepting
            || authority.epoch != active.epoch
            || authority.deadline <= Instant::now()
            || authority.cancel.is_cancelled()
        {
            return Err(unavailable());
        }
        if state.leases.len() >= 64 {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        // A child token allows lease completion to revoke callbacks without cancelling
        // the parent invocation that delegated an inter-module call.
        authority.cancel = authority.cancel.child_token();
        let handle = uuid::Uuid::new_v4().to_string();
        state.leases.insert(handle.clone(), authority.clone());
        Ok(Lease {
            handle,
            authority,
            gate: self.clone(),
        })
    }
    pub fn authority(&self, handle: &str) -> Result<Authority> {
        let state = self.state.lock().unwrap();
        let authority = state.leases.get(handle).ok_or_else(unavailable)?;
        if authority.cancel.is_cancelled() || authority.deadline <= Instant::now() {
            return Err(unavailable());
        }
        Ok(authority.clone())
    }
    /// The closure must perform only the immediate nonblocking send admission.
    /// Never place an async wait between this check and the actual dispatch.
    pub fn dispatch<T>(&self, handle: &str, send: impl FnOnce() -> T) -> Result<T> {
        let state = self.state.lock().unwrap();
        let authority = state.leases.get(handle).ok_or_else(unavailable)?;
        if authority.cancel.is_cancelled() || authority.deadline <= Instant::now() {
            return Err(unavailable());
        }
        Ok(send())
    }
    pub fn is_active(&self, guild: &GuildId, epoch: u64) -> bool {
        let state = self.state.lock().unwrap();
        !state.closed
            && state
                .activations
                .get(guild)
                .is_some_and(|active| active.accepting && active.epoch == epoch)
    }
    pub fn in_flight(&self) -> usize {
        self.state.lock().unwrap().leases.len()
    }
    pub fn close(&self, guild: Option<&GuildId>) {
        let mut state = self.state.lock().unwrap();
        match guild {
            Some(guild) => {
                if let Some(active) = state.activations.get_mut(guild) {
                    active.accepting = false;
                }
            }
            None => {
                state.closed = true;
            }
        }
    }
    pub fn fence(&self, guild: Option<&GuildId>) {
        let mut state = self.state.lock().unwrap();
        state.leases.retain(|_, authority| {
            let affected = guild.is_none_or(|guild| guild == &authority.guild);
            if affected {
                authority.cancel.cancel();
            }
            !affected
        });
        match guild {
            Some(guild) => {
                state.activations.remove(guild);
            }
            None => {
                state.closed = true;
                state.activations.clear();
            }
        }
        self.changed.notify_waiters();
    }
    pub async fn drain(&self, guild: Option<&GuildId>, timeout: Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self
                    .state
                    .lock()
                    .unwrap()
                    .leases
                    .values()
                    .any(|a| guild.is_none_or(|g| g == &a.guild))
                {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn authority(guild: &str, epoch: u64) -> Authority {
        Authority {
            guild: guild.parse().unwrap(),
            epoch,
            actor: PolicyContext::LocalOperator,
            capabilities: BTreeSet::new(),
            deadline: Instant::now() + Duration::from_secs(5),
            depth: 0,
            configuration_revision: None,
            cancel: CancellationToken::new(),
        }
    }
    #[tokio::test]
    async fn quiescing_drains_existing_work_and_fences_stale_epoch() {
        let gate = Arc::new(Admission::default());
        let guild: GuildId = "123".parse().unwrap();
        gate.activate(guild.clone(), 1).unwrap();
        let lease = gate.admit(authority("123", 1)).unwrap();
        gate.close(Some(&guild));
        assert!(gate.admit(authority("123", 1)).is_err());
        assert_eq!(gate.dispatch(&lease.handle, || 42).unwrap(), 42);
        assert!(!gate.drain(Some(&guild), Duration::from_millis(1)).await);
        gate.fence(Some(&guild));
        assert!(lease.authority.cancel.is_cancelled());
        gate.activate(guild.clone(), 2).unwrap();
        assert!(gate.authority(&lease.handle).is_err());
        assert!(gate.admit(authority("123", 1)).is_err());
        assert!(gate.admit(authority("123", 2)).is_ok());
    }
    #[tokio::test]
    async fn queued_dispatch_after_fence_never_sends_and_other_guild_survives() {
        let gate = Arc::new(Admission::default());
        gate.activate("123".parse().unwrap(), 1).unwrap();
        gate.activate("456".parse().unwrap(), 2).unwrap();
        let lease = gate.admit(authority("123", 1)).unwrap();
        let other = gate.admit(authority("456", 2)).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (wake_tx, wake_rx) = tokio::sync::oneshot::channel();
        let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_gate = gate.clone();
        let task = tokio::spawn(async move {
            ready_tx.send(()).unwrap();
            wake_rx.await.unwrap(); // Models completion of a retry/rate-limit wait.
            task_gate.dispatch(&lease.handle, || sent_tx.send("sent").unwrap())
        });
        ready_rx.await.unwrap();
        gate.close(Some(&"123".parse().unwrap()));
        gate.fence(Some(&"123".parse().unwrap()));
        wake_tx.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(sent_rx.recv().await.is_none());
        assert!(gate.authority(&other.handle).is_ok());
        drop(other);
        assert!(gate.drain(None, Duration::from_secs(1)).await);
    }
}
