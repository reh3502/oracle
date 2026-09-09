//! Host-owned asynchronous work with atomic admission and joined shutdown.
//!
//! Tasks must yield cooperatively. Tokio cannot forcibly interrupt blocking or
//! non-yielding native work; that work needs a separately supervised process.
//! Call `shutdown` before closing the runtime. Drop cancels and aborts work but
//! cannot asynchronously join it. Task error and panic payloads never enter the
//! summaries; the host must separately configure its process-wide panic hook.

use serde::Serialize;
use std::{
    collections::BTreeMap,
    fmt,
    future::{Future, poll_fn},
    sync::Mutex,
    task::Poll,
    time::Duration,
};
use tokio::{runtime::Handle, task::JoinSet};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TaskId(pub u64);

/// Deliberately contains no arbitrary error text. Detailed errors belong in a
/// separately reviewed, redacted operation diagnostic, never in task health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskError;
impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("host task failed")
    }
}
impl std::error::Error for TaskError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    NotAccepting,
    NoRuntime,
    IdsExhausted,
    InvalidTag,
}
impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAccepting => "host task scope is not accepting work",
            Self::NoRuntime => "host task spawning requires a Tokio runtime",
            Self::IdsExhausted => "host task identifiers exhausted",
            Self::InvalidTag => "host task tag must be a short static identifier",
        })
    }
}
impl std::error::Error for SpawnError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Lifecycle {
    Accepting,
    Quiescing,
    Closed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TaskCounts {
    pub spawned: u64,
    pub running: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub panicked: u64,
    pub aborted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskStats {
    pub lifecycle: Lifecycle,
    pub counts: TaskCounts,
    /// Tags must be static identifiers authored in code, never user input.
    pub by_tag: BTreeMap<&'static str, TaskCounts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShutdownSummary {
    pub stats: TaskStats,
    /// True if any shutdown attempt had to abort work after its grace period.
    pub forced: bool,
}

struct TaskMeta {
    tag: &'static str,
}

enum Outcome {
    Succeeded,
    Failed,
    Panicked,
    Aborted,
}
impl TaskCounts {
    fn admitted(&mut self) {
        self.spawned += 1;
        self.running += 1;
    }
    fn finished(&mut self, outcome: &Outcome) {
        self.running -= 1;
        match outcome {
            Outcome::Succeeded => self.succeeded += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Panicked => self.panicked += 1,
            Outcome::Aborted => self.aborted += 1,
        }
    }
}

struct State {
    lifecycle: Lifecycle,
    next_id: u64,
    tasks: JoinSet<Result<(), TaskError>>,
    metadata: BTreeMap<tokio::task::Id, TaskMeta>,
    counts: TaskCounts,
    by_tag: BTreeMap<&'static str, TaskCounts>,
    forced: bool,
}
impl State {
    fn record(
        &mut self,
        result: Result<(tokio::task::Id, Result<(), TaskError>), tokio::task::JoinError>,
    ) {
        let (id, outcome) = match result {
            Ok((id, Ok(()))) => (id, Outcome::Succeeded),
            Ok((id, Err(_))) => (id, Outcome::Failed),
            Err(error) => {
                let outcome = if error.is_panic() {
                    Outcome::Panicked
                } else {
                    Outcome::Aborted
                };
                (error.id(), outcome)
            }
        };
        // The admission lock installs metadata before any join can reap work.
        let metadata = self
            .metadata
            .remove(&id)
            .expect("every owned task has metadata");
        self.counts.finished(&outcome);
        self.by_tag
            .get_mut(metadata.tag)
            .expect("registered task tag")
            .finished(&outcome);
    }
    fn reap_finished(&mut self) {
        while let Some(result) = self.tasks.try_join_next_with_id() {
            self.record(result);
        }
    }
    fn snapshot(&self) -> TaskStats {
        TaskStats {
            lifecycle: self.lifecycle,
            counts: self.counts,
            by_tag: self.by_tag.clone(),
        }
    }
}

pub struct HostTasks {
    state: Mutex<State>,
    cancellation: CancellationToken,
    shutdown_lock: tokio::sync::Mutex<()>,
}
impl Default for HostTasks {
    fn default() -> Self {
        Self::new()
    }
}
impl HostTasks {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                lifecycle: Lifecycle::Accepting,
                next_id: 1,
                tasks: JoinSet::new(),
                metadata: BTreeMap::new(),
                counts: TaskCounts::default(),
                by_tag: BTreeMap::new(),
                forced: false,
            }),
            cancellation: CancellationToken::new(),
            shutdown_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Cancelling a returned token affects only its task/subtree, not siblings or
    /// the host. Tokens requested after shutdown are already cancelled.
    pub fn token(&self) -> CancellationToken {
        self.cancellation.child_token()
    }

