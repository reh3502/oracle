use dandys_world_core::refresh_job::{
    RefreshJobFailure, RefreshJobOutcome, RefreshJobSettings, run,
};
use oracle_module_sdk::CancellationToken;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    store: PathBuf,
    settings: RefreshJobSettings,
}
impl Fixture {
    fn new(body: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dw-job-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let store = root.join("store");
        fs::DirBuilder::new().mode(0o700).create(&store).unwrap();
        let worker = root.join("worker.py");
        fs::write(&worker,format!("import argparse, pathlib, os, time, sys, json\np=argparse.ArgumentParser()\np.add_argument('--output',required=True)\np.add_argument('--budget-bytes',type=int,required=True)\np.add_argument('--previous')\na=p.parse_args()\no=pathlib.Path(a.output)\nassert o.is_absolute() and not o.exists()\no.mkdir()\n{body}\n")).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();
        let settings = RefreshJobSettings {
            enabled: true,
            source_access_qualified: true,
            python: Some(PathBuf::from("/usr/bin/python3")),
            worker: Some(worker),
            previous: None,
        };
        Self {
            root,
            store,
            settings,
        }
    }
    async fn run(&self) -> RefreshJobOutcome {
        run(&self.settings, &self.store, CancellationToken::new()).await
    }
    fn clean(&self) {
        assert!(!fs::read_dir(&self.store).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("refresh-work-")
        }));
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.store.join("writer.lock"))
            .unwrap();
        writer
            .try_lock()
            .expect("writer lock released only after cleanup");
        writer.unlock().unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
