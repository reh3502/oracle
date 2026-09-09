//! Real native RPC -> journal -> permit-guarded loopback TCP dispatch.
//! Every absence assertion is made after sender shutdown and receiver EOF.
use async_trait::async_trait;
use oracle_core::*;
use oracle_modules::{ModuleManager, Observation, SendTransport};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
const BOUND: Duration = Duration::from_secs(10);
#[derive(Debug, PartialEq)]
enum Event {
    Ready(usize),
    Observe(usize),
}
#[derive(Clone, Copy)]
enum Mode {
    Verified,
    Pending,
    RetryFirst,
}
struct Wire {
    stream: Mutex<Option<TcpStream>>,
    gates: Semaphore,
    events: mpsc::Sender<Event>,
    ready: AtomicUsize,
    observed: AtomicUsize,
    sent: AtomicUsize,
    mode: Mode,
}
#[async_trait]
impl SendTransport for Wire {
    async fn ready(&self) -> Result<()> {
        let n = self.ready.fetch_add(1, Ordering::SeqCst) + 1;
        self.events
            .send(Event::Ready(n))
            .await
            .map_err(|_| Error::new(ErrorCode::Io))?;
        self.gates
            .acquire()
            .await
            .map_err(|_| Error::new(ErrorCode::Cancelled))?
            .forget();
        Ok(())
    }
    fn dispatch(&self, body: &Value) -> Result<Value> {
        let mut bytes = serde_json::to_vec(body).unwrap();
        bytes.push(b'\n');
        let stream = self.stream.lock().unwrap();
        // A real nonblocking syscall happens synchronously while DispatchPermit holds
        // its authority lock. No async send future is constructed or left unpolled.
        let written = stream
            .as_ref()
            .unwrap()
            .try_write(&bytes)
            .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
        assert_eq!(
            written,
            bytes.len(),
            "tiny loopback test payload was only partially sent"
        );
        self.sent.fetch_add(1, Ordering::SeqCst);
        Ok(body.clone())
    }
    async fn observe(&self, _receipt: Value) -> Result<Observation> {
        let n = self.observed.fetch_add(1, Ordering::SeqCst) + 1;
        self.events
            .send(Event::Observe(n))
            .await
            .map_err(|_| Error::new(ErrorCode::Io))?;
        if matches!(self.mode, Mode::Verified) {
            Ok(Observation::Verified(_receipt))
        } else if matches!(self.mode, Mode::RetryFirst) && n == 1 {
            // The controlled receiver represents a definite rejection, never an
            // ambiguous timeout. Only this explicit observation permits a retry.
            Ok(Observation::RetryAfter(Duration::ZERO))
        } else {
            std::future::pending().await
        }
    }
}
struct Fixture {
    scratch: PathBuf,
    storage: Arc<Storage>,
    core: Arc<CoreService>,
    manager: Arc<ModuleManager>,
    wire: Arc<Wire>,
    events: mpsc::Receiver<Event>,
    receiver: JoinHandle<Vec<u8>>,
    module: ModuleId,
    guild: GuildId,
    digest: String,
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let scratch =
            std::env::temp_dir().join(format!("oracle-effect-wire-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&scratch).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        stream.writable().await.unwrap();
        let (mut received, _) = listener.accept().await.unwrap();
        drop(listener);
        let receiver = tokio::spawn(async move {
            let mut bytes = Vec::new();
            received.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let (events, rx) = mpsc::channel(8);
        let wire = Arc::new(Wire {
            stream: Mutex::new(Some(stream)),
            gates: Semaphore::new(0),
            events,
            ready: AtomicUsize::new(0),
            observed: AtomicUsize::new(0),
            sent: AtomicUsize::new(0),
            mode,
        });
        let storage = Arc::new(
            Storage::open(DatabaseConfig::Sqlite {
                path: scratch.join("state.sqlite"),
            })
            .await
            .unwrap(),
        );
        let guild: GuildId = "123".parse().unwrap();
        let module: ModuleId = "fixture.counter".parse().unwrap();
        storage
            .initialize_guilds(std::slice::from_ref(&guild))
            .await
            .unwrap();
        let core = Arc::new(CoreService::new(
            storage.clone(),
            vec![GuildPolicy {
                guild: guild.clone(),
                operators: vec![],
            }],
        ));
        let manager = ModuleManager::with_transport(
            storage.clone(),
            core.clone(),
            scratch.join("artifacts"),
            wire.clone(),
        )
        .unwrap();
        let package_dir = scratch.join("counter");
        std::fs::create_dir(&package_dir).unwrap();
        let bytes = std::fs::read(root.join("target/debug/oracle-example-counter"))
            .expect("build default counter executable first");
        std::fs::write(package_dir.join("module"), &bytes).unwrap();
        let manifest: ModuleManifest = serde_json::from_slice(
            &std::fs::read(root.join("examples/modules/counter/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.data_version, 1);
        let package = ModulePackage {
            manifest,
            entrypoint: "module".into(),
            files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
            source_revision: "real-wire-test".into(),
            toolchain: "default cargo fixture build".into(),
            license: "test-only".into(),
        };
        std::fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        let installed = manager.install(&package_dir, true).await.unwrap();
        manager
            .load(&installed.digest)
            .await
            .expect("counter executable must match default manifest, including echo");
        manager
            .activate(
                &PolicyContext::LocalOperator,
                DesiredActivation {
                    module: module.clone(),
                    guild: guild.clone(),
                    active: true,
                    grants: vec!["host.echo".into()],
                    bindings: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        Self {
            scratch,
            storage,
            core,
            manager,
            wire,
            events: rx,
            receiver,
            module,
            guild,
            digest: installed.digest,
        }
    }
    fn invoke(&self, purpose: &str) -> JoinHandle<Result<Value>> {
        let manager = self.manager.clone();
        let module = self.module.clone();
        let guild = self.guild.clone();
        let purpose = purpose.to_string();
        tokio::spawn(async move {
            manager
                .invoke(
                    &PolicyContext::LocalOperator,
                    &module,
                    &guild,
                    "echo",
                    json!({"purpose":purpose,"body":{"message":"one"}}),
                )
                .await
        })
    }
    async fn event(&mut self, expected: Event) {
        assert_eq!(
            tokio::time::timeout(BOUND, self.events.recv())
                .await
                .expect("transport barrier timed out"),
            Some(expected)
        );
    }
    async fn unload(&self) {
        tokio::time::timeout(BOUND, self.manager.unload(&self.module, Duration::ZERO))
            .await
            .expect("unload timed out")
            .unwrap();
    }
    async fn unknown(&self) {
        tokio::time::timeout(BOUND, async {
            loop {
                let effects = self
                    .core
                    .recovery(&PolicyContext::LocalOperator, &self.guild, 100)
                    .await
                    .unwrap();
                if effects.iter().any(|e| e.state == EffectState::Unknown) {
                    assert_eq!(effects.len(), 1);
                    break;
                }
                // Journal completion is independently owned after RPC cancellation.
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("independent journal owner did not resolve Sent to Unknown");
    }
    async fn finish(self, expected_messages: usize) {
        tokio::time::timeout(BOUND, self.manager.shutdown())
            .await
            .expect("manager shutdown timed out")
            .unwrap();
        self.storage.close().await.unwrap();
        drop(self.wire.stream.lock().unwrap().take());
        let bytes = tokio::time::timeout(BOUND, self.receiver)
            .await
            .expect("TCP receiver did not reach EOF")
            .unwrap();
        let expected = b"{\"message\":\"one\"}\n".repeat(expected_messages);
        assert_eq!(
            bytes, expected,
            "actual receiver bytes differ from authorized dispatches"
        );
        assert_eq!(self.wire.sent.load(Ordering::SeqCst), expected_messages);
        fn writable_directories(path: &std::path::Path) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    writable_directories(&entry.path());
                }
            }
        }
        writable_directories(&self.scratch);
        std::fs::remove_dir_all(self.scratch).unwrap();
    }
}
async fn failed(call: JoinHandle<Result<Value>>) {
    assert!(
        tokio::time::timeout(BOUND, call)
            .await
            .expect("invocation did not end")
            .unwrap()
            .is_err()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires cargo build -p oracle-example-counter (default profile)"]
async fn queued_readiness_revoked_by_forced_unload_never_sends() {
    let mut f = Fixture::new(Mode::Pending).await;
    let call = f.invoke("queued");
    f.event(Event::Ready(1)).await;
    f.unload().await;
    f.wire.gates.add_permits(1);
    failed(call).await;
    f.finish(0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires cargo build -p oracle-example-counter (default profile)"]
async fn definite_rejection_retry_fenced_before_second_ready_never_resends() {
    let mut f = Fixture::new(Mode::RetryFirst).await;
    let call = f.invoke("retry");
    f.event(Event::Ready(1)).await;
    f.wire.gates.add_permits(1);
    f.event(Event::Observe(1)).await;
    f.event(Event::Ready(2)).await;
    f.unload().await;
    f.wire.gates.add_permits(1);
    failed(call).await;
    f.finish(1).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires cargo build -p oracle-example-counter (default profile)"]
async fn sent_then_forced_unload_becomes_unknown_and_reload_same_purpose_never_resends() {
    let mut f = Fixture::new(Mode::Pending).await;
    let call = f.invoke("sent");
    f.event(Event::Ready(1)).await;
    f.wire.gates.add_permits(1);
    f.event(Event::Observe(1)).await;
    f.unload().await;
    failed(call).await;
    f.unknown().await;
    f.manager.load(&f.digest).await.unwrap();
    // load restores persisted desired activation; reusing the purpose must stop in
    // the core journal before transport readiness or a second syscall is reached.
    f.wire.gates.add_permits(1);
    failed(f.invoke("sent")).await;
    assert_eq!(
        f.wire.ready.load(Ordering::SeqCst),
        1,
        "unknown reservation reentered sender readiness"
    );
    f.finish(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires cargo build -p oracle-example-counter (default profile)"]
async fn verified_echo_records_one_real_wire_message_and_returns_receipt() {
    let mut f = Fixture::new(Mode::Verified).await;
    let call = f.invoke("verified");
    f.event(Event::Ready(1)).await;
    f.wire.gates.add_permits(1);
    f.event(Event::Observe(1)).await;
    assert_eq!(
        tokio::time::timeout(BOUND, call)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        json!({"message":"one"})
    );
    assert!(
        f.core
            .recovery(&PolicyContext::LocalOperator, &f.guild, 100)
            .await
            .unwrap()
            .is_empty()
    );
    f.finish(1).await;
}
