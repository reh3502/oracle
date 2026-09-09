//! Linux supervision for trusted modules. Policy and negotiation validation belong to the host.
//! Call `shutdown` before stopping the Tokio executor; Drop can only request cleanup.
#![forbid(unsafe_code)]
use nix::{
    errno::Errno,
    sys::{
        prctl::set_child_subreaper,
        signal::{Signal, killpg},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::Pid,
};
use oracle_rpc::{RpcError, RpcHandler, RpcPeer};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    os::fd::AsRawFd,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("process I/O failed")]
    Io(#[from] std::io::Error),
    #[error("Linux process setup failed")]
    Linux(#[from] Errno),
    #[error("module RPC failed")]
    Rpc(#[from] RpcError),
    #[error("executable digest mismatch")]
    DigestMismatch,
    #[error("runtime or process is unavailable")]
    Unavailable,
    #[error("process cleanup deadline exceeded")]
    CleanupTimeout,
    #[error("process supervisor failed")]
    Supervisor,
}
pub type Result<T> = std::result::Result<T, RuntimeError>;
#[derive(Debug, Clone, Serialize)]
pub struct StopReport {
    pub pid: u32,
    pub generation: u64,
    pub exit_code: Option<i32>,
    pub forced: bool,
    pub descendants_reaped: usize,
    pub stderr_bytes: u64,
    pub cleanup_error: Option<String>,
}
struct Instance {
    pid: u32,
    generation: u64,
    peer: RpcPeer,
    admitting: AtomicBool,
    stop: CancellationToken,
    stopped: watch::Sender<Option<StopReport>>,
    stop_lock: tokio::sync::Mutex<()>,
}
impl Instance {
    fn fence(&self) {
        self.admitting.store(false, Ordering::Release);
    }
}
struct State {
    closing: bool,
    instances: HashMap<u64, Arc<Instance>>,
    supervisors: Vec<JoinHandle<()>>,
}
struct RuntimeInner {
    next_generation: AtomicU64,
    state: Mutex<State>,
    shutdown_lock: tokio::sync::Mutex<()>,
}
pub struct ProcessRuntime {
    inner: Arc<RuntimeInner>,
}
impl ProcessRuntime {
    pub fn new() -> Result<Self> {
        set_child_subreaper(true)?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                next_generation: AtomicU64::new(1),
                state: Mutex::new(State {
                    closing: false,
                    instances: HashMap::new(),
                    supervisors: Vec::new(),
                }),
                shutdown_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }
    pub fn loaded_count(&self) -> usize {
        self.inner.state.lock().unwrap().instances.len()
    }
    /// Verify the open executable and execute through its inherited descriptor to prevent
    /// a path replacement between digest verification and exec. Installed bytes must be immutable.
    pub async fn spawn(
        &self,
        path: impl AsRef<Path>,
        expected_sha256: &str,
        hello: Value,
        handler: Arc<dyn RpcHandler>,
    ) -> Result<ModuleProcess> {
        let mut executable = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        // Keep the bounded hashing buffer off nested async orchestration stacks.
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = executable.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if format!("{:x}", hash.finalize()) != expected_sha256 {
            return Err(RuntimeError::DigestMismatch);
        }
        let instance = {
            // Admission, spawn and task ownership transfer are synchronous and atomic with shutdown.
            let mut state = self.inner.state.lock().unwrap();
            if state.closing {
                return Err(RuntimeError::Unavailable);
            }
            let generation = self
                .inner
                .next_generation
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
                .map_err(|_| RuntimeError::Unavailable)?;
            let mut child = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()?;
            let pid = child.id().expect("new child PID");
            let peer = RpcPeer::new(
                child.stdout.take().unwrap(),
                child.stdin.take().unwrap(),
                handler,
            );
            let stderr = child.stderr.take().unwrap();
            let (stopped, _) = watch::channel(None);
            let instance = Arc::new(Instance {
                pid,
                generation,
                peer,
                admitting: AtomicBool::new(true),
                stop: CancellationToken::new(),
                stopped,
                stop_lock: tokio::sync::Mutex::new(()),
            });
            state.instances.insert(generation, instance.clone());
            // Finished supervisors own no child/task resources; prune handles between spawns.
            state.supervisors.retain(|task| !task.is_finished());
            state.supervisors.push(tokio::spawn(supervise(
                child,
                stderr,
                instance.clone(),
                Arc::downgrade(&self.inner),
            )));
            instance
        };
        drop(executable);
        let mut guard = CleanupGuard(Some(instance.clone()));
        let response = instance
            .peer
            .call("hello", hello, Duration::from_secs(5))
            .await;
        let response = match response {
            Ok(response) if instance.admitting.load(Ordering::Acquire) => response,
            result => {
                instance.fence();
                instance.stop.cancel();
                wait_report(&instance, None).await?;
                return Err(result
                    .err()
                    .map(RuntimeError::Rpc)
                    .unwrap_or(RuntimeError::Unavailable));
            }
        };
        guard.0 = None;
        Ok(ModuleProcess {
            instance,
            hello: Arc::new(response),
        })
    }
    /// Close spawn admission permanently, request every cleanup, and join every supervisor.
    pub async fn shutdown(&self) -> Result<Vec<StopReport>> {
        let _serial = self.inner.shutdown_lock.lock().await;
        let instances: Vec<_> = {
            let mut state = self.inner.state.lock().unwrap();
            state.closing = true;
            state.instances.values().cloned().collect()
        };
        for instance in &instances {
            instance.fence();
            instance.stop.cancel();
        }
        let mut reports = Vec::new();
        for instance in instances {
            reports.push(wait_report(&instance, None).await?);
        }
        // Keep JoinHandles in owner while awaiting so cancellation of shutdown loses none.
        loop {
            let task = { self.inner.state.lock().unwrap().supervisors.pop() };
            let Some(task) = task else {
                break;
            };
            let mut guard = SupervisorGuard {
                owner: self.inner.clone(),
                task: Some(task),
            };
            let joined = guard.task.as_mut().unwrap().await;
            guard.task = None;
            joined.map_err(|_| RuntimeError::Supervisor)?;
        }
        Ok(reports)
    }
}
struct SupervisorGuard {
    owner: Arc<RuntimeInner>,
    task: Option<JoinHandle<()>>,
}
impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.owner.state.lock().unwrap().supervisors.push(task);
        }
    }
}
impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closing = true;
        for instance in state.instances.values() {
            instance.fence();
            instance.stop.cancel();
        }
    }
}
struct CleanupGuard(Option<Arc<Instance>>);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some(instance) = &self.0 {
            instance.fence();
            instance.stop.cancel();
        }
    }
}
#[derive(Clone)]
pub struct ModuleProcess {
    instance: Arc<Instance>,
    hello: Arc<Value>,
}
impl ModuleProcess {
    pub fn pid(&self) -> u32 {
        self.instance.pid
    }
    pub fn generation(&self) -> u64 {
        self.instance.generation
    }
    pub fn hello(&self) -> &Value {
        &self.hello
    }
    pub fn is_alive(&self) -> bool {
        self.instance.admitting.load(Ordering::Acquire)
    }
    pub async fn call(&self, method: &str, params: Value, deadline: Duration) -> Result<Value> {
        self.call_with_cancel(method, params, deadline, CancellationToken::new())
            .await
    }
    pub async fn call_with_cancel(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if !self.is_alive() {
            return Err(RuntimeError::Unavailable);
        }
        Ok(self
            .instance
            .peer
            .call_with_cancel(method, params, deadline, cancel)
            .await?)
    }
    pub async fn stop(&self, grace: Duration) -> Result<StopReport> {
        let _serial = self.instance.stop_lock.lock().await;
        if let Some(report) = self.instance.stopped.borrow().clone() {
            return Ok(report);
        }
        let _guard = CleanupGuard(Some(self.instance.clone()));
        self.instance.fence();
        let _ = self.instance.peer.call("shutdown", json!({}), grace).await;
        self.instance.stop.cancel();
        self.wait_stopped(Duration::from_secs(5)).await
    }
    /// Request cleanup through the runtime-owned supervisor without spawning a waiter.
    pub fn request_stop(&self) {
        self.instance.fence();
        self.instance.stop.cancel();
    }
    pub async fn force_stop(&self) -> Result<StopReport> {
        self.instance.fence();
        self.instance.stop.cancel();
        self.wait_stopped(Duration::from_secs(5)).await
    }
    pub async fn wait_stopped(&self, duration: Duration) -> Result<StopReport> {
        wait_report(&self.instance, Some(duration)).await
    }
}
async fn wait_report(instance: &Instance, duration: Option<Duration>) -> Result<StopReport> {
    let mut receiver = instance.stopped.subscribe();
    let wait = async {
        loop {
            if let Some(report) = receiver.borrow().clone() {
                return Ok(report);
            }
            receiver
                .changed()
                .await
                .map_err(|_| RuntimeError::Supervisor)?;
        }
    };
    match duration {
        Some(duration) => timeout(duration, wait)
            .await
            .map_err(|_| RuntimeError::CleanupTimeout)?,
        None => wait.await,
    }
}
fn signal_group(pid: u32, signal: Signal) -> std::result::Result<(), Errno> {
    match killpg(Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn observe_exit(pid: u32) -> std::result::Result<WaitStatus, Errno> {
    loop {
        match waitid(
            Id::Pid(Pid::from_raw(pid as i32)),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        ) {
            Ok(WaitStatus::StillAlive) | Err(Errno::EINTR) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            result => return result,
        }
    }
}

async fn supervise(
    mut child: Child,
    mut stderr: tokio::process::ChildStderr,
    instance: Arc<Instance>,
    owner: Weak<RuntimeInner>,
) {
    let stderr_bytes = Arc::new(AtomicU64::new(0));
    let count = stderr_bytes.clone();
    let mut stderr_task = tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    count.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    });
    // WNOWAIT observes exit without releasing the leader's PID. This keeps
    // the process-group identity reserved until the final group signal, even
    // when descendants hold stdout open after a crash.
    let first_exit = tokio::select! {
        exit = observe_exit(instance.pid) => Some(exit),
        _ = instance.stop.cancelled() => None,
        _ = instance.peer.wait_closed() => None,
    };
    instance.fence();
    instance.peer.close().await;
    let mut forced = false;
    let mut cleanup_errors: Vec<String> = Vec::new();
    let observation = match first_exit {
        Some(result) => Some(result),
        None => match timeout(Duration::from_millis(250), observe_exit(instance.pid)).await {
            Ok(result) => Some(result),
            Err(_) => {
                forced = true;
                None
            }
        },
    };
    // No leader reap occurs before this signal. Never signal a PID provided by
    // the guest or a group whose leader was unexpectedly reaped elsewhere.
    if let Some(Err(_error)) = observation {
        cleanup_errors.push("observe_child_failed".into());
    } else if let Err(_error) = signal_group(instance.pid, Signal::SIGKILL) {
        cleanup_errors.push("kill_process_group_failed".into());
    }
    // Public waiters have deadlines, but cleanup retains ownership until the OS
    // actually reports exit. A task stuck in a kernel wait is not unloaded.
    let exit: Option<ExitStatus> = match child.wait().await {
        Ok(status) => Some(status),
        Err(_error) => {
            cleanup_errors.push("direct_child_wait_failed".into());
            None
        }
    };
    let mut descendants_reaped = 0;
    loop {
        // Only this generation's adopted descendants, and only after Tokio
        // has finished waiting for the direct child. No waitpid(-1) races.
        match waitpid(
            Pid::from_raw(-(instance.pid as i32)),
            Some(WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::StillAlive) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => descendants_reaped += 1,
            Ok(_) => {}
            Err(Errno::ECHILD) => break,
            Err(Errno::EINTR) => continue,
            Err(_error) => {
                cleanup_errors.push("descendant_wait_failed".into());
                break;
            }
        }
    }
    if timeout(Duration::from_millis(250), &mut stderr_task)
        .await
        .is_err()
    {
        stderr_task.abort();
        let _ = stderr_task.await;
        cleanup_errors.push("stderr did not reach EOF after group cleanup".into());
    }
    let report = StopReport {
        pid: instance.pid,
        generation: instance.generation,
        exit_code: exit.and_then(|status| status.code()),
        forced,
        descendants_reaped,
        stderr_bytes: stderr_bytes.load(Ordering::Relaxed),
        cleanup_error: (!cleanup_errors.is_empty()).then(|| cleanup_errors.join("; ")),
    };
    if let Some(owner) = owner.upgrade() {
        owner
            .state
            .lock()
            .unwrap()
            .instances
            .remove(&instance.generation);
    }
    instance.stopped.send_replace(Some(report));
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_rpc::RpcHandler;
    struct Pending;
    #[async_trait::async_trait]
    impl RpcHandler for Pending {
        async fn handle(
            &self,
            _peer: RpcPeer,
            _method: String,
            _params: Value,
            cancellation: CancellationToken,
        ) -> std::result::Result<Value, RpcError> {
            cancellation.cancelled().await;
            Err(RpcError::Cancelled)
        }
    }
    fn digest(path: &str) -> String {
        format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
    }
    #[tokio::test]
    async fn digest_failure_never_spawns_and_shutdown_closes_admission() {
        let runtime = ProcessRuntime::new().unwrap();
        assert!(matches!(
            runtime
                .spawn("/bin/true", &"0".repeat(64), json!({}), Arc::new(Pending))
                .await,
            Err(RuntimeError::DigestMismatch)
        ));
        assert_eq!(runtime.loaded_count(), 0);
        assert!(runtime.shutdown().await.unwrap().is_empty());
        assert!(matches!(
            runtime
                .spawn(
                    "/bin/true",
                    &digest("/bin/true"),
                    json!({}),
                    Arc::new(Pending)
                )
                .await,
            Err(RuntimeError::Unavailable)
        ));
    }
    #[tokio::test]
    async fn failed_hello_reaps_child_before_returning() {
        let runtime = ProcessRuntime::new().unwrap();
        assert!(
            runtime
                .spawn(
                    "/bin/true",
                    &digest("/bin/true"),
                    json!({}),
                    Arc::new(Pending)
                )
                .await
                .is_err()
        );
        assert_eq!(runtime.loaded_count(), 0);
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn dropped_spawn_future_retains_cleanup_ownership() {
        let runtime = ProcessRuntime::new().unwrap();
        let expected = digest("/bin/cat");
        let mut spawn =
            Box::pin(runtime.spawn("/bin/cat", &expected, json!({}), Arc::new(Pending)));
        tokio::select! {biased;
            result=&mut spawn=>panic!("cat unexpectedly completed handshake: {}",result.is_ok()),
            _=tokio::time::sleep(Duration::from_millis(30))=>{}
        }
        let pid = runtime
            .inner
            .state
            .lock()
            .unwrap()
            .instances
            .values()
            .next()
            .unwrap()
            .pid;
        // Dropping the actual future (not just a pinned reference) requests stop.
        drop(spawn);
        timeout(Duration::from_secs(3), async {
            while runtime.loaded_count() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropped spawn must request cleanup without shutdown");
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        runtime.shutdown().await.unwrap();
        assert_eq!(runtime.loaded_count(), 0);
    }
    #[tokio::test]
    async fn exit_observation_preserves_pid_until_group_signal_and_reap() {
        let mut child = Command::new("/bin/true")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let observed = timeout(Duration::from_secs(2), observe_exit(pid))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(observed, WaitStatus::Exited(_, 0)));
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        assert!(
            status
                .lines()
                .any(|line| line.starts_with("State:") && line.contains('Z'))
        );
        signal_group(pid, Signal::SIGKILL).unwrap();
        assert!(child.wait().await.unwrap().success());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