async fn ready(store: &Path) -> PathBuf {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for entry in fs::read_dir(store).unwrap().flatten() {
                let ready = entry.path().join("output/ready");
                if ready.is_file() {
                    return ready;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture worker started")
}
#[tokio::test]
async fn disabled_and_unqualified_settings_never_start_worker() {
    let mut f = Fixture::new("raise RuntimeError('must not execute')");
    f.settings.enabled = false;
    assert_eq!(f.run().await, RefreshJobOutcome::Disabled);
    f.settings.enabled = true;
    f.settings.source_access_qualified = false;
    assert_eq!(f.run().await, RefreshJobOutcome::Denied);
    assert!(!f.store.join("writer.lock").exists());
    assert!(
        !serde_json::from_str::<RefreshJobSettings>("{}")
            .unwrap()
            .source_access_qualified
    );
    assert!(
        serde_json::from_str::<RefreshJobSettings>(
            r#"{"enabled":true,"url":"https://other.invalid"}"#
        )
        .is_err()
    );
}
#[tokio::test]
async fn candidate_is_bounded_unpublished_and_cleanup_releases_writer() {
    let f = Fixture::new("(o/'candidate.json').write_bytes(b'{\"fixture\":true}')");
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Candidate(b"{\"fixture\":true}".to_vec())
    );
    assert!(!f.store.join("active").exists());
    f.clean();
}
#[tokio::test]
async fn runner_holds_writer_singleflight_but_never_readers_lock() {
    let f = Fixture::new("(o/'candidate.json').write_bytes(b'fixture')");
    let readers = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.store.join("readers.lock"))
        .unwrap();
    readers.try_lock().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), f.run())
        .await
        .expect("runner must not wait for readers.lock");
    assert!(matches!(result, RefreshJobOutcome::Candidate(_)));
    drop(readers);
    f.clean();
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.store.join("writer.lock"))
        .unwrap();
    writer.try_lock().unwrap();
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Failed(RefreshJobFailure::Busy)
    );
    writer.unlock().unwrap();
    drop(writer);
    f.clean();
}
#[tokio::test]
async fn recursive_existing_bytes_are_subtracted_from_worker_budget() {
    let mut f = Fixture::new(
        "(o/'candidate.json').write_text(json.dumps({'budget':a.budget_bytes,'previous':a.previous}))",
    );
    let nested = f.store.join("retained/subdir");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("bytes"), vec![0; 4321]).unwrap();
    let previous = f.root.join("prior corpus with spaces");
    fs::create_dir(&previous).unwrap();
    f.settings.previous = Some(previous.clone());
    let RefreshJobOutcome::Candidate(bytes) = f.run().await else {
        panic!("expected acquisition bytes")
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["budget"], 512_u64 * 1024 * 1024 - 4321);
    assert_eq!(value["previous"], previous.to_str().unwrap());
    assert!(nested.join("bytes").exists());
    f.clean();
}
#[tokio::test]
async fn exhausted_store_quota_does_not_spawn_or_remove_existing_files() {
    let f = Fixture::new("raise RuntimeError('must not start')");
    let file = fs::File::create(f.store.join("retained")).unwrap();
    file.set_len(512 * 1024 * 1024).unwrap();
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Failed(RefreshJobFailure::Quota)
    );
    assert_eq!(file.metadata().unwrap().len(), 512 * 1024 * 1024);
    f.clean();
}
#[tokio::test]
async fn cancellation_reaps_child_before_removing_output_and_unlocking() {
    let f = Fixture::new("(o/'ready').write_text(str(os.getpid()))\ntime.sleep(30)");
    let settings = f.settings.clone();
    let store = f.store.clone();
    let token = CancellationToken::new();
    let child_token = token.clone();
    let task = tokio::spawn(async move { run(&settings, &store, child_token).await });
    let ready = ready(&f.store).await;
    let pid = fs::read_to_string(ready).unwrap();
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.store.join("writer.lock"))
        .unwrap();
    assert!(writer.try_lock().is_err());
    token.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap(),
        RefreshJobOutcome::Cancelled
    );
    assert!(
        !Path::new("/proc").join(pid).exists(),
        "cancelled child must already be reaped"
    );
    f.clean();
}
#[tokio::test]
async fn dropping_caller_future_still_cancels_and_joins_owned_child_cleanup() {
    let f = Fixture::new("(o/'ready').write_text(str(os.getpid()))\ntime.sleep(30)");
    let settings = f.settings.clone();
    let store = f.store.clone();
    let task = tokio::spawn(async move { run(&settings, &store, CancellationToken::new()).await });
    let ready = ready(&f.store).await;
    let pid = fs::read_to_string(ready).unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !Path::new("/proc").join(&pid).exists()
                && !fs::read_dir(&f.store).unwrap().any(|e| {
                    e.unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("refresh-work-")
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.clean();
}
#[tokio::test]
async fn source_denial_and_worker_timeout_are_sanitized_outcomes() {
    for (code, expected) in [
        (3, RefreshJobOutcome::Denied),
        (124, RefreshJobOutcome::Failed(RefreshJobFailure::Timeout)),
        (7, RefreshJobOutcome::Failed(RefreshJobFailure::Worker)),
    ] {
        let f = Fixture::new(&format!(
            "print('private remote body',file=sys.stderr)\nsys.exit({code})"
        ));
        assert_eq!(f.run().await, expected);
        f.clean();
    }
}
#[tokio::test]
async fn valid_retry_floor_is_retained_but_unknown_metadata_is_not_trusted() {
    let f = Fixture::new(
        "(o/'result.json').write_text(json.dumps({'status':'retry','retry_not_before_ms':9999999999999}))\nsys.exit(2)",
    );
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::RetryAt {
            not_before_ms: 9999999999999
        }
    );
    f.clean();
    for metadata in [
        r#"{"status":"retry","retry_not_before_ms":123,"body":"untrusted"}"#,
        r#"{"status":"retry","retry_not_before_ms":-1}"#,
        r#"{"status":"retry","retry_not_before_ms":0}"#,
        r#"{"status":"other","retry_not_before_ms":123}"#,
    ] {
        let f = Fixture::new(&format!(
            "(o/'result.json').write_text({metadata:?})\nsys.exit(2)"
        ));
        assert_eq!(
            f.run().await,
            RefreshJobOutcome::Failed(RefreshJobFailure::Worker)
        );
        f.clean();
    }
}
#[tokio::test]
async fn candidate_symlink_oversize_missing_and_fifo_are_rejected_safely() {
    for body in [
        "(o/'candidate.json').symlink_to('/etc/passwd')",
        "f=(o/'candidate.json').open('wb');f.truncate(128*1024*1024+1);f.close()",
        "pass",
        "os.mkfifo(o/'candidate.json')",
    ] {
        let f = Fixture::new(body);
        let outcome = tokio::time::timeout(Duration::from_secs(5), f.run())
            .await
            .unwrap();
        assert!(matches!(outcome, RefreshJobOutcome::Failed(_)));
        f.clean();
    }
}
#[tokio::test]
async fn preexisting_symlink_quota_tree_is_rejected_without_following_or_deleting_it() {
    let f = Fixture::new("raise RuntimeError('must not start')");
    let target = f.root.join("unrelated");
    fs::write(&target, b"retain").unwrap();
    std::os::unix::fs::symlink(&target, f.store.join("link")).unwrap();
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Failed(RefreshJobFailure::Storage)
    );
    assert_eq!(fs::read(&target).unwrap(), b"retain");
    assert!(
        f.store
            .join("link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
#[tokio::test]
async fn relative_and_writable_worker_settings_are_rejected() {
    let mut f = Fixture::new("raise RuntimeError('must not start')");
    f.settings.worker = Some(PathBuf::from("relative.py"));
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Failed(RefreshJobFailure::InvalidSettings)
    );
    f.settings.worker = Some(f.root.join("worker.py"));
    fs::set_permissions(
        f.settings.worker.as_ref().unwrap(),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert_eq!(
        f.run().await,
        RefreshJobOutcome::Failed(RefreshJobFailure::InvalidSettings)
    );
}

// Separate test executables isolate the Linux subreaper setting from every other
// concurrently executing test. The guardian can reap the worker orphan itself;
// this does not depend on container PID1's zombie-reaping behavior.
#[cfg(target_os = "linux")]
#[test]
fn parent_death_supervisor_fixture() {
    let Ok(settings) = std::env::var("DW_JOB_DEATH_SETTINGS") else {
        return;
    };
    if std::env::var("DW_JOB_DEATH_MODE").as_deref() != Ok("supervisor") {
        return;
    }
    let settings: RefreshJobSettings = serde_json::from_str(&settings).unwrap();
    let store = PathBuf::from(std::env::var_os("DW_JOB_DEATH_STORE").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _ = runtime.block_on(run(&settings, &store, CancellationToken::new()));
}
#[cfg(target_os = "linux")]
#[test]
fn parent_death_guardian_fixture() {
    if std::env::var("DW_JOB_DEATH_MODE").as_deref() != Ok("guardian") {
        return;
    }
    // SAFETY: this isolated guardian owns the supervisor and becomes the worker's
    // reaper after killing it; no application process state is modified.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let store = PathBuf::from(std::env::var_os("DW_JOB_DEATH_STORE").unwrap());
    let mut supervisor = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "parent_death_supervisor_fixture", "--nocapture"])
        .env("DW_JOB_DEATH_MODE", "supervisor")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let worker = loop {
        let found = fs::read_dir(&store)
            .unwrap()
            .flatten()
            .find_map(|entry| fs::read_to_string(entry.path().join("output/ready")).ok());
        if let Some(pid) = found {
            break pid.parse::<libc::pid_t>().unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "worker did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut status = 0;
        // SAFETY: worker is the exact positive PID reported by our fixture, and
        // status points to initialized writable storage for waitpid's result.
        let reaped = unsafe { libc::waitpid(worker, &mut status, libc::WNOHANG) };
        if reaped == worker {
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker survived its parent's SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn force_killing_module_supervisor_kills_and_reaps_acquisition_worker() {
    let f = Fixture::new("(o/'ready').write_text(str(os.getpid()))\ntime.sleep(30)");
    let mut guardian = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "parent_death_guardian_fixture", "--nocapture"])
        .env("DW_JOB_DEATH_MODE", "guardian")
        .env(
            "DW_JOB_DEATH_SETTINGS",
            serde_json::to_string(&f.settings).unwrap(),
        )
        .env("DW_JOB_DEATH_STORE", &f.store)
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(12), guardian.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    // Forced kill cannot run Rust cleanup. Its own staging remains charged to
    // quota, while the process and writer lock are gone; no broad deletion occurs.
    assert!(fs::read_dir(&f.store).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("refresh-work-")
    }));
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.store.join("writer.lock"))
        .unwrap();
    writer.try_lock().unwrap();
    writer.unlock().unwrap();
}

#[tokio::test]
async fn cleanup_failure_must_not_erase_access_denial_or_retry_floor() {
    for (body, expected) in [
        ("os.chmod(o,0)\nsys.exit(3)", RefreshJobOutcome::Denied),
        (
            "(o/'result.json').write_text(json.dumps({'status':'retry','retry_not_before_ms':9999999999999}))\n(o/'blocked').mkdir();os.chmod(o/'blocked',0)\nsys.exit(2)",
            RefreshJobOutcome::RetryAt {
                not_before_ms: 9999999999999,
            },
        ),
    ] {
        let f = Fixture::new(body);
        assert_eq!(f.run().await, expected);
        for entry in fs::read_dir(&f.store).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("refresh-work-")
            {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700)).unwrap();
                if entry.path().join("output").exists() {
                    fs::set_permissions(
                        entry.path().join("output"),
                        fs::Permissions::from_mode(0o700),
                    )
                    .unwrap();
                }
                let blocked = entry.path().join("output/blocked");
                if blocked.exists() {
                    fs::set_permissions(blocked, fs::Permissions::from_mode(0o700)).unwrap();
                }
                fs::remove_dir_all(entry.path()).unwrap();
            }
        }
        f.clean();
    }
}
