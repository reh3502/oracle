//! Trusted Linux process supervision. All authority lives in the host lease table.
use crate::rpc::{RpcError, RpcHandler, RpcPeer};
use async_trait::async_trait;
use nix::{
    errno::Errno,
    sys::{
        prctl::set_child_subreaper,
        signal::{Signal, killpg},
        wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid},
    },
    unistd::Pid,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Notify, watch},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Linux process setup: {0}")]
    Linux(#[from] Errno),
    #[error("RPC: {0}")]
    Rpc(#[from] RpcError),
    #[error("incompatible fixture handshake: {0}")]
    Handshake(String),
    #[error("module generation is unavailable")]
    Unavailable,
    #[error("process cleanup did not finish within its deadline")]
    CleanupTimeout,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub identity: String,
    pub build: String,
}

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

struct Lease {
    scope: String,
    expires: Instant,
}
struct Gate {
    admitting: bool,
    fenced: bool,
    leases: HashMap<String, Lease>,
}
struct Instance {
    pid: u32,
    generation: u64,
    session: String,
    artifact: OnceLock<Artifact>,
    gate: Mutex<Gate>,
    peer: OnceLock<RpcPeer>,
    lease_changed: Notify,
    stop: CancellationToken,
    stopped: watch::Sender<Option<StopReport>>,
    unload_lock: tokio::sync::Mutex<()>,
    effects: AtomicU64,
}
impl Instance {
    fn peer(&self) -> &RpcPeer {
        self.peer.get().expect("peer installed before publication")
    }
    fn fence(&self) {
        let mut gate = self.gate.lock().unwrap();
        gate.admitting = false;
        gate.fenced = true;
        gate.leases.clear();
        self.lease_changed.notify_waiters();
    }
    fn authorize(&self, gate: &Gate, envelope: &Value) -> Result<(), RpcError> {
        let valid = !gate.fenced
            && envelope["session"].as_str() == Some(self.session.as_str())
            && envelope["generation"].as_u64() == Some(self.generation)
            && envelope["lease"]
                .as_str()
                .and_then(|id| gate.leases.get(id))
                .is_some_and(|lease| {
                    Instant::now() < lease.expires
                        && envelope["scope"].as_str() == Some(lease.scope.as_str())
                });
        if valid {
            Ok(())
        } else {
            Err(RpcError::Remote("lease_fenced".into()))
        }
    }
}
struct HostHandler(Weak<Instance>);
#[async_trait]
impl RpcHandler for HostHandler {
    async fn handle(
        &self,
        peer: RpcPeer,
        method: String,
        params: Value,
        _cancellation: CancellationToken,
    ) -> Result<Value, RpcError> {
        let instance = self.0.upgrade().ok_or(RpcError::Closed)?;
        {
            let gate = instance.gate.lock().unwrap();
            instance.authorize(&gate, &params)?;
        }
        if method != "host.echo" {
            return Err(RpcError::Remote("unknown_host_operation".into()));
        }
        // A second callback into the guest proves both readers keep dispatching
        // while the original guest->host and host->guest calls are suspended.
        let result = peer
            .call("echo", params.clone(), Duration::from_secs(2))
            .await?;
        {
            let gate = instance.gate.lock().unwrap();
            instance.authorize(&gate, &params)?;
            instance.effects.fetch_add(1, Ordering::Relaxed);
        }
        Ok(result)
    }
}
struct RuntimeInner {
    next_generation: AtomicU64,
    instances: Mutex<HashMap<u64, Arc<Instance>>>,
}

/// A dedicated prototype host. Subreaper configuration is process-wide on Linux.
/// Call `shutdown` before shutting down the Tokio executor.
pub struct ProcessRuntime {
    inner: Arc<RuntimeInner>,
}
impl ProcessRuntime {
    pub fn new() -> Result<Self, RuntimeError> {
        set_child_subreaper(true)?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                next_generation: AtomicU64::new(1),
                instances: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn loaded_count(&self) -> usize {
        self.inner.instances.lock().unwrap().len()
    }

    /// Load an executable by path after the host is already running.
    /// P1 uses operator-trusted local files, not a production package installer.
    pub async fn load(
        &self,
        path: impl AsRef<Path>,
        identity: &str,
    ) -> Result<ModuleHandle, RuntimeError> {
        self.load_with_args(path, identity, &[], Duration::from_secs(5))
            .await
    }

    pub async fn load_with_args(
        &self,
        path: impl AsRef<Path>,
        identity: &str,
        args: &[&str],
        handshake_timeout: Duration,
    ) -> Result<ModuleHandle, RuntimeError> {
        let path = std::fs::canonicalize(path)?;
        let bytes = std::fs::read(&path)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        drop(bytes);
        let mut command = Command::new(&path);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child.id().expect("new child has pid");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        let (stopped, _) = watch::channel(None);
        let instance = Arc::new(Instance {
            pid,
            generation,
            session: uuid::Uuid::new_v4().to_string(),
            artifact: OnceLock::new(),
            gate: Mutex::new(Gate {
                admitting: false,
                fenced: false,
                leases: HashMap::new(),
            }),
            peer: OnceLock::new(),
            lease_changed: Notify::new(),
            stop: CancellationToken::new(),
            stopped,
            unload_lock: tokio::sync::Mutex::new(()),
            effects: AtomicU64::new(0),
        });
        let peer = RpcPeer::new(
            output,
            input,
            Arc::new(HostHandler(Arc::downgrade(&instance))),
        );
        assert!(instance.peer.set(peer).is_ok());
        self.inner
            .instances
            .lock()
            .unwrap()
            .insert(generation, instance.clone());
        let monitor_instance = instance.clone();
        let owner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            supervise(child, stderr, monitor_instance, owner).await;
        });
        let handle = ModuleHandle { instance };
        // A dropped load future must also request cleanup instead of losing the child.
        let mut loading = LoadingGuard(Some(handle.clone()));
        let result = async {
            let hello = handle
                .instance
                .peer()
                .call("hello", json!({}), handshake_timeout)
                .await?;
            if hello["identity"].as_str() != Some(identity) || hello["protocol"].as_u64() != Some(1)
            {
                return Err(RuntimeError::Handshake(hello.to_string()));
            }
            let artifact = Artifact {
                path,
                sha256: digest,
                identity: identity.into(),
                build: hello["build"].as_str().unwrap_or("unknown").into(),
            };
            assert!(handle.instance.artifact.set(artifact).is_ok());
            let initialized = handle
                .instance
                .peer()
                .call(
                    "initialize",
                    json!({
                        "session": handle.instance.session,
                        "generation": generation,
                    }),
                    handshake_timeout,
                )
                .await?;
            if initialized["initialized"].as_bool() != Some(true) {
                return Err(RuntimeError::Handshake(
                    "initialization was not acknowledged".into(),
                ));
            }
            let mut gate = handle.instance.gate.lock().unwrap();
            if gate.fenced {
                return Err(RuntimeError::Unavailable);
            }
            gate.admitting = true;
            Ok::<_, RuntimeError>(())
        }
        .await;
        if let Err(error) = result {
            handle.instance.fence();
            handle.instance.stop.cancel();
            handle.wait_stopped(Duration::from_secs(5)).await?;
            return Err(error);
        }
        loading.0 = None;
        Ok(handle)
    }

    pub async fn shutdown(&self) -> Result<Vec<StopReport>, RuntimeError> {
        let instances: Vec<_> = self
            .inner
            .instances
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut reports = Vec::new();
        for instance in instances {
            reports.push(
                ModuleHandle { instance }
                    .unload(Duration::from_millis(250))
                    .await?,
            );
        }
        Ok(reports)
    }
}
impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        for instance in self.inner.instances.lock().unwrap().values() {
            instance.fence();
            instance.stop.cancel();
        }
    }
}
struct LoadingGuard(Option<ModuleHandle>);
impl Drop for LoadingGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.instance.fence();
            handle.instance.stop.cancel();
        }
    }
}

