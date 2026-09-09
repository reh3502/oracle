use oracle_drain_boundary::{Authority, Caller, EffectState, Ledger, Sender};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
    task::JoinSet,
    time::timeout,
};
const TTL: Duration = Duration::from_secs(5);
struct Fixture {
    addr: std::net::SocketAddr,
    received: Arc<AtomicUsize>,
    changed: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(mode: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(AtomicUsize::new(0));
        let count = received.clone();
        let changed = Arc::new(Notify::new());
        let notify = changed.clone();
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let (stream, _) = connection.unwrap();
                        children.spawn(serve(stream, count.clone(), notify.clone(), mode));
                    }
                    _ = children.join_next(), if !children.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            received,
            changed,
            task,
        }
    }
    async fn wait_for(&self, n: usize) {
        timeout(TTL, async {
            loop {
                let wait = self.changed.notified();
                tokio::pin!(wait);
                wait.as_mut().enable();
                if self.received.load(Ordering::SeqCst) >= n {
                    return;
                }
                wait.await;
            }
        })
        .await
        .unwrap();
    }
    fn count(&self) -> usize {
        self.received.load(Ordering::SeqCst)
    }
}
async fn serve(
    mut stream: tokio::net::TcpStream,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    mode: &str,
) {
    let mut body = Vec::new();
    let mut buf = [0; 1024];
    loop {
        match timeout(TTL, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => return,
            Ok(Ok(n)) => body.extend_from_slice(&buf[..n]),
        }
        if body.ends_with(b"{\"content\":\"test\"}") {
            break;
        }
        if body.len() > 4096 {
            return;
        }
    }
    let n = count.fetch_add(1, Ordering::SeqCst);
    notify.notify_waiters();
    let response = if mode == "lost" {
        return;
    } else if mode == "retry" && n == 0 {
        b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .as_slice()
    } else {
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
    };
    stream.write_all(response).await.unwrap();
}
struct Files(std::path::PathBuf);
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn sender(path: &std::path::Path, permits: usize) -> Sender {
    Sender::new(
        Authority::new(&["guild-a", "guild-b"]),
        Ledger::open(path).unwrap(),
        permits,
    )
}
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let files = Files(std::env::temp_dir().join(format!("oracle-p2-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&files.0).unwrap();
    let mut checks = Vec::new();
    // Deterministic stall before the dispatch boundary, across every entry kind.
    for caller in [Caller::Ai, Caller::Command, Caller::Job] {
        let fixture = Fixture::new("ok").await;
        let s = sender(&files.0.join(format!("stall-{caller:?}")), 0);
        let lease = s.authority.admit("guild-a", caller, TTL).unwrap();
        let work = {
            let s = s.clone();
            let addr = fixture.addr;
            tokio::spawn(async move { s.send(&lease, "queued", addr).await.unwrap() })
        };
        s.authority.quiesce("guild-a");
        assert!(s.authority.admit("guild-a", caller, TTL).is_err());
        s.authority.fence("guild-a");
        assert_eq!(
            timeout(Duration::from_millis(300), work)
                .await
                .unwrap()
                .unwrap(),
            EffectState::Fenced
        );
        assert_eq!(fixture.count(), 0);
    }
    checks
        .push("AI/command/job admission closes at quiesce; stalled writes cannot send after fence");
    // Existing work may drain before cutoff, while another guild remains admitted.
    let f = Fixture::new("ok").await;
    let s = sender(&files.0.join("drain"), 2);
    let l = s.authority.admit("guild-a", Caller::Ai, TTL).unwrap();
    s.authority.quiesce("guild-a");
    assert_eq!(
        s.send(&l, "draining", f.addr).await.unwrap(),
        EffectState::Verified
    );
    let b = s.authority.admit("guild-b", Caller::Job, TTL).unwrap();
    assert_eq!(
        s.send(&b, "unrelated", f.addr).await.unwrap(),
        EffectState::Verified
    );
    assert_eq!(f.count(), 2);
    checks.push("existing leases drain and unaffected guild continues");
    // A 429 triggers a SECOND permit wait, and must revalidate on its retry.
    let f = Fixture::new("retry").await;
    let s = sender(&files.0.join("retry"), 1);
    let l = s.authority.admit("guild-a", Caller::Command, TTL).unwrap();
    let work = {
        let s = s.clone();
        let addr = f.addr;
        tokio::spawn(async move { s.send(&l, "retry", addr).await.unwrap() })
    };
    f.wait_for(1).await;
    timeout(TTL, async {
        while s.ledger.lock().unwrap().latest("retry") != Some(EffectState::RateLimited) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    s.authority.fence("guild-a");
    assert_eq!(
        timeout(Duration::from_millis(300), work)
            .await
            .unwrap()
            .unwrap(),
        EffectState::Fenced
    );
    assert_eq!(f.count(), 1);
    checks.push("429 retry rechecks after its own rate wait; no second send after fencing");
    // Remote may have accepted bytes even if no response reaches the caller.
    let f = Fixture::new("lost").await;
    let path = files.0.join("lost");
    let s = sender(&path, 1);
    let l = s.authority.admit("guild-a", Caller::Ai, TTL).unwrap();
    assert_eq!(
        s.send(&l, "lost", f.addr).await.unwrap(),
        EffectState::UnknownOutcome
    );
    s.authority.fence("guild-a");
    assert_eq!(
        Ledger::open(&path).unwrap().latest("lost"),
        Some(EffectState::UnknownOutcome)
    );
    assert_eq!(
        s.send(&l, "lost", f.addr).await.unwrap(),
        EffectState::UnknownOutcome
    );
    assert_eq!(f.count(), 1);
    checks.push(
        "already-sent lost response stays durable unknown across reopen and cannot blindly retry",
    );
    // Crash during a later journal append must not erase earlier durable uncertainty.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"effect\":\"torn").unwrap();
        file.sync_data().unwrap();
    }
    assert_eq!(
        Ledger::open(&path).unwrap().latest("lost"),
        Some(EffectState::UnknownOutcome)
    );
    checks.push("torn final journal append preserves earlier fsynced unknown outcome");
    // Poll both calls into their initial wait before releasing any rate permit.
    {
        use std::future::Future;
        let duplicate_fixture = Fixture::new("ok").await;
        let duplicate_sender = sender(&files.0.join("same-effect"), 0);
        let lease = duplicate_sender
            .authority
            .admit("guild-a", Caller::Ai, TTL)
            .unwrap();
        let first = duplicate_sender.send(&lease, "same-effect", duplicate_fixture.addr);
        let second = duplicate_sender.send(&lease, "same-effect", duplicate_fixture.addr);
        tokio::pin!(first, second);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        duplicate_sender.rate.add_permits(2);
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap(), EffectState::Verified);
        assert_eq!(second.unwrap(), EffectState::Verified);
        assert_eq!(duplicate_fixture.count(), 1);
    }
    checks.push(
        "concurrent same-effect calls share one dispatch and cannot overwrite each other's outcome",
    );
    // Security revocation, completed and expired leases all invalidate send authority.
    for kind in ["revoke", "finished", "expired"] {
        let f = Fixture::new("ok").await;
        let s = sender(&files.0.join(kind), 1);
        let l = s
            .authority
            .admit(
                "guild-a",
                Caller::Job,
                if kind == "expired" {
                    Duration::from_millis(1)
                } else {
                    TTL
                },
            )
            .unwrap();
        match kind {
            "revoke" => s.authority.revoke("guild-a"),
            "finished" => s.authority.finish(&l),
            _ => tokio::time::sleep(Duration::from_millis(5)).await,
        }
        assert_eq!(s.send(&l, kind, f.addr).await.unwrap(), EffectState::Fenced);
        assert_eq!(f.count(), 0);
    }
    checks.push("current authorization, completion and expiry checked at send");
    let s = sender(&files.0.join("restart"), 0);
    let old = s.authority.admit("guild-b", Caller::Ai, TTL).unwrap();
    let disruption = s.authority.force_restart("guild-a");
    assert_eq!(disruption.interrupted_guilds, vec!["guild-a", "guild-b"]);
    assert!(s.authority.admit("guild-a", Caller::Ai, TTL).is_err());
    assert!(s.authority.admit("guild-b", Caller::Ai, TTL).is_ok());
    let f = Fixture::new("ok").await;
    s.rate.add_permits(1);
    assert_eq!(
        s.send(&old, "old", f.addr).await.unwrap(),
        EffectState::Fenced
    );
    assert_eq!(f.count(), 0);
    checks
        .push("one-guild forced restart reports both interrupted guilds and fences old generation");
    // Verify the escalation model against an actual shared fixture process.
    let fixture_path = std::env::args()
        .nth(2)
        .expect("pass the alpha executable as second argument");
    let runtime = oracle_process_prototype::runtime::ProcessRuntime::new().unwrap();
    let module = runtime
        .load(std::path::Path::new(&fixture_path), "fixture.alpha")
        .await
        .unwrap();
    for guild in ["guild-a", "guild-b"] {
        assert_eq!(
            module
                .invoke("echo", json!(guild), guild, TTL)
                .await
                .unwrap(),
            json!(guild)
        );
    }
    let old_pid = module.pid();
    let old_generation = module.generation();
    let stopped = module.force_stop().await.unwrap();
    assert!(stopped.cleanup_error.is_none());
    assert!(!std::path::Path::new(&format!("/proc/{old_pid}")).exists());
    let replacement = runtime
        .load(std::path::Path::new(&fixture_path), "fixture.alpha")
        .await
        .unwrap();
    assert_ne!(replacement.generation(), old_generation);
    assert!(
        module
            .invoke("echo", json!(null), "guild-b", TTL)
            .await
            .is_err()
    );
    assert_eq!(
        replacement
            .invoke("echo", json!("reactivated"), "guild-b", TTL)
            .await
            .unwrap(),
        json!("reactivated")
    );
    assert!(
        replacement
            .unload(Duration::from_millis(100))
            .await
            .unwrap()
            .cleanup_error
            .is_none()
    );
    checks.push("shared native process restart reaps old PID, fences old handle and restores other guild on new generation");
    // Race entry and force-fence repeatedly. All attempts released after fence must have zero bytes.
    for round in 0..100 {
        let f = Fixture::new("ok").await;
        let s = sender(&files.0.join(format!("race{round}")), 0);
        let mut jobs = JoinSet::new();
        for caller in [Caller::Ai, Caller::Command, Caller::Job] {
            let s = s.clone();
            let addr = f.addr;
            jobs.spawn(async move {
                match s.authority.admit("guild-a", caller, TTL) {
                    Ok(l) => s.send(&l, "race", addr).await.unwrap(),
                    Err(_) => EffectState::Fenced,
                }
            });
        }
        s.authority.quiesce("guild-a");
        s.authority.fence("guild-a");
        s.rate.add_permits(3);
        while let Some(r) = jobs.join_next().await {
            assert_eq!(r.unwrap(), EffectState::Fenced);
        }
        assert_eq!(f.count(), 0);
    }
    checks.push("100 concurrent entry/quiesce/fence rounds with zero post-fence requests");
    let report = json!({"prototype":"P2","status":"passed","checks":checks,"race_rounds":100,"disruption":disruption,"transport":"host-owned loopback HTTP; no live Discord/TLS verification"});
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "p2-report.json".into());
    std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("P2 passed: {output}");
}
