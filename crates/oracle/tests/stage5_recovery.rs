//! Release failure drills through the real executable. All state is temporary;
//! no provider, Discord, deployment, or external database is contacted.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Deployment {
    root: TempDir,
    config: PathBuf,
}
impl Deployment {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("oracle.json");
        Self { root, config }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oracle"));
        // Clear inherited credentials and provider configuration, including names
        // chosen by an operator rather than the conventional environment names.
        command.env_clear().env("PATH", "/usr/bin:/bin");
        command
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        Process(Some(self.command(args).spawn().unwrap())).finish()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn configure(&self) {
        self.ok(&["init"]);
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&self.config).unwrap()).unwrap();
        config["guilds"] = json!([{"guild":"100", "operators":["10"]}]);
        std::fs::write(&self.config, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    fn serve(&self) -> Process {
        let mut process = Process(Some(self.command(&["serve"]).spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(20);
        while UnixStream::connect(self.root.path().join("state/control.sock")).is_err() {
            assert!(
                process.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "host exited before readiness"
            );
            assert!(Instant::now() < deadline, "host readiness timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        process
    }
}
struct Process(Option<Child>);
impl Process {
    fn finish(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "CLI did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
    fn kill(mut self) {
        self.0.as_mut().unwrap().kill().unwrap();
        assert!(!self.finish().status.success());
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn error(output: Output, code: &str) {
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"result":null,"error":code})
    );
    assert!(
        output.stderr.is_empty(),
        "failure disclosed unstructured diagnostic data"
    );
}

#[test]
fn sigkill_releases_host_lock_and_cold_restart_replaces_stale_socket() {
    let deployment = Deployment::new();
    deployment.configure();
    let before = deployment.ok(&["status", "--guild", "100"]);
    let host = deployment.serve();
    deployment.ok(&["control", "--guild", "100", "pause"]);
    error(deployment.run(&["serve"]), "already_running");
    host.kill();
    assert!(
        deployment.root.path().join("state/control.sock").exists(),
        "SIGKILL must leave the socket cleanup unexecuted"
    );
    let restarted = deployment.serve();
    let after = deployment.ok(&["status", "--guild", "100"]);
    assert_eq!(after["deployment"], before["deployment"]);
    assert_eq!(after["guilds"][0]["paused"], true);
    assert_eq!(after["modules_loaded"], 0);
    assert_eq!(after["ai_available"], false);
    restarted.kill();
    // The offline path must also recover after the second abrupt termination.
    assert_eq!(
        deployment.ok(&["status"])["deployment"],
        before["deployment"]
    );
}

#[test]
fn backup_output_io_failure_preserves_host_and_durable_state() {
    let deployment = Deployment::new();
    deployment.configure();
    let host = deployment.serve();
    deployment.ok(&["control", "--guild", "100", "pause"]);
    let before = deployment.ok(&["status", "--guild", "100"]);
    let obstruction = deployment.root.path().join("private-path-canary");
    std::fs::write(&obstruction, b"unchanged").unwrap();
    let destination = obstruction.join("backup");
    error(
        deployment.run(&["backup", "--output", destination.to_str().unwrap()]),
        "io",
    );
    assert_eq!(std::fs::read(obstruction).unwrap(), b"unchanged");
    let after = deployment.ok(&["status", "--guild", "100"]);
    assert_eq!(after["deployment"], before["deployment"]);
    assert_eq!(after["guilds"], before["guilds"]);
    // Once the failed destination is avoided, a native backup still succeeds.
    deployment.ok(&[
        "backup",
        "--output",
        deployment.root.path().join("good-backup").to_str().unwrap(),
    ]);
    host.kill();
}

#[test]
fn corrupted_native_backup_is_rejected_before_target_database_creation() {
    let source = Deployment::new();
    source.configure();
    let before = source.ok(&["status"]);
    let bundle = source.root.path().join("backup");
    source.ok(&["backup", "--output", bundle.to_str().unwrap()]);
    let database = std::fs::read_dir(&bundle)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            std::fs::read(path)
                .unwrap()
                .starts_with(b"SQLite format 3\0")
        })
        .unwrap();
    let original = std::fs::read(&database).unwrap();
    std::fs::write(&database, b"corrupt-secret-canary").unwrap();
    let target = Deployment::new();
    std::fs::copy(&source.config, &target.config).unwrap();
    error(
        target.run(&["restore", "--backup", bundle.to_str().unwrap()]),
        "integrity",
    );
    assert!(!target.root.path().join("state/oracle.sqlite").exists());
    assert_eq!(source.ok(&["status"])["deployment"], before["deployment"]);
    // Repairing the bundle permits an isolated retry, with a fresh identity.
    std::fs::write(database, original).unwrap();
    let restored = target.ok(&["restore", "--backup", bundle.to_str().unwrap()]);
    assert_ne!(restored["deployment"], before["deployment"]);
    assert_eq!(restored["guilds"][0]["paused"], true);
}

#[test]
fn malformed_configuration_diagnostics_do_not_disclose_input_or_private_paths() {
    let deployment = Deployment::new();
    // Serde's original error contains this unknown field; the process boundary
    // must expose only the stable error code, in both stdout and stderr.
    std::fs::write(
        &deployment.config,
        br#"{"secret-canary-provider-key":"secret-canary-token"}"#,
    )
    .unwrap();
    error(deployment.run(&["status"]), "invalid_input");
    assert!(!deployment.root.path().join("state").exists());
}
