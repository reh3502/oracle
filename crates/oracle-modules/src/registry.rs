//! A revision fence shared by registry publication and immediate command sends.
use oracle_core::{Error, ErrorCode, Result};
use oracle_process::ModuleProcess;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub(crate) struct RegistrySignal {
    revision: Mutex<u64>,
    changes: watch::Sender<u64>,
}
impl Default for RegistrySignal {
    fn default() -> Self {
        let (changes, _) = watch::channel(1);
        Self {
            revision: Mutex::new(1),
            changes,
        }
    }
}
impl RegistrySignal {
    // Global order: signal -> registry map -> activation/gate. No await or
    // externally supplied callback may mutate the registry while this is held.
    pub(crate) fn mutate<T>(&self, action: impl FnOnce() -> T) -> T {
        let mut revision = self.revision.lock().unwrap();
        let result = action();
        *revision = revision
            .checked_add(1)
            .expect("registry revision exhausted");
        self.changes.send_replace(*revision);
        result
    }
    pub(crate) fn snapshot<T>(&self, inspect: impl FnOnce(u64) -> T) -> T {
        let revision = self.revision.lock().unwrap();
        inspect(*revision)
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}
#[derive(Clone)]
pub struct RegistryDispatchPermit {
    pub(crate) signal: Arc<RegistrySignal>,
    pub(crate) revision: u64,
    pub(crate) processes: Vec<ModuleProcess>,
}
impl RegistryDispatchPermit {
    /// Only the immediate nonblocking send belongs in this closure. A lifecycle
    /// mutation cannot pass between this check and dispatch. Process liveness is
    /// also checked so a crash is fenced before its watcher finishes cleanup.
    pub fn dispatch<T>(&self, send: impl FnOnce() -> T) -> Result<T> {
        self.signal.snapshot(|revision| {
            if revision != self.revision || self.processes.iter().any(|p| !p.is_alive()) {
                return Err(Error::new(ErrorCode::ModuleUnavailable));
            }
            Ok(send())
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn mutation_invalidates_existing_send_permit_and_notifies_watchers() {
        let signal = Arc::new(RegistrySignal::default());
        let mut changes = signal.subscribe();
        let permit = RegistryDispatchPermit {
            signal: signal.clone(),
            revision: 1,
            processes: vec![],
        };
        let calls = AtomicUsize::new(0);
        signal.mutate(|| ());
        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow_and_update(), 2);
        assert!(
            permit
                .dispatch(|| calls.fetch_add(1, Ordering::SeqCst))
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let fresh = RegistryDispatchPermit {
            signal,
            revision: 2,
            processes: vec![],
        };
        fresh
            .dispatch(|| calls.fetch_add(1, Ordering::SeqCst))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