#[derive(Clone)]
pub struct ModuleHandle {
    instance: Arc<Instance>,
}
impl ModuleHandle {
    pub fn pid(&self) -> u32 {
        self.instance.pid
    }
    pub fn generation(&self) -> u64 {
        self.instance.generation
    }
    pub fn artifact(&self) -> &Artifact {
        self.instance.artifact.get().expect("loaded artifact")
    }
    pub fn accepted_callbacks(&self) -> u64 {
        self.instance.effects.load(Ordering::Relaxed)
    }

    pub async fn invoke(
        &self,
        method: &str,
        input: Value,
        scope: &str,
        deadline: Duration,
    ) -> Result<Value, RuntimeError> {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut gate = self.instance.gate.lock().unwrap();
            if !gate.admitting || gate.fenced {
                return Err(RuntimeError::Unavailable);
            }
            gate.leases.insert(
                id.clone(),
                Lease {
                    scope: scope.into(),
                    expires: Instant::now() + deadline,
                },
            );
        }
        let _lease = LeaseGuard {
            instance: self.instance.clone(),
            id: id.clone(),
        };
        let envelope = json!({
            "session": self.instance.session, "generation": self.generation(),
            "lease": id, "scope": scope, "input": input,
        });
        Ok(self
            .instance
            .peer()
            .call(method, envelope, deadline)
            .await?)
    }

    pub async fn unload(&self, drain: Duration) -> Result<StopReport, RuntimeError> {
        let _serial = self.instance.unload_lock.lock().await;
        if let Some(report) = self.instance.stopped.borrow().clone() {
            return Ok(report);
        }
        let _cleanup_on_cancel = LoadingGuard(Some(self.clone()));
        self.instance.gate.lock().unwrap().admitting = false;
        let wait = async {
            loop {
                let changed = self.instance.lease_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.instance.gate.lock().unwrap().leases.is_empty() {
                    break;
                }
                changed.await;
            }
        };
        let _ = timeout(drain, wait).await;
        self.instance.fence();
        // A bounded cleanup-only call; no new ordinary invocation lease is issued.
        let _ = self
            .instance
            .peer()
            .call("shutdown", json!({}), Duration::from_millis(200))
            .await;
        self.instance.stop.cancel();
        self.wait_stopped(Duration::from_secs(5)).await
    }

    pub async fn force_stop(&self) -> Result<StopReport, RuntimeError> {
        self.instance.fence();
        self.instance.stop.cancel();
        self.wait_stopped(Duration::from_secs(5)).await
    }

    pub async fn wait_stopped(&self, duration: Duration) -> Result<StopReport, RuntimeError> {
        let mut receiver = self.instance.stopped.subscribe();
        timeout(duration, async {
            loop {
                if let Some(report) = receiver.borrow().clone() {
                    return report;
                }
                receiver
                    .changed()
                    .await
                    .expect("instance owns completion sender");
            }
        })
        .await
        .map_err(|_| RuntimeError::CleanupTimeout)
    }
}
struct LeaseGuard {
    instance: Arc<Instance>,
    id: String,
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.instance.gate.lock().unwrap().leases.remove(&self.id);
        self.instance.lease_changed.notify_waiters();
    }
}

