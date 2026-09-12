//! Offline Linux process-runtime qualification fixture; not a module SDK example.
use oracle_process::ProcessRuntime;
use oracle_rpc::{RpcError, RpcHandler, RpcPeer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

static STUBBORN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct Guest;
#[async_trait::async_trait]
impl RpcHandler for Guest {
    async fn handle(
        &self,
        _peer: RpcPeer,
        method: String,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method.as_str() {
            "hello" => {
                STUBBORN.store(
                    params["cycle"].as_u64().unwrap() % 3 == 1,
                    std::sync::atomic::Ordering::Relaxed,
                );
                Ok(params)
            }
            "echo" | "shutdown" => Ok(params),
            "descendant" => {
                // The supervisor, acting as subreaper, owns this intentionally orphaned child.
                #[allow(clippy::zombie_processes)]
                let child = std::process::Command::new("/bin/sleep")
                    .arg("300")
                    .spawn()
                    .map_err(|_| RpcError::Remote("fixture spawn failed".into()))?;
                Ok(json!(child.id()))
            }
            "crash" => std::process::exit(23),
            _ => Err(RpcError::Remote("unsupported fixture operation".into())),
        }
    }
}
fn resources(pid: u32) -> Value {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .count();
    json!({"rss_kib": rss, "fds": fds})
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 1 {
        let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), Arc::new(Guest));
        peer.wait_closed().await;
        if STUBBORN.load(std::sync::atomic::Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        return;
    }
    assert_eq!(
        args.len(),
        4,
        "usage: process_soak CYCLES CALLS PAYLOAD_BYTES"
    );
    let cycles = args[1].parse::<usize>().unwrap();
    let calls = args[2].parse::<usize>().unwrap();
    let payload = "x".repeat(args[3].parse::<usize>().unwrap());
    let executable = std::env::current_exe().unwrap();
    let digest = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
    let owner = Arc::new(ProcessRuntime::new().unwrap());
    let runtime = owner.clone();
    // Preserve supervision even when a workload assertion fails or RPC is interrupted.
    let workload = tokio::spawn(async move {
        let start = Instant::now();
        for cycle in 0..cycles {
            let process = runtime
                .spawn(
                    &executable,
                    &digest,
                    json!({"cycle": cycle}),
                    Arc::new(Guest),
                )
                .await
                .unwrap();
            assert_eq!(process.hello(), &json!({"cycle": cycle}));
            let descendant = process
                .call("descendant", json!({}), Duration::from_secs(2))
                .await
                .unwrap()
                .as_u64()
                .unwrap();
            for call in 0..calls {
                let request = json!({"sequence": call, "payload": payload});
                assert_eq!(
                    process
                        .call("echo", request.clone(), Duration::from_secs(2))
                        .await
                        .unwrap(),
                    request
                );
            }
            let guest = resources(process.pid());
            let stop_start = Instant::now();
            let mode = ["graceful", "forced", "crash"][cycle % 3];
            let report = match mode {
                "graceful" => process.stop(Duration::from_secs(1)).await.unwrap(),
                "forced" => process.force_stop().await.unwrap(),
                _ => {
                    assert!(
                        process
                            .call("crash", json!({}), Duration::from_secs(2))
                            .await
                            .is_err()
                    );
                    process.wait_stopped(Duration::from_secs(5)).await.unwrap()
                }
            };
            assert!(report.cleanup_error.is_none(), "{report:?}");
            if mode == "forced" {
                assert!(report.forced);
            }
            if mode == "crash" {
                assert_eq!(report.exit_code, Some(23));
            }
            assert!(report.descendants_reaped >= 1);
            assert!(!Path::new(&format!("/proc/{}", process.pid())).exists());
            assert!(!Path::new(&format!("/proc/{descendant}")).exists());
            assert_eq!(runtime.loaded_count(), 0);
            let stop_ms = stop_start.elapsed().as_secs_f64() * 1000.0;
            drop(process);
            // Allow completed supervisor and transport futures to release allocations.
            tokio::time::sleep(Duration::from_millis(10)).await;
            println!(
                "{}",
                json!({"cycle": cycle, "mode": mode, "host": resources(std::process::id()), "guest": guest, "stop_ms": stop_ms, "stop": report, "elapsed_seconds": start.elapsed().as_secs_f64()})
            );
        }
    });
    let outcome = workload.await;
    owner.shutdown().await.unwrap();
    assert_eq!(owner.loaded_count(), 0);
    outcome.unwrap();
}