    /// The static tag identifies a host work category; never embed credentials,
    /// payloads, guild IDs or other user-controlled/high-cardinality values.
    /// Rejected futures are dropped without being polled or spawned.
    pub fn spawn<F>(&self, tag: &'static str, future: F) -> Result<TaskId, SpawnError>
    where
        F: Future<Output = Result<(), TaskError>> + Send + 'static,
    {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != Lifecycle::Accepting {
            return Err(SpawnError::NotAccepting);
        }
        if tag.is_empty()
            || tag.len() > 64
            || !tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(SpawnError::InvalidTag);
        }
        let runtime = Handle::try_current().map_err(|_| SpawnError::NoRuntime)?;
        let next = state
            .next_id
            .checked_add(1)
            .ok_or(SpawnError::IdsExhausted)?;
        state.reap_finished();
        let id = TaskId(state.next_id);
        let abort = state.tasks.spawn_on(future, &runtime);
        state.metadata.insert(abort.id(), TaskMeta { tag });
        state.next_id = next;
        state.counts.admitted();
        state.by_tag.entry(tag).or_default().admitted();
        Ok(id)
    }

    pub fn stats(&self) -> TaskStats {
        let mut state = self.state.lock().unwrap();
        state.reap_finished();
        state.snapshot()
    }

