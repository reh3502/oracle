//! Windows-specific FFI is kept behind owned handles and safe process operations.
use super::*;
use std::{
    fs::{File, OpenOptions},
    io,
    mem::{size_of, zeroed},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        },
    },
};

pub(super) fn open_executable(path: &Path) -> io::Result<(PathBuf, File)> {
    let path = std::fs::canonicalize(path)?;
    // FILE_SHARE_READ only: existing writers are rejected and future writes,
    // renames and deletes are denied until this handle closes after spawn.
    let file = OpenOptions::new().read(true).share_mode(1).open(&path)?;
    Ok((path, file))
}

pub(super) struct Job(OwnedHandle);
impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: no name or security attributes, so the new handle is private
        // and non-inheritable. OwnedHandle closes it on every return path.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        // SAFETY: a zeroed Win32 POD structure with its documented limit set.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: handle is live and structure pointer and size agree.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
    fn terminate(&self) -> io::Result<()> {
        // SAFETY: owned handle remains live throughout the call.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn accounting(&self) -> io::Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
        // SAFETY: Win32 POD output has the exact size expected by this query.
        let mut info = unsafe { zeroed::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() };
        if unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }
}

pub(super) fn spawn(path: &Path) -> io::Result<(Child, Job)> {
    spawn_command(Command::new(path))
}

fn spawn_command(mut command: Command) -> io::Result<(Child, Job)> {
    let job = Job::new()?;
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
        .kill_on_drop(true);
    // Windows system libraries may need these OS paths; no user/bot secrets
    // are inherited by module processes.
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let child = command.spawn()?;
    // SAFETY: Tokio owns a live child handle. The initial thread is suspended,
    // so no module code can execute or create descendants before assignment.
    if unsafe {
        AssignProcessToJobObject(
            job.0.as_raw_handle(),
            child.raw_handle().expect("new child handle"),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    resume_initial_thread(child.id().expect("new child PID"))?;
    Ok((child, job))
}

fn resume_initial_thread(pid: u32) -> io::Result<()> {
    // SAFETY: API creates a new non-inheritable snapshot handle.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: Win32 POD initialized with its required structure size.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: OpenThread returns an independently owned handle. The
            // suspended child has exactly one initial thread and cannot exit
            // or recycle its identity before we resume it.
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread unavailable",
    ))
}

pub(super) async fn supervise(
    mut child: Child,
    job: Job,
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
    let first_exit = tokio::select! {
        exit = child.wait() => Some(exit),
        _ = instance.stop.cancelled() => None,
        _ = instance.peer.wait_closed() => None,
    };
    instance.fence();
    instance.peer.close().await;
    let mut errors = Vec::new();
    let mut forced = false;
    let observed = match first_exit {
        Some(result) => Some(result),
        None => match timeout(Duration::from_millis(250), child.wait()).await {
            Ok(result) => Some(result),
            Err(_) => {
                forced = true;
                None
            }
        },
    };
    // The job handle reserves the containment identity even after leader exit.
    if job.terminate().is_err() {
        errors.push("terminate_job_failed");
    }
    let exit = match match observed {
        Some(result) => result,
        None => child.wait().await,
    } {
        Ok(status) => Some(status),
        Err(_) => {
            errors.push("direct_child_wait_failed");
            None
        }
    };
    let descendants_reaped = loop {
        match job.accounting() {
            Ok(info) if info.ActiveProcesses == 0 => {
                break info.TotalProcesses.saturating_sub(1) as usize;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            Err(_) => {
                errors.push("job_accounting_failed");
                break 0;
            }
        }
    };
    drop(job);
    if timeout(Duration::from_millis(250), &mut stderr_task)
        .await
        .is_err()
    {
        stderr_task.abort();
        let _ = stderr_task.await;
        errors.push("stderr did not reach EOF after job cleanup");
    }
    let report = StopReport {
        pid: instance.pid,
        generation: instance.generation,
        exit_code: exit.and_then(|status| status.code()),
        forced,
        descendants_reaped,
        stderr_bytes: stderr_bytes.load(Ordering::Relaxed),
        cleanup_error: (!errors.is_empty()).then(|| errors.join("; ")),
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

    #[test]
    fn verification_handle_blocks_write_and_replacement() {
        let path =
            std::env::temp_dir().join(format!("oracle-executable-lock-{}.exe", std::process::id()));
        std::fs::write(&path, b"verified bytes").unwrap();
        let (_, handle) = open_executable(&path).unwrap();
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, path.with_extension("moved")).is_err());
        drop(handle);
        std::fs::remove_file(path).unwrap();
    }

    // Executed only as subprocess fixtures by the containment test below.
    #[test]
    #[ignore]
    fn guest_child() {
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    #[ignore]
    fn guest_parent() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "windows::tests::guest_child", "--ignored"])
            .spawn()
            .unwrap();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn closing_job_kills_child_and_descendants() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "windows::tests::guest_parent", "--ignored"]);
        let (mut child, job) = spawn_command(command).unwrap();
        timeout(Duration::from_secs(10), async {
            while job.accounting().unwrap().ActiveProcesses < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture must create a descendant inside the job");
        job.terminate().unwrap();
        timeout(Duration::from_secs(10), async {
            while job.accounting().unwrap().ActiveProcesses != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("termination must finish every descendant");
        assert_eq!(job.accounting().unwrap().TotalProcesses, 2);
        assert!(!child.wait().await.unwrap().success());

        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "windows::tests::guest_child", "--ignored"]);
        let (mut child, job) = spawn_command(command).unwrap();
        drop(job);
        // Closing a job may report exit code zero; the deadline proves the
        // 60-second fixture was killed, regardless of the OS-selected code.
        timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("dropping the last job handle must kill the child")
            .unwrap();
    }
}
