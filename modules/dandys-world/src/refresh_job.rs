//! Bounded, operator-configured acquisition worker. No shell, publication, or
//! untrusted process output crosses this boundary. Cancel and await `run` during
//! module quiescence so the worker is reaped before its writer lock is released.
use crate::snapshot::{MAX_SNAPSHOT_BYTES, Store};
use oracle_module_sdk::CancellationToken;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{process::Command, time::Instant};

const STORE_BUDGET: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 50_000;
const MAX_DEPTH: usize = 32;
const RUN_TIMEOUT: Duration = Duration::from_secs(900);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshJobSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source_access_qualified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshJobFailure {
    InvalidSettings,
    Busy,
    Storage,
    Quota,
    Spawn,
    Worker,
    Timeout,
    InvalidOutput,
    Cleanup,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RefreshJobOutcome {
    Disabled,
    /// Acquisition bytes only. Review/schema/provenance validation is mandatory
    /// before publication; a worker success cannot authorize publication.
    Candidate(Vec<u8>),
    Denied,
    RetryAt {
        not_before_ms: u64,
    },
    Cancelled,
    Failed(RefreshJobFailure),
}
type Result<T> = std::result::Result<T, RefreshJobFailure>;
fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
        && path.components().all(|c| {
            matches!(
                c,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}
fn safe_open() -> OpenOptions {
    crate::snapshot::safe_options()
}
fn single_link(file: &File) -> bool {
    #[cfg(unix)]
    {
        file.metadata().is_ok_and(|m| m.nlink() == 1)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: open file handle and writable information buffer remain live.
        unsafe {
            GetFileInformationByHandle(file.as_raw_handle(), &mut info) != 0
                && info.nNumberOfLinks == 1
        }
    }
}

#[cfg(unix)]
fn check_file(path: &Path, uid: u32, python: bool) -> Result<()> {
    if !absolute(path) {
        return Err(RefreshJobFailure::InvalidSettings);
    }
    // Python virtual environments commonly use a symlink to the system binary.
    // Preserve the operator's executable spelling so Python still finds pyvenv.cfg.
    let meta = if python {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .map_err(|_| RefreshJobFailure::InvalidSettings)?;
    if !meta.is_file()
        || (!python && crate::snapshot::is_reparse(&meta))
        || !matches!(meta.uid(),owner if owner==uid||owner==0)
        || meta.mode() & 0o022 != 0
        || (python && (meta.mode() & 0o111 == 0 || meta.mode() & 0o6000 != 0))
    {
        return Err(RefreshJobFailure::InvalidSettings);
    }
    Ok(())
}
#[cfg(windows)]
fn check_file(path: &Path, _uid: u32, _python: bool) -> Result<()> {
    if !absolute(path) || !oracle_local_ipc::private_file(path).unwrap_or(false) {
        return Err(RefreshJobFailure::InvalidSettings);
    }
    directory(path.parent().ok_or(RefreshJobFailure::InvalidSettings)?)
        .map_err(|_| RefreshJobFailure::InvalidSettings)
}
fn directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        let meta = fs::symlink_metadata(&current).map_err(|_| RefreshJobFailure::Storage)?;
        if !meta.is_dir() || crate::snapshot::is_reparse(&meta) {
            return Err(RefreshJobFailure::Storage);
        }
    }
    Ok(())
}
fn check_budget(cancel: &CancellationToken, deadline: Instant) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(RefreshJobFailure::Worker);
    }
    if Instant::now() >= deadline {
        return Err(RefreshJobFailure::Timeout);
    }
    Ok(())
}
fn disk_bytes(root: &Path, cancel: &CancellationToken, deadline: Instant) -> Result<u64> {
    let mut pending = vec![(root.to_owned(), 0)];
    let mut total = 0u64;
    let mut count = 0;
    while let Some((path, depth)) = pending.pop() {
        check_budget(cancel, deadline)?;
        if depth > MAX_DEPTH {
            return Err(RefreshJobFailure::Quota);
        }
        directory(&path)?;
        for entry in fs::read_dir(path).map_err(|_| RefreshJobFailure::Storage)? {
            check_budget(cancel, deadline)?;
            count += 1;
            if count > MAX_ENTRIES {
                return Err(RefreshJobFailure::Quota);
            }
            let entry = entry.map_err(|_| RefreshJobFailure::Storage)?;
            let meta =
                fs::symlink_metadata(entry.path()).map_err(|_| RefreshJobFailure::Storage)?;
            if crate::snapshot::is_reparse(&meta) {
                return Err(RefreshJobFailure::Storage);
            }
            if meta.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if meta.is_file() {
                total = total
                    .checked_add(meta.len())
                    .ok_or(RefreshJobFailure::Quota)?;
                if total > STORE_BUDGET {
                    return Err(RefreshJobFailure::Quota);
                }
            } else {
                return Err(RefreshJobFailure::Storage);
            }
        }
    }
    Ok(total)
}
struct Work {
    path: PathBuf,
}
impl Work {
    fn create(root: &Path) -> Result<Self> {
        for _ in 0..32 {
            let path = root.join(format!(
                "refresh-work-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            let created = fs::DirBuilder::new().mode(0o700).create(&path);
            #[cfg(windows)]
            let created = oracle_local_ipc::create_private_directory_new(&path);
            match created {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(RefreshJobFailure::Storage),
            }
        }
        Err(RefreshJobFailure::Storage)
    }
    fn output(&self) -> PathBuf {
        self.path.join("output")
    }
    fn cleanup(&self) -> Result<()> {
        remove_owned_tree(&self.path, 0, &mut 0).map_err(|_| RefreshJobFailure::Cleanup)
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
// Never follow links, even if a worker failed while creating its output. The
// wrapper directory was atomically reserved by this invocation, not discovered
// by a broad staging glob. Other jobs and operator data are never removed.
fn remove_owned_tree(path: &Path, depth: usize, count: &mut usize) -> std::io::Result<()> {
    if depth > MAX_DEPTH + 2 || *count > MAX_ENTRIES * 2 {
        return Err(std::io::Error::other("cleanup bound"));
    }
    *count += 1;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() && !crate::snapshot::is_reparse(&meta) {
        for entry in fs::read_dir(path)? {
            remove_owned_tree(&entry?.path(), depth + 1, count)?;
        }
        fs::remove_dir(path)
    } else if meta.is_dir() {
        // Windows junctions are directories; unlink the junction itself.
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}
fn read_output(
    work: &Work,
    name: &str,
    max: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>> {
    check_budget(cancel, deadline)?;
    directory(&work.output())?;
    let mut file = safe_open()
        .read(true)
        .open(work.output().join(name))
        .map_err(|_| RefreshJobFailure::InvalidOutput)?;
    let metadata = file
        .metadata()
        .map_err(|_| RefreshJobFailure::InvalidOutput)?;
    if !metadata.is_file()
        || crate::snapshot::is_reparse(&metadata)
        || !single_link(&file)
        || metadata.len() == 0
        || metadata.len() > max as u64
    {
        return Err(RefreshJobFailure::InvalidOutput);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0u8; 65536];
    loop {
        check_budget(cancel, deadline)?;
        let n = file
            .read(&mut buffer)
            .map_err(|_| RefreshJobFailure::InvalidOutput)?;
        if n == 0 {
            break;
        }
        if bytes.len().saturating_add(n) > max {
            return Err(RefreshJobFailure::InvalidOutput);
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
    if bytes.is_empty() {
        return Err(RefreshJobFailure::InvalidOutput);
    }
    Ok(bytes)
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetryStatus {
    Retry,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryMetadata {
    status: RetryStatus,
    retry_not_before_ms: u64,
}
fn retry_floor(work: &Work, cancel: &CancellationToken, deadline: Instant) -> Option<u64> {
    let bytes = read_output(work, "result.json", 512, cancel, deadline).ok()?;
    let result: RetryMetadata = serde_json::from_slice(&bytes).ok()?;
    let RetryStatus::Retry = result.status;
    (result.retry_not_before_ms > 0).then_some(result.retry_not_before_ms)
}
struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
struct Prepared {
    writer: WriterLock,
    work: Work,
    available: u64,
    settings: RefreshJobSettings,
    root: PathBuf,
}
fn prepare(
    settings: RefreshJobSettings,
    root: PathBuf,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Prepared> {
    check_budget(cancel, deadline)?;
    let (Some(python), Some(worker)) = (&settings.python, &settings.worker) else {
        return Err(RefreshJobFailure::InvalidSettings);
    };
    if !absolute(&root)
        || root.components().count() < 3
        || settings.previous.as_ref().is_some_and(|p| !absolute(p))
    {
        return Err(RefreshJobFailure::InvalidSettings);
    }
    Store::new(&root).map_err(|_| RefreshJobFailure::Storage)?;
    #[cfg(unix)]
    let uid = fs::metadata(&root)
        .map_err(|_| RefreshJobFailure::Storage)?
        .uid();
    #[cfg(windows)]
    let uid = {
        if !oracle_local_ipc::private_directory(&root).unwrap_or(false) {
            return Err(RefreshJobFailure::Storage);
        }
        0
    };
    check_file(python, uid, true)?;
    check_file(worker, uid, false)?;
    if settings
        .previous
        .as_ref()
        .is_some_and(|p| directory(p).is_err())
    {
        return Err(RefreshJobFailure::InvalidSettings);
    }
    let writer = safe_open()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("writer.lock"))
        .map_err(|_| RefreshJobFailure::Storage)?;
    if !writer
        .metadata()
        .is_ok_and(|m| m.is_file() && !crate::snapshot::is_reparse(&m))
        || !single_link(&writer)
    {
        return Err(RefreshJobFailure::Storage);
    }
    writer.try_lock().map_err(|_| RefreshJobFailure::Busy)?;
    let writer = WriterLock(writer);
    let used = disk_bytes(&root, cancel, deadline)?;
    let available = STORE_BUDGET - used;
    if available == 0 {
        return Err(RefreshJobFailure::Quota);
    }
    check_budget(cancel, deadline)?;
    let work = Work::create(&root)?;
    Ok(Prepared {
        writer,
        work,
        available,
        settings,
        root,
    })
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
/// A joined supervisor owns the child and writer lock. Even if the calling
/// future is dropped, its guard cancels the supervisor; the supervisor reaps the
/// process and performs blocking cleanup before releasing the lock.
pub async fn run(
    settings: &RefreshJobSettings,
    store_root: &Path,
    cancel: CancellationToken,
) -> RefreshJobOutcome {
    if !settings.enabled {
        return RefreshJobOutcome::Disabled;
    }
    if !settings.source_access_qualified {
        return RefreshJobOutcome::Denied;
    }
    if cancel.is_cancelled() {
        return RefreshJobOutcome::Cancelled;
    }
    let local = cancel.child_token();
    let guard = CancelOnDrop(local.clone());
    let settings = settings.clone();
    let root = store_root.to_owned();
    let deadline = Instant::now() + RUN_TIMEOUT;
    let job = tokio::spawn(async move { run_inner(settings, root, local, deadline).await });
    let outcome = job
        .await
        .unwrap_or(RefreshJobOutcome::Failed(RefreshJobFailure::Worker));
    drop(guard);
    outcome
}
async fn run_inner(
    settings: RefreshJobSettings,
    root: PathBuf,
    cancel: CancellationToken,
    deadline: Instant,
) -> RefreshJobOutcome {
    use RefreshJobOutcome::*;
    let admission_deadline = (Instant::now() + Duration::from_secs(2)).min(deadline);
    let prepared = loop {
        let checking = cancel.clone();
        let settings = settings.clone();
        let root = root.clone();
        let prepared =
            tokio::task::spawn_blocking(move || prepare(settings, root, &checking, deadline)).await;
        match prepared {
            Ok(Ok(prepared)) => break prepared,
            Ok(Err(_)) if cancel.is_cancelled() => return Cancelled,
            // Busy is returned before staging allocation or process creation.
            // Await each attempt; never retry a worker that has started.
            Ok(Err(RefreshJobFailure::Busy)) if Instant::now() < admission_deadline => {
                tokio::select! {biased; _=cancel.cancelled()=>return Cancelled, _=tokio::time::sleep(Duration::from_millis(25))=>{}}
            }
            Ok(Err(RefreshJobFailure::Busy)) if Instant::now() >= deadline => {
                return Failed(RefreshJobFailure::Timeout);
            }
            Ok(Err(error)) => return Failed(error),
            Err(_) => return Failed(RefreshJobFailure::Storage),
        }
    };
    let mut command = Command::new(prepared.settings.python.as_ref().unwrap());
    command
        .arg(prepared.settings.worker.as_ref().unwrap())
        .arg("--output")
        .arg(prepared.work.output())
        .arg("--budget-bytes")
        .arg(prepared.available.to_string())
        .env_clear()
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .current_dir(&prepared.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command.env("PYTHONUTF8", "1");
    }
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: this post-fork hook uses only async-signal-safe syscalls and
        // stack values. Register before exec, then close the parent-death race.
        // Python and the worker cannot outlive a forcibly killed module host.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(125);
                }
                Ok(())
            });
        }
    }
    if let Some(previous) = &prepared.settings.previous {
        command.arg("--previous").arg(previous);
    }
    let status = if cancel.is_cancelled() {
        Err(Cancelled)
    } else if Instant::now() >= deadline {
        Err(Failed(RefreshJobFailure::Timeout))
    } else {
        match command.spawn() {
            Err(_) => Err(Failed(RefreshJobFailure::Spawn)),
            Ok(mut child) => {
                tokio::select! {biased;
                    _=cancel.cancelled()=>{let _=child.kill().await;Err(Cancelled)},
                    _=tokio::time::sleep_until(deadline)=>{let _=child.kill().await;Err(Failed(RefreshJobFailure::Timeout))},
                    status=child.wait()=>match status{Ok(s)=>Ok(s),Err(_)=>{let _=child.kill().await;Err(Failed(RefreshJobFailure::Worker))}},
                }
            }
        }
    };
    // Scans, candidate reads and recursive cleanup run off the query executor.
    // This task is joined even after cancellation; writer.lock stays owned until
    // all child-produced disk work is gone. No readers.lock is ever acquired.
    tokio::task::spawn_blocking(move || {
        let outcome = if cancel.is_cancelled() {
            Cancelled
        } else {
            match status {
                Err(outcome) => outcome,
                Ok(status) if status.code() == Some(3) => Denied,
                Ok(status) if status.code() == Some(124) => Failed(RefreshJobFailure::Timeout),
                Ok(status) if !status.success() => match (
                    status.code(),
                    retry_floor(&prepared.work, &cancel, deadline),
                ) {
                    (Some(2), Some(not_before_ms)) => RetryAt { not_before_ms },
                    _ => Failed(RefreshJobFailure::Worker),
                },
                Ok(_) => match disk_bytes(&prepared.root, &cancel, deadline).and_then(|_| {
                    read_output(
                        &prepared.work,
                        "candidate.json",
                        MAX_SNAPSHOT_BYTES,
                        &cancel,
                        deadline,
                    )
                }) {
                    Ok(bytes) => Candidate(bytes),
                    Err(_) if cancel.is_cancelled() => Cancelled,
                    Err(e) => Failed(e),
                },
            }
        };
        let cleaned = prepared.work.cleanup();
        drop(prepared.work);
        drop(prepared.writer);
        if cleaned.is_err() {
            // Never discard a denial or server retry floor because local cleanup
            // failed: that would authorize another request too soon. Residue
            // remains charged to quota and no candidate is returned.
            match outcome {
                Denied | RetryAt { .. } | Cancelled => outcome,
                _ => Failed(RefreshJobFailure::Cleanup),
            }
        } else {
            outcome
        }
    })
    .await
    .unwrap_or(Failed(RefreshJobFailure::Cleanup))
}

#[cfg(all(test, unix))]
mod deadline_tests {
    use super::*;
    fn admission_fixture() -> (PathBuf, PathBuf, RefreshJobSettings, File) {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "dw-job-admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let worker = root.join("worker.py");
        fs::write(&worker,b"from pathlib import Path\nimport sys\np=Path(__file__).with_suffix('.count')\np.write_text(str(int(p.read_text())+1) if p.exists() else '1')\nsys.exit(3)\n").unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();
        let store = root.join("store");
        Store::new(&store).unwrap();
        let writer = safe_open()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.join("writer.lock"))
            .unwrap();
        writer.lock().unwrap();
        let settings = RefreshJobSettings {
            enabled: true,
            source_access_qualified: true,
            python: Some(PathBuf::from("/usr/bin/python3")),
            worker: Some(worker),
            previous: None,
        };
        (root, store, settings, writer)
    }
    #[tokio::test]
    async fn busy_admission_retries_only_before_staging_and_source_execution() {
        let (root, store, settings, writer) = admission_fixture();
        let worker = settings.worker.clone().unwrap();
        let job_store = store.clone();
        let job =
            tokio::spawn(async move { run(&settings, &job_store, CancellationToken::new()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!worker.with_extension("count").exists());
        assert!(!fs::read_dir(&store).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("refresh-work-")
        }));
        writer.unlock().unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), job)
                .await
                .unwrap()
                .unwrap(),
            RefreshJobOutcome::Denied
        );
        assert_eq!(
            fs::read_to_string(worker.with_extension("count")).unwrap(),
            "1"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn cancelled_busy_admission_never_allocates_or_starts_after_unlock() {
        let (root, store, settings, writer) = admission_fixture();
        let worker = settings.worker.clone().unwrap();
        let job_store = store.clone();
        let cancel = CancellationToken::new();
        let pending = cancel.clone();
        let job = tokio::spawn(async move { run(&settings, &job_store, pending).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), job)
                .await
                .unwrap()
                .unwrap(),
            RefreshJobOutcome::Cancelled
        );
        writer.unlock().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!worker.with_extension("count").exists());
        assert!(!fs::read_dir(&store).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("refresh-work-")
        }));
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn supervisor_deadline_kills_started_worker_and_cleans_before_unlock() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "dw-job-deadline-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let worker = root.join("worker.py");
        fs::write(&worker,b"import argparse,pathlib,time,os\np=argparse.ArgumentParser();p.add_argument('--output');p.add_argument('--budget-bytes');a=p.parse_args();o=pathlib.Path(a.output);o.mkdir();(o.parent.parent/'started').write_text(str(os.getpid()));time.sleep(30)\n").unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();
        let settings = RefreshJobSettings {
            enabled: true,
            source_access_qualified: true,
            python: Some(PathBuf::from("/usr/bin/python3")),
            worker: Some(worker),
            previous: None,
        };
        let store = root.join("store");
        let outcome = run_inner(
            settings,
            store.clone(),
            CancellationToken::new(),
            Instant::now() + Duration::from_millis(300),
        )
        .await;
        assert_eq!(
            outcome,
            RefreshJobOutcome::Failed(RefreshJobFailure::Timeout)
        );
        let pid = fs::read_to_string(store.join("started"))
            .expect("fixture entered the worker before the deadline");
        assert!(!Path::new("/proc").join(pid).exists());
        assert!(!fs::read_dir(&store).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("refresh-work-")
        }));
        let writer = safe_open()
            .read(true)
            .write(true)
            .open(store.join("writer.lock"))
            .unwrap();
        writer.try_lock().unwrap();
        writer.unlock().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
