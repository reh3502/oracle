# P1 acceptance report — 2026-09-08

**Result: confirmed for the Stage 0 P1 trusted Linux process prototype.** The executable experiment passes the roadmap's handshake, nested bidirectional RPC, background socket, crash, hot-load/unload, process-reaping, stale-handle and finite resource-trend gates. This resolves P1; it does not declare the bot, SDK or P2 complete.

## Acceptance evidence

| Requirement | Implementation and direct evidence | Result |
| --- | --- | --- |
| Two ordinary Rust executable fixtures | Distinct alpha/beta artifacts handshake and echo; both are copied to new paths after the empty host starts and then loaded | Confirmed |
| No host restart during hot load/unload | One runtime and host PID throughout the experiment; replacement generation performs a fresh nested call | Confirmed |
| Nested bidirectional RPC without pipe deadlock | 16 concurrent 128 KiB calls complete through host → guest → host → guest; event/job callbacks return; 256 KiB stderr flood drains; a hanging handler does not prevent echo | Confirmed for exercised loads |
| Background socket task ends on unload | Actual loopback TCP heartbeats observed, followed by EOF/reset after graceful unload; leader and descendant sockets also close on forced cleanup | Confirmed |
| Crash and old process cleanup | Exit 71 is observed even when a descendant retains stdout; leader and descendant `/proc` entries disappear, adopted descendant is reaped; stubborn leader requires force and is reaped | Confirmed |
| Handles fenced | Old handle fails before and after reload; completed lease replay and forged scope/session/generation/lease callbacks are denied without accepted effects; unit test verifies lease expiration while still stored | Confirmed |
| Bounded resource trend over repeated cycles | 60 measured cycles after 10 warmup generations; post-cleanup RSS spread 16 KiB, FD baseline/max 10; OS child lists empty after cleanup | Confirmed within finite experiment |
| Reproducible harness and exact version/artifact record | Lockfile, check script, recorded compiler/kernel/hardware, source SHA-256 map and binary digests; retained raw samples | Confirmed |

Additional failure checks pass for incompatible protocol, rejected initialization, handshake timeout and cancellation of a load future before it returns a handle. Unit coverage checks malformed/oversized frames, bounded admission, reserved control capacity, cancellation/deadlines, dropped callers, peer-owner cleanup and PID retention until group signaling. Pre-admission cancellation and zero-deadline regressions were reproduced before correction; the final tests require that neither dispatches remote work.

## Commands and results

`./scripts/check-p1.sh` exited 0: formatting passed, workspace/all-target Clippy passed with warnings denied, all 13 unit tests passed, locked release build passed, 60-cycle process experiment passed.

A separate `cargo +1.95.0 build --locked --release -p oracle-fixture-beta --target-dir target/msrv` exited 0. The current release host and alpha (Rust 1.98.1) then ran the complete harness with that beta (Rust 1.95.0) for 30 measured cycles, exiting 0. This demonstrates the tested wire contract across independently compiled Rust executables; it does not establish compatibility with arbitrary future wire versions.

| Measurement after cleanup | Release, 60 cycles | Mixed compiler, 30 cycles |
| --- | ---: | ---: |
| Baseline host RSS, KiB | 15,808 | 15,828 |
| Minimum–maximum RSS, KiB | 15,808–15,824 | 15,828–15,840 |
| Median RSS, KiB | 15,820 | 15,836 |
| Linear RSS slope, KiB/cycle | 0.2514 | 0.3987 |
| Baseline / maximum host FDs | 10 / 10 | 10 / 10 |

Both runs had graceful exit 0, crash exit 71 with one descendant reaped, forced termination with one descendant reaped, and no cleanup error. Both used five warmup rounds loading both fixtures (10 generations) before measurements.

## Version and artifact record

The retained [machine-readable evidence](evidence/p1-2026-09-08.json) includes full raw reports, fixture/host SHA-256 hashes, the exact source hash map and compiler metadata. Source hashes were checked against the final implementation before saving this evidence. The base Git commit is `329eddf0d8996b0969aece4cf28bcdbec9a51296`; the implementation is uncommitted, so that commit alone does not identify these tested sources.

- Host/alpha/beta release compiler: Rust 1.98.1, commit `48a229ceaefd4985c50990b14116b6d856af0985`; Cargo 1.98.1.
- Mixed-run beta compiler: Rust 1.95.0, commit `59807616e1fa2540724bfbac14d7976d7e4a3860`.
- Target: `x86_64-unknown-linux-gnu`; Linux `7.0.0-31-generic`; AMD Ryzen 7 5800X3D, 16 logical CPUs, 32,779,104 KiB total memory.
- Cargo.lock pins dependencies; key versions include Tokio 1.53.1, tokio-util 0.7.19, nix 0.31.3 and serde 1.0.229.

## Contract learned and remaining scope

Keep a continuously progressing RPC reader and reserve capacity for responses/cancellation. Supervise OS exit independently of pipe EOF. Preserve the process-group leader's PID until signaling finishes, then reap the leader and adopted descendants. Stop admission before draining, and fence authority independently of whether a caller has noticed cancellation. Cleanup success must mean OS resources have actually been reclaimed.

The bounds were declared before measurements: RSS spread ≤64 MiB, positive fitted slope ≤1 MiB/cycle and FDs ≤baseline+2. The measured changes are much smaller, but this is a finite regression screen, not proof of unlimited-duration leak freedom or a production benchmark. Socket activity spans lifecycle operations, not an endurance soak.

The supported trust model requires descendants to remain in their process group. Process-wide subreaper ownership, non-escaping descendants and a live Tokio runtime are integration assumptions. Host checks here fence synthetic callbacks, not external Discord side effects; P2 must still verify queue/rate-limiter fencing. No P2–P7 result is implied. Reproduction instructions and API boundaries are in [README.md](README.md).
