#![cfg(windows)]
//! Offline Windows acquisition supervisor checks. No source request is sent.
use dandys_world_core::refresh_job::{RefreshJobOutcome, RefreshJobSettings, run};
use oracle_module_sdk::CancellationToken;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

fn copy_private(source: &Path, target: &Path) {
    oracle_local_ipc::create_private_directory_new(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let path = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_private(&entry.path(), &path);
        } else {
            let mut file = oracle_local_ipc::create_private_file(&path).unwrap();
            file.write_all(&fs::read(entry.path()).unwrap()).unwrap();
        }
    }
}
fn worker(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    let mut file = oracle_local_ipc::create_private_file(&path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    path
}

#[tokio::test]
async fn private_windows_worker_candidates_denial_cancellation_and_hardlinks() {
    let source = PathBuf::from(
        std::env::var_os("ORACLE_TEST_PYTHON_DIR")
            .expect("set ORACLE_TEST_PYTHON_DIR to the Windows embedded Python directory"),
    );
    let root = std::env::temp_dir().join(format!("dw-refresh-win-{}", std::process::id()));
    oracle_local_ipc::create_private_directory_new(&root).unwrap();
    copy_private(&source, &root.join("python"));
    let store = root.join("store");
    oracle_local_ipc::create_private_directory_new(&store).unwrap();
    let candidate = worker(
        &root,
        "candidate.py",
        "import argparse,pathlib\np=argparse.ArgumentParser();p.add_argument('--output');p.add_argument('--budget-bytes');a=p.parse_args();o=pathlib.Path(a.output);o.mkdir();(o/'candidate.json').write_bytes(b'{}')\n",
    );
    let mut settings = RefreshJobSettings {
        enabled: true,
        source_access_qualified: true,
        python: Some(root.join("python/python.exe")),
        worker: Some(candidate),
        previous: None,
    };
    assert_eq!(
        run(&settings, &store, CancellationToken::new()).await,
        RefreshJobOutcome::Candidate(b"{}".to_vec())
    );
    settings.worker = Some(worker(&root, "denied.py", "raise SystemExit(3)\n"));
    assert_eq!(
        run(&settings, &store, CancellationToken::new()).await,
        RefreshJobOutcome::Denied
    );
    settings.worker = Some(worker(&root, "slow.py", "import time\ntime.sleep(30)\n"));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        trigger.cancel();
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), run(&settings, &store, cancel))
            .await
            .unwrap(),
        RefreshJobOutcome::Cancelled
    );
    settings.worker = Some(worker(
        &root,
        "linked.py",
        "import argparse,pathlib,os\np=argparse.ArgumentParser();p.add_argument('--output');p.add_argument('--budget-bytes');a=p.parse_args();o=pathlib.Path(a.output);o.mkdir();(o/'original').write_bytes(b'{}');os.link(o/'original',o/'candidate.json')\n",
    ));
    assert_eq!(
        run(&settings, &store, CancellationToken::new()).await,
        RefreshJobOutcome::Failed(dandys_world_core::refresh_job::RefreshJobFailure::InvalidOutput)
    );
    assert!(!fs::read_dir(&store).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("refresh-work-")
    }));
    fs::remove_dir_all(root).unwrap();
}