fn signal_group(pid: u32, signal: Signal) -> Result<(), Errno> {
    match killpg(Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn observe_exit(pid: u32) -> Result<WaitStatus, Errno> {
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
        _ = instance.peer().wait_closed() => None,
    };
    instance.fence();
    instance.peer().close().await;
    let mut forced = false;
    let mut cleanup_errors = Vec::new();
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
    if let Some(Err(error)) = observation {
        cleanup_errors.push(format!("observe child: {error}"));
    } else if let Err(error) = signal_group(instance.pid, Signal::SIGKILL) {
        cleanup_errors.push(format!("killpg: {error}"));
    }
    // Public waiters have deadlines, but cleanup retains ownership until the OS
    // actually reports exit. A task stuck in a kernel wait is not unloaded.
    let exit: Option<ExitStatus> = match child.wait().await {
        Ok(status) => Some(status),
        Err(error) => {
            cleanup_errors.push(format!("direct child wait: {error}"));
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
            Err(error) => {
                cleanup_errors.push(format!("descendant wait: {error}"));
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
        owner.instances.lock().unwrap().remove(&instance.generation);
    }
    instance.stopped.send_replace(Some(report));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_expiry_fences_without_waiting_for_caller_cleanup() {
        let (stopped, _) = watch::channel(None);
        let instance = Instance {
            pid: 1,
            generation: 7,
            session: "session".into(),
            artifact: OnceLock::new(),
            gate: Mutex::new(Gate {
                admitting: false,
                fenced: false,
                leases: HashMap::new(),
            }),
            peer: OnceLock::new(),
            lease_changed: Notify::new(),
            stop: CancellationToken::new(),
            stopped,
            unload_lock: tokio::sync::Mutex::new(()),
            effects: AtomicU64::new(0),
        };
        let mut gate = instance.gate.lock().unwrap();
        gate.leases.insert(
            "lease".into(),
            Lease {
                scope: "guild:123".into(),
                expires: Instant::now() + Duration::from_secs(1),
            },
        );
        let envelope =
            json!({"session": "session", "generation": 7, "lease": "lease", "scope": "guild:123"});
        // Quiesce closes new admission but preserves this existing unexpired lease.
        assert!(instance.authorize(&gate, &envelope).is_ok());
        gate.leases.get_mut("lease").unwrap().expires = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            instance.authorize(&gate, &envelope),
            Err(RpcError::Remote("lease_fenced".into()))
        );
        assert!(gate.leases.contains_key("lease"));
    }

    #[tokio::test]
    async fn exit_observation_preserves_pid_until_group_signal_and_reap() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
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
