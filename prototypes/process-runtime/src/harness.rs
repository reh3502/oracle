//! Executable Linux acceptance experiment, independent of runtime internals.
use crate::runtime::{ModuleHandle, ProcessRuntime, RuntimeError, StopReport};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

pub type HarnessResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const CALL: Duration = Duration::from_secs(10);
const WARMUP: usize = 5;
const RSS_SPREAD_KIB: u64 = 64 * 1024;
const RSS_SLOPE_KIB: f64 = 1024.0;
const FD_ALLOWANCE: usize = 2;

#[derive(Debug, Serialize)]
struct Sample {
    rss_kib: u64,
    fds: usize,
}
fn sample(pid: u32) -> HarnessResult<Sample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or("missing VmRSS")?
        .parse()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))?.count();
    Ok(Sample { rss_kib, fds })
}
fn require(ok: bool, message: &str) -> HarnessResult<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
fn gone(pid: u32) -> HarnessResult<()> {
    require(
        !Path::new(&format!("/proc/{pid}")).exists(),
        &format!("PID {pid} remains (including zombie)"),
    )
}
fn no_children() -> HarnessResult<()> {
    // Linux associates children with the spawning thread; inspect every task,
    // including Tokio workers, rather than only /proc/self/task/<main>/children.
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        match std::fs::read_to_string(path) {
            Ok(children) => require(
                children.trim().is_empty(),
                "host still owns child processes after cleanup",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn clean(report: &StopReport) -> HarnessResult<()> {
    require(
        report.cleanup_error.is_none(),
        &format!("cleanup fault: {:?}", report.cleanup_error),
    )?;
    gone(report.pid)
}
async fn call(module: &ModuleHandle, method: &str, input: Value) -> HarnessResult<Value> {
    Ok(module.invoke(method, input, "guild:123", CALL).await?)
}
async fn connected(module: &ModuleHandle, method: &str) -> HarnessResult<(TcpStream, Value)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let result = call(
        module,
        method,
        json!({"addr": listener.local_addr()?.to_string()}),
    )
    .await?;
    let (mut socket, _) = timeout(CALL, listener.accept()).await??;
    let mut heartbeat = [0; 10];
    timeout(CALL, socket.read_exact(&mut heartbeat)).await??;
    require(
        &heartbeat == b"heartbeat\n",
        "socket did not send actual heartbeat",
    )?;
    Ok((socket, result))
}
async fn closed(mut socket: TcpStream) -> HarnessResult<()> {
    timeout(Duration::from_secs(3), async {
        let mut bytes = [0; 1024];
        loop {
            match socket.read(&mut bytes).await {
                Ok(0) => return Ok::<_, std::io::Error>(()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    Ok(())
}
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run P1 with fixtures copied into an initially empty directory after host startup.
/// Always attempt runtime shutdown even when an assertion fails.
pub async fn run(alpha: &Path, beta: &Path, cycles: usize) -> HarnessResult<Value> {
    require(cycles >= 30, "P1 requires at least 30 measured cycles")?;
    let runtime = ProcessRuntime::new()?;
    let host_pid = std::process::id();
    let staged = Staged(std::env::temp_dir().join(format!("oracle-p1-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&staged.0)?;
    require(runtime.loaded_count() == 0, "host starts without modules")?;
    let a = staged.0.join("alpha");
    let b = staged.0.join("beta");
    require(
        !a.exists() && !b.exists(),
        "hotload destination must start absent",
    )?;
    std::fs::copy(alpha, &a)?;
    std::fs::copy(beta, &b)?;
    let result = exercise(&runtime, &a, &b, cycles, host_pid).await;
    let cleanup = runtime.shutdown().await;
    match result {
        Err(error) => Err(error),
        Ok(report) => {
            for stop in cleanup? {
                clean(&stop)?;
            }
            require(
                runtime.loaded_count() == 0,
                "runtime not empty at completion",
            )?;
            Ok(report)
        }
    }
}

async fn exercise(
    runtime: &ProcessRuntime,
    a: &Path,
    b: &Path,
    cycles: usize,
    host_pid: u32,
) -> HarnessResult<Value> {
    let alpha = runtime.load(a, "fixture.alpha").await?;
    let beta = runtime.load(b, "fixture.beta").await?;
    require(
        runtime.loaded_count() == 2,
        "both fixture binaries must be live",
    )?;
    let artifacts = json!([alpha.artifact(), beta.artifact()]);
    require(
        alpha.artifact().sha256 != beta.artifact().sha256,
        "fixtures must be distinct executable artifacts",
    )?;
    for module in [&alpha, &beta] {
        let hello = call(module, "hello", json!(null)).await?;
        require(
            hello["identity"] == module.artifact().identity && hello["pid"] == module.pid(),
            "hello identity/PID mismatch",
        )?;
        require(
            call(module, "echo", json!({"echo": 42})).await? == json!({"echo": 42}),
            "echo mismatch",
        )?;
    }
    // Each nested call goes host->guest->host->guest before returning.
    let mut calls = JoinSet::new();
    for index in 0..16 {
        let module = if index % 2 == 0 {
            alpha.clone()
        } else {
            beta.clone()
        };
        calls.spawn(async move {
            let input = json!({"index": index, "payload": "x".repeat(128 * 1024)});
            let output = call(&module, "nested", input.clone()).await?;
            require(output == input, "concurrent nested large payload mismatch")
        });
    }
    while let Some(result) = calls.join_next().await {
        result??;
    }
    for method in ["event", "job"] {
        let input = json!({"kind": method});
        require(
            call(&beta, method, input.clone()).await? == input,
            "event/job callback mismatch",
        )?;
    }
    let callbacks = alpha.accepted_callbacks() + beta.accepted_callbacks();
    require(callbacks == 18, "nested callback count incorrect")?;
    call(&alpha, "remember", json!({"old": true})).await?;
    require(
        call(&alpha, "replay", json!(null)).await.is_err(),
        "completed lease replay accepted",
    )?;
    for (field, value) in [
        ("scope", json!("guild:999")),
        ("session", json!("forged")),
        ("generation", json!(0)),
        ("lease", json!("forged")),
    ] {
        let result = call(&alpha, "forge", json!({"field": field, "value": value})).await;
        require(
            result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains("lease_fenced")),
            "forged authority was not explicitly fenced",
        )?;
    }
    require(
        alpha.accepted_callbacks() + beta.accepted_callbacks() == callbacks,
        "denied callbacks produced accepted effects",
    )?;
    require(
        call(&alpha, "stderr.flood", json!(null)).await?["bytes"] == 262_144,
        "stderr flood incomplete",
    )?;
    let (socket, _) = connected(&alpha, "socket.start").await?;
    let old_generation = alpha.generation();
    let graceful = alpha.unload(Duration::from_millis(200)).await?;
    clean(&graceful)?;
    require(
        !graceful.forced && graceful.exit_code == Some(0),
        "cooperative unload was not graceful",
    )?;
    require(
        graceful.stderr_bytes >= 262_144,
        "stderr drain lost flood bytes",
    )?;
    closed(socket).await?;
    require(
        matches!(
            alpha.invoke("echo", json!(null), "guild:123", CALL).await,
            Err(RuntimeError::Unavailable)
        ),
        "unloaded handle remains callable",
    )?;
    let replacement = runtime.load(a, "fixture.alpha").await?;
    require(
        replacement.generation() != old_generation,
        "reload reused generation",
    )?;
    require(
        matches!(
            alpha.invoke("echo", json!(null), "guild:123", CALL).await,
            Err(RuntimeError::Unavailable)
        ),
        "stale handle became valid after reload",
    )?;
    require(
        call(&replacement, "nested", json!("fresh")).await? == json!("fresh"),
        "replacement unusable",
    )?;
    let pending_module = replacement.clone();
    let pending = tokio::spawn(async move {
        pending_module
            .invoke("hang", json!(null), "guild:123", Duration::from_secs(60))
            .await
    });
    // The subsequent completed invocation establishes this generation is processing
    // traffic while the pending operation remains suspended.
    tokio::time::sleep(Duration::from_millis(30)).await;
    require(!pending.is_finished(), "hang did not remain pending")?;
    require(
        call(&replacement, "echo", json!("during hang")).await? == json!("during hang"),
        "pending handler blocked reader",
    )?;
    let cancelled = replacement.unload(Duration::from_millis(50)).await?;
    clean(&cancelled)?;
    require(
        timeout(Duration::from_secs(2), pending).await??.is_err(),
        "pending invocation did not fail on unload",
    )?;
    // Deliberately retain stdout in an orphan that also owns a live native socket.
    let (descendant_socket, descendant) = connected(&beta, "descendant.start").await?;
    let descendant_pid = descendant["pid"].as_u64().ok_or("missing descendant PID")? as u32;
    require(
        call(&beta, "crash", json!(null)).await.is_err(),
        "crash invocation unexpectedly succeeded",
    )?;
    let crashed = beta.wait_stopped(Duration::from_secs(5)).await?;
    clean(&crashed)?;
    require(crashed.exit_code == Some(71), "crash exit status lost")?;
    require(
        crashed.descendants_reaped >= 1,
        "orphan descendant was not reaped by host",
    )?;
    gone(descendant_pid)?;
    closed(descendant_socket).await?;
    drop((alpha, beta, replacement));
    require(
        runtime.loaded_count() == 0,
        "cleanup left registered generations",
    )?;

    let stubborn = runtime
        .load_with_args(b, "fixture.beta", &["--stubborn"], CALL)
        .await?;
    let (forced_socket, _) = connected(&stubborn, "socket.start").await?;
    let (forced_descendant_socket, child) = connected(&stubborn, "descendant.start").await?;
    let forced_descendant_pid = child["pid"]
        .as_u64()
        .ok_or("missing forced descendant PID")? as u32;
    let forced = stubborn.unload(Duration::from_millis(50)).await?;
    clean(&forced)?;
    require(
        forced.forced,
        "stubborn process did not require forced cleanup",
    )?;
    require(
        forced.descendants_reaped >= 1,
        "forced cleanup did not reap descendant",
    )?;
    gone(forced_descendant_pid)?;
    closed(forced_socket).await?;
    closed(forced_descendant_socket).await?;
    drop(stubborn);
    let mut rejected_loads = Vec::new();
    for flag in ["--bad-protocol", "--bad-init", "--no-hello"] {
        let error = match runtime
            .load_with_args(a, "fixture.alpha", &[flag], Duration::from_millis(100))
            .await
        {
            Ok(module) => {
                module.force_stop().await?;
                return Err(format!("invalid fixture launch {flag} was admitted").into());
            }
            Err(error) => error,
        };
        require(
            runtime.loaded_count() == 0,
            "failed handshake leaked runtime generation",
        )?;
        rejected_loads.push(json!({"flag": flag, "error": error.to_string()}));
        no_children()?;
    }

    // Dropping a load future is different from an ordinary handshake error.
    // The owner guard must trigger cleanup without a returned ModuleHandle.
    require(
        timeout(
            Duration::from_millis(30),
            runtime.load_with_args(a, "fixture.alpha", &["--no-hello"], CALL),
        )
        .await
        .is_err(),
        "cancelled load unexpectedly finished",
    )?;
    timeout(Duration::from_secs(5), async {
        while runtime.loaded_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    no_children()?;

    for _ in 0..WARMUP {
        cycle(runtime, a, b).await?;
    }
    let baseline = sample(host_pid)?;
    let mut host_samples = Vec::new();
    let mut live_samples = Vec::new();
    for index in 0..cycles {
        let path = if index % 2 == 0 { a } else { b };
        let identity = if index % 2 == 0 {
            "fixture.alpha"
        } else {
            "fixture.beta"
        };
        let module = runtime.load(path, identity).await?;
        require(
            call(&module, "nested", json!({"cycle": index})).await? == json!({"cycle": index}),
            "cycle callback mismatch",
        )?;
        live_samples.push(
            json!({"cycle": index, "host": sample(host_pid)?, "child": sample(module.pid())?}),
        );
        let stop = module.unload(Duration::from_millis(100)).await?;
        clean(&stop)?;
        drop(module);
        require(
            runtime.loaded_count() == 0,
            "cycle leaked runtime registration",
        )?;
        host_samples.push(sample(host_pid)?);
        no_children()?;
    }
    let mut rss: Vec<_> = host_samples.iter().map(|s| s.rss_kib).collect();
    let n = rss.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = rss.iter().sum::<u64>() as f64 / n;
    let slope = rss
        .iter()
        .enumerate()
        .map(|(i, y)| (i as f64 - mean_x) * (*y as f64 - mean_y))
        .sum::<f64>()
        / (0..rss.len())
            .map(|i| (i as f64 - mean_x).powi(2))
            .sum::<f64>();
    rss.sort_unstable();
    let minimum = rss[0].min(baseline.rss_kib);
    let maximum = rss[rss.len() - 1].max(baseline.rss_kib);
    let median = (rss[(rss.len() - 1) / 2] + rss[rss.len() / 2]) as f64 / 2.0;
    let max_fds = host_samples
        .iter()
        .map(|s| s.fds)
        .max()
        .ok_or("missing FD samples")?;
    require(
        maximum - minimum <= RSS_SPREAD_KIB,
        "RSS spread exceeded predeclared 64 MiB bound",
    )?;
    require(
        slope <= RSS_SLOPE_KIB,
        "RSS growth exceeded predeclared 1 MiB/cycle bound",
    )?;
    require(
        max_fds <= baseline.fds + FD_ALLOWANCE,
        "FD count exceeded baseline + 2",
    )?;
    require(std::process::id() == host_pid, "host restarted")?;
    let executable = std::env::current_exe()?;
    let host_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&executable)?));
    Ok(json!({
        "schema": 1, "status": "passed", "scope": "Stage 0 P1 trusted Linux process prototype; not P2 or a production SDK",
        "host": {"pid": host_pid, "executable": executable, "sha256": host_sha256},
        "artifacts": artifacts,
        "checks": ["hotload two distinct binaries after host startup", "hello and echo", "16 concurrent nested 128 KiB callbacks", "event and job callbacks", "completed lease replay denied", "forged scope/session/generation/lease denied", "262144-byte stderr drain", "live TCP task closes on graceful unload", "old handles fenced across reload", "pending handler settles on unload", "crash cleanup independent of stdout EOF", "orphan PID reaped and native socket closed", "forced leader and descendant cleanup", "rejected protocol/init/handshake-timeout cleanup", "OS child list empty after cleanup", "cancelled load future cleans unreturned generation", "bounded repeated-cycle RSS/FD"],
        "nested_accepted_callbacks": callbacks,
        "stops": {"graceful": graceful, "pending_cancelled": cancelled, "crashed": crashed, "forced": forced},
        "rejected_loads": rejected_loads,
        "resource_experiment": {"warmup_rounds": WARMUP, "warmup_generations": 2 * WARMUP, "measured_cycles": cycles,
            "bounds": {"rss_spread_kib": RSS_SPREAD_KIB, "rss_slope_kib_per_cycle": RSS_SLOPE_KIB, "fd_baseline_allowance": FD_ALLOWANCE},
            "baseline": baseline, "after_cleanup": host_samples, "while_loaded": live_samples,
            "rss_min_kib": minimum, "rss_max_kib": maximum, "rss_median_kib": median,
            "rss_linear_slope_kib_per_cycle": slope, "max_fds": max_fds}
    }))
}
async fn cycle(runtime: &ProcessRuntime, a: &Path, b: &Path) -> HarnessResult<()> {
    for (path, identity) in [(a, "fixture.alpha"), (b, "fixture.beta")] {
        let module = runtime.load(path, identity).await?;
        require(
            call(&module, "nested", json!("warmup")).await? == json!("warmup"),
            "warmup callback mismatch",
        )?;
        clean(&module.unload(Duration::from_millis(100)).await?)?;
    }
    Ok(())
}
