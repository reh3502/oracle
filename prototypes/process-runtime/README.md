# Stage 0 P1: native process runtime

P1 is an executable Linux experiment for Oracle's proposed module boundary. Two ordinary Rust executables can be introduced after the host starts, loaded, invoked and unloaded without restarting the host. The harness exercises real pipes, TCP sockets, process crashes and descendant reaping.

## Reproduce

Requirements: Linux with `/proc`, Rust 1.95 or later, Cargo, rustfmt, Clippy, Python 3 and Git. The initial build needs access to the crates in Cargo.lock. No Discord, database or AI credentials are required.

From the repository root:

```sh
./scripts/check-p1.sh
```

The script checks formatting, denies Clippy warnings, runs the unit tests, builds the workspace in release mode with the lockfile, and runs 60 measured load/invoke/unload cycles after 10 warmup generations. Pass an integer of at least 30 to change the measured cycle count, for example `./scripts/check-p1.sh 120`. Failure returns a nonzero exit status.

Generated `artifacts/environment.json` records compiler, Cargo, kernel, hardware and source hashes. `artifacts/p1-release.json` records binary SHA-256 digests, pass/fail, cleanup reports, resource samples and acceptance checks. These outputs and `target/` are ignored by Git.

To exercise independently compiled executables with different Rust toolchains (install 1.95.0 first if needed):

```sh
cargo +1.95.0 build --locked --release -p oracle-fixture-beta --target-dir target/msrv
target/release/p1 --alpha target/release/oracle-fixture-alpha \
  --beta target/msrv/release/oracle-fixture-beta --cycles 30 \
  --output prototypes/process-runtime/artifacts/p1-msrv.json
```

## Components and contracts

- [src/rpc.rs](src/rpc.rs): major-version-1 JSON frames with a four-byte big-endian length prefix; requests, responses and cancellation use request IDs. A continuously running reader dispatches bounded handlers independently of the writer, allowing host → module → host → module calls. Frames are limited to 1 MiB and serialized bodies to 512 KiB. Each direction admits at most 64 pending calls; a reserved control queue keeps responses and cancellation available under request saturation.
- [src/runtime.rs](src/runtime.rs): `ProcessRuntime::load` validates identity, wire major and initialization before returning a `ModuleHandle`. Each load receives a new generation and session. `invoke` issues a host-owned, scoped, expiring lease; callbacks are checked before and after nested work. Completed leases and unloaded handles cannot regain authority after reload.
- [../fixtures/common.rs](../fixtures/common.rs): alpha and beta implement handshake, nested/event/job callbacks, a tracked TCP heartbeat task, stderr flooding, cancellation and deliberate failure modes. Beta also launches a native descendant retaining stdout and a socket.
- [src/harness.rs](src/harness.rs): creates an empty runtime, copies binaries into a fresh directory after startup, then checks results through the runtime's public API and Linux process/socket observations. It samples host and live-child RSS/FD counts and verifies the host's OS child lists are empty after cleanup.

`ModuleHandle::unload` closes admission, allows existing leases a bounded drain period, fences the generation, requests bounded cooperative shutdown and waits for cleanup. The supervisor separately observes OS exit, so a descendant retaining stdout cannot conceal a leader crash. It observes exit with `WNOWAIT`, retaining the leader PID until the final process-group signal, then reaps the leader and adopted descendants. A completion timeout means cleanup is still unconfirmed; it never means successful unload.

## Scope and limits

This is a trusted-process prototype on one Linux host. `ProcessRuntime::new` enables process-wide child-subreaper behavior. Its integration must own child supervision and must not let another waiter reap its leaders. Descendants must remain in the module's process group; escaping via `setsid` or passing descriptors outside that group is outside this trust model. The subprocess boundary does not sandbox filesystem, network or credentials.

The prototype has no stable public SDK, Discord client, installer, persistence or AI agent. P2 still needs to prove fencing at actual queued Discord effects. Event/job methods here exercise callback transport, not a production router or scheduler. Artifact digests identify the tested bytes; hashing a path before execution is not an atomic or authenticated installation mechanism.

Resource thresholds are screening bounds for this finite workload: RSS spread ≤64 MiB, fitted RSS slope ≤1 MiB/cycle and post-cleanup FD count ≤baseline+2. Raw measurements are retained so these deliberately generous limits are visible. A pass does not establish an infinite-duration leak bound or production capacity. TCP tests establish continuing background activity and closure during lifecycle operations; they are not long-duration network endurance tests.

Call `RpcPeer::close` from the owning supervisor, not from one of that peer's inbound handlers: close joins the handler tasks. Keep the Tokio runtime alive while asynchronous process cleanup completes.
