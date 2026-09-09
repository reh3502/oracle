//! Real CLI/Unix-socket/native SQLite exercises. No live Discord, provider or
//! PostgreSQL credentials are used; PostgreSQL is covered by its isolated suite.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WAIT: Duration = Duration::from_secs(20);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "oracle-cli-{}-{}-{nanos}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> PathBuf {
        self.0.join("oracle.json")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oracle"));
    command
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in [
        "GEMINI_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "DISCORD_TOKEN",
        "DISCORD_BOT_TOKEN",
        "ORACLE_TEST_POSTGRES_URL",
    ] {
        command.env_remove(key);
    }
    command
}

struct Running(Option<Child>);
impl Running {
    fn spawn(command: &mut Command) -> Self {
        Self(Some(command.spawn().expect("CLI executable starts")))
    }
    fn pid(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }
    fn exited(&mut self) -> bool {
        self.0.as_mut().unwrap().try_wait().unwrap().is_some()
    }
    fn collect(mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        while !self.exited() {
            assert!(
                Instant::now() < deadline,
                "CLI subprocess did not exit within the test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
    fn terminate(self) -> Output {
        assert!(
            Command::new("kill")
                .arg("-TERM")
                .arg(self.pid().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        self.collect()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn invoke<I, S>(config: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let output = Running::spawn(command(config).args(&args)).collect();
    if !output.status.success() {
        let error = serde_json::from_slice::<Value>(&output.stdout)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned));
        eprintln!(
            "CLI test command {:?} exited {:?} with structured error {:?}",
            args.first(),
            output.status.code(),
            error
        );
    }
    output
}
fn successful(output: Output) -> Value {
    assert!(output.status.success(), "CLI command failed");
    serde_json::from_slice(&output.stdout).expect("CLI returns one JSON value")
}
fn rejected(output: Output) -> Value {
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let value: Value =
        serde_json::from_slice(&output.stdout).expect("CLI error is structured JSON");
    assert!(value["error"].is_string());
    assert!(value["result"].is_null());
    value
}
fn configure_guild(config: &Path) {
    let mut value: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    value["guilds"] = json!([{"guild":"100","operators":["10"]}]);
    assert!(value["discord"].is_null());
    std::fs::write(config, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}
fn wait_for_socket(running: &mut Running, socket: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(!running.exited(), "host exited before socket readiness");
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "host socket readiness timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn bundle_files(bundle: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(bundle)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "backup bundle contains an unexpected entry"
            );
            (
                entry.file_name().into(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn init_is_empty_without_credentials_and_does_not_overwrite_config() {
    let sandbox = Sandbox::new();
    let config = sandbox.config();
    let status = successful(invoke(&config, ["init"]));
    assert_eq!(status["modules_loaded"], 0);
    assert_eq!(status["ai_available"], false);
    assert_eq!(status["guilds"], json!([]));
    assert_eq!(status["recovery_required"], 0);
    assert!(
        status["deployment"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    let original = std::fs::read(&config).unwrap();
    rejected(invoke(&config, ["init"]));
    assert_eq!(std::fs::read(&config).unwrap(), original);
    let status_again = successful(invoke(&config, ["status"]));
    assert_eq!(status_again["deployment"], status["deployment"]);
}

#[test]
fn serve_socket_policy_exclusive_ownership_and_joined_sigterm_shutdown() {
    let sandbox = Sandbox::new();
    let config = sandbox.config();
    successful(invoke(&config, ["init"]));
    configure_guild(&config);
    let socket = sandbox.0.join("state/control.sock");
    let mut host = Running::spawn(command(&config).arg("serve"));
    wait_for_socket(&mut host, &socket);
    // Offline fallback cannot acquire the lock while serve owns it, so successful
    // CLI commands here establish actual request/reply through the live socket.
    let status = successful(invoke(&config, ["status", "--guild", "100"]));
    assert_eq!(status["guilds"].as_array().unwrap().len(), 1);
    assert_eq!(status["guilds"][0]["guild"], "100");
    let observed_revision = status["guilds"][0]["revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let paused = successful(invoke(
        &config,
        [
            "control",
            "--guild",
            "100",
            "pause",
            "--expected-revision",
            observed_revision.as_str(),
        ],
    ));
    assert_eq!(paused["guild"]["paused"], true);
    let forbidden = rejected(invoke(&config, ["control", "--guild", "200", "pause"]));
    assert_eq!(forbidden["error"], "forbidden_scope");
    let second = rejected(invoke(&config, ["serve"]));
    assert_eq!(second["error"], "already_running");
    assert!(socket.exists());
    let mut incomplete = UnixStream::connect(&socket).unwrap();
    incomplete.write_all(b"{\"command\":").unwrap();
    // A later accepted status connection demonstrates that the earlier stalled
    // connection is inside the server's tracked local-control work.
    let status = successful(invoke(&config, ["status", "--guild", "100"]));
    assert_eq!(status["guilds"][0]["paused"], true);
    let output = host.terminate();
    assert!(output.status.success(), "SIGTERM did not exit cleanly");
    let events: Vec<Value> = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<Value>()
        .collect::<Result<_, _>>()
        .expect("host emits JSON events");
    let ready = events
        .iter()
        .find(|event| event["event"] == "ready")
        .expect("ready event");
    assert_eq!(ready["discord_connected"], false);
    assert_eq!(ready["status"]["modules_loaded"], 0);
    assert_eq!(ready["status"]["ai_available"], false);
    let stopped = events
        .iter()
        .find(|event| event["event"] == "stopped")
        .expect("stopped event");
    assert_eq!(stopped["tasks"]["stats"]["lifecycle"], "Closed");
    assert_eq!(stopped["tasks"]["stats"]["counts"]["running"], 0);
    assert_eq!(stopped["tasks"]["forced"], false);
    assert!(!socket.exists());
    incomplete
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0_u8; 1];
    assert_eq!(incomplete.read(&mut byte).unwrap(), 0);
    let after = successful(invoke(&config, ["status", "--guild", "100"]));
    assert_eq!(after["guilds"][0]["paused"], true);
}

#[test]
fn native_sqlite_backup_isolated_restore_rotates_identity_and_pauses_mutations() {
    let source = Sandbox::new();
    let config = source.config();
    successful(invoke(&config, ["init"]));
    configure_guild(&config);
    let before = successful(invoke(&config, ["status", "--guild", "100"]));
    assert_eq!(before["guilds"][0]["paused"], false);
    let bundle = source.0.join("backup");
    successful(invoke(
        &config,
        [
            OsStr::new("backup"),
            OsStr::new("--output"),
            bundle.as_os_str(),
        ],
    ));
    let original_bundle = bundle_files(&bundle);
    assert!(original_bundle.contains_key(Path::new("manifest.json")));
    assert_eq!(
        original_bundle
            .values()
            .filter(|bytes| bytes.starts_with(b"SQLite format 3\0"))
            .count(),
        1,
        "bundle must contain a native SQLite database"
    );
    rejected(invoke(
        &config,
        [
            OsStr::new("backup"),
            OsStr::new("--output"),
            bundle.as_os_str(),
        ],
    ));
    assert_eq!(bundle_files(&bundle), original_bundle);
    let target = Sandbox::new();
    let restored_config = target.config();
    std::fs::copy(&config, &restored_config).unwrap();
    let restored = successful(invoke(
        &restored_config,
        [
            OsStr::new("restore"),
            OsStr::new("--backup"),
            bundle.as_os_str(),
        ],
    ));
    assert_ne!(restored["deployment"], before["deployment"]);
    assert_eq!(restored["guilds"][0]["guild"], "100");
    assert_eq!(restored["guilds"][0]["paused"], true);
    assert_eq!(restored["modules_loaded"], 0);
    assert_eq!(restored["ai_available"], false);
    rejected(invoke(
        &restored_config,
        [
            OsStr::new("restore"),
            OsStr::new("--backup"),
            bundle.as_os_str(),
        ],
    ));
    let after = successful(invoke(&restored_config, ["status", "--guild", "100"]));
    assert_eq!(after["deployment"], restored["deployment"]);
    assert_eq!(after["guilds"][0]["paused"], true);
    let source_after = successful(invoke(&config, ["status", "--guild", "100"]));
    assert_eq!(source_after["deployment"], before["deployment"]);
    assert_eq!(source_after["guilds"][0]["paused"], false);
}

#[test]
fn missing_discord_secret_fails_before_socket_publication_and_releases_ownership() {
    let sandbox = Sandbox::new();
    let config = sandbox.config();
    let initial = successful(invoke(&config, ["init"]));
    let mut data: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    data["discord"] = json!({"token_env":"DISCORD_TOKEN"});
    std::fs::write(&config, serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(
        rejected(invoke(&config, ["serve"]))["error"],
        "invalid_input"
    );
    assert!(!sandbox.0.join("state/control.sock").exists());
    let after = successful(invoke(&config, ["status"]));
    assert_eq!(after["deployment"], initial["deployment"]);
    assert_eq!(after["modules_loaded"], 0);
}