    // Keep ownership inside state across each await. Cancelling this future
    // leaves JoinSet and every not-yet-observed result available to the next call.
    async fn join_one(&self) -> bool {
        poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();
            match state.tasks.poll_join_next_with_id(cx) {
                Poll::Ready(Some(result)) => {
                    state.record(result);
                    Poll::Ready(true)
                }
                Poll::Ready(None) => Poll::Ready(false),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    }

    /// Close admission, signal cooperative cancellation, wait at most `grace`,
    /// then abort and join all remaining yielding async tasks. Repeated and
    /// concurrent shutdowns return the same cumulative accounting. A cancelled
    /// shutdown can be resumed without losing ownership or task outcomes.
    /// Invoke from the supervisor, not a task owned by this scope: joining the
    /// caller itself cannot complete cooperatively.
    pub async fn shutdown(&self, grace: Duration) -> ShutdownSummary {
        let _serial = self.shutdown_lock.lock().await;
        {
            let mut state = self.state.lock().unwrap();
            state.reap_finished();
            if state.lifecycle == Lifecycle::Closed {
                return ShutdownSummary {
                    stats: state.snapshot(),
                    forced: state.forced,
                };
            }
            state.lifecycle = Lifecycle::Quiescing;
        }
        self.cancellation.cancel();
        if tokio::time::timeout(grace, async { while self.join_one().await {} })
            .await
            .is_err()
        {
            let mut state = self.state.lock().unwrap();
            state.reap_finished();
            if !state.tasks.is_empty() {
                state.forced = true;
                state.tasks.abort_all();
            }
        }
        while self.join_one().await {}
        let mut state = self.state.lock().unwrap();
        state.lifecycle = Lifecycle::Closed;
        ShutdownSummary {
            stats: state.snapshot(),
            forced: state.forced,
        }
    }
}
impl Drop for HostTasks {
    fn drop(&mut self) {
        self.cancellation.cancel();
        // Drop cannot await; abort ensures dropping the scope never detaches work.
        if let Ok(state) = self.state.get_mut() {
            state.tasks.abort_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::{Barrier, oneshot};

    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cooperative_cancellation_is_joined_and_child_cannot_cancel_siblings() {
        let scope = HostTasks::new();
        let cancelled_child = scope.token();
        let sibling = scope.token();
        cancelled_child.cancel();
        assert!(!sibling.is_cancelled());
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = Dropped(dropped.clone());
        let (started, ready) = oneshot::channel();
        scope
            .spawn("scheduler", async move {
                let _guard = guard;
                let _ = started.send(());
                sibling.cancelled().await;
                Ok(())
            })
            .unwrap();
        ready.await.unwrap();
        let summary = scope.shutdown(Duration::from_secs(1)).await;
        assert_eq!(summary.stats.lifecycle, Lifecycle::Closed);
        assert!(!summary.forced);
        assert_eq!(summary.stats.counts.succeeded, 1);
        assert_eq!(summary.stats.counts.running, 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(scope.token().is_cancelled());
        assert_eq!(scope.shutdown(Duration::ZERO).await, summary);
    }

    #[tokio::test]
    async fn pending_work_aborts_and_is_joined_with_failures_and_panics_accounted() {
        let scope = HostTasks::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = Dropped(dropped.clone());
        let (started, ready) = oneshot::channel();
        scope
            .spawn("pending", async move {
                let _guard = guard;
                let _ = started.send(());
                std::future::pending().await
            })
            .unwrap();
        scope.spawn("failure", async { Err(TaskError) }).unwrap();
        scope
            .spawn("panic", async {
                panic!("test panic payload must not enter task summary")
            })
            .unwrap();
        ready.await.unwrap();
        // Wait for failure/panic completion without relying on an elapsed sleep.
        while scope.stats().counts.failed + scope.stats().counts.panicked < 2 {
            tokio::task::yield_now().await;
        }
        let summary = scope.shutdown(Duration::ZERO).await;
        assert!(summary.forced);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            summary.stats.counts,
            TaskCounts {
                spawned: 3,
                running: 0,
                succeeded: 0,
                failed: 1,
                panicked: 1,
                aborted: 1
            }
        );
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("test panic payload")
        );
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_tasks_owned_for_resumption() {
        let scope = HostTasks::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = Dropped(dropped.clone());
        scope
            .spawn("pending", async move {
                let _guard = guard;
                std::future::pending().await
            })
            .unwrap();
        let mut shutdown = Box::pin(scope.shutdown(Duration::from_secs(60)));
        poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(scope.stats().lifecycle, Lifecycle::Quiescing);
        drop(shutdown);
        assert_eq!(
            scope.spawn("late", async { Ok(()) }),
            Err(SpawnError::NotAccepting)
        );
        let summary = scope.shutdown(Duration::ZERO).await;
        assert_eq!(summary.stats.counts.aborted, 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_shutdown_race_has_no_unowned_or_post_quiesce_admission() {
        let scope = Arc::new(HostTasks::new());
        let barrier = Arc::new(Barrier::new(17));
        let mut writers = JoinSet::new();
        for _ in 0..16 {
            let scope = scope.clone();
            let barrier = barrier.clone();
            writers.spawn(async move {
                barrier.wait().await;
                scope.spawn("race", async { Ok(()) })
            });
        }
        barrier.wait().await;
        let summary = scope.shutdown(Duration::from_secs(1)).await;
        let mut admitted = 0;
        while let Some(result) = writers.join_next().await {
            match result.unwrap() {
                Ok(_) => admitted += 1,
                Err(error) => assert_eq!(error, SpawnError::NotAccepting),
            }
        }
        assert_eq!(summary.stats.counts.spawned, admitted);
        assert_eq!(summary.stats.counts.running, 0);
        assert_eq!(summary.stats.counts.succeeded, admitted);
        let polled = Arc::new(AtomicBool::new(false));
        let probe = polled.clone();
        assert_eq!(
            scope.spawn("late", async move {
                probe.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Err(SpawnError::NotAccepting)
        );
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[test]
    fn spawning_without_runtime_is_a_typed_error() {
        let scope = HostTasks::new();
        assert_eq!(
            scope.spawn("valid", async { Ok(()) }),
            Err(SpawnError::NoRuntime)
        );
        assert_eq!(scope.stats().counts.spawned, 0);
    }
}
