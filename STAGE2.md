# Stage 2: dynamic module SDK

Stage 2 is implemented and qualified on September 8, 2026. The final run passed all 27 checks and 102 Rust test executions, with zero failures. The acceptance contract is [Stage 2 of the roadmap](docs/ROADMAP.md). Build and operator commands are in [README.md](README.md); sample authoring, staging, reload and upgrade commands are in [the module guide](examples/modules/README.md).

## Acceptance and evidence

| Requirement | Implementation | Executed evidence |
|---|---|---|
| Install an executable absent at host startup; invoke and unload without restarting | Immutable artifact store, live Unix-socket module commands, process registry | Real-process `lifecycle` test creates packages after manager startup; developer drill keeps host PID unchanged across two reloads and an upgrade |
| No compile-time feature registry or bundled feature module | Separate SDK/client executables; host depends on framework crates only | Workspace builds; host normal dependency tree contains no example/fixture crate; runtime install/load tests |
| Static installation, native trust and compatibility | Explicit trust flag, bounded package layout, SHA-256 verification, native executable checks, offline schemas and protocol/target/API checks | Package unit tests reject tampering, traversal, links, unsupported protocol and invalid schema without running executable code |
| Dependencies and scoped contracts | Same-guild bindings, required dependency graph, inherited capability ceiling | Dependency unit tests, real counter/consumer calls, missing-provider rejection and provider-crash recovery |
| Framed bidirectional RPC and process ownership | Bounded frames/pending calls, cancellation, reserved control capacity, process groups and descendant reaping | RPC/process workspace tests and real lifecycle drills; cancelled unload retains ownership until the old writer is reaped |
| Guild activation independent of process lifetime | Shared process generation, distinct guild epochs and document namespaces | Two-guild lifecycle tests; graceful deactivation preserves the other guild; forced cutoff restores remaining desired guilds with fresh epochs |
| Durable registry publication | SDK preparation followed by durable activation intent, then synchronous admission publication | Three `publication` regressions hold persistence pending: success, failure and cancellation all prevent early invocation/document writes |
| Quiesce, forced termination and effect fencing | Opaque invocation leases, close/drain/fence transitions and host-owned effect tasks | Four loopback-TCP `effects` tests prove zero sends after initial fencing, no second send after retry fencing, durable Unknown after a sent cancellation, and verified delivery |
| Host-derived authority | Session/generation/epoch-scoped handles; storage namespace and capability checks; migration-only clients | Raw `authority` fixture rejects forged, stale, expired and cross-scope callbacks, including ordinary writes/effects from a migration process |
| Task health and recovery | SDK scoped tasks, host health task, bounded restart backoff, dependency recovery and quarantine | Lifecycle crash/restart/quarantine tests, health output and joined shutdown in developer and CLI drills |
| Durable module documents and forward migrations on both backends | Scoped CAS documents, durable page tickets/read sets, old-writer fencing, resumable checkpoints | SQLite and real PostgreSQL storage contracts plus manager lifecycle/upgrade drills; active and inactive namespaces, interrupted checkpoints, retry and downgrade rejection |
| Backup and isolated restore | Schema-2 native backup, deployment identity renewal, paused restore, cleared load intent | Stage 1 cross-backend native restore drills plus module `restore` drill: missing artifact rejection, trusted reinstall and retained document recovery |
| Developer rebuild/stage/reload workflow | Immutable provenance-bearing packages, explicit data-version upgrade command | `check-module-dev.py`: counter value 7 preserved across reloads and v1-to-v2 migration; generation/epoch change; implicit downgrade rejected; zero remaining shutdown tasks |

The publication regressions first failed against the old implementation because an increment reached storage while activation persistence was blocked. All three passed after admission publication moved behind the durable acknowledgement.

## Final verification record

The final run finished at `2026-09-08T23:52:09Z` using Rust 1.98.1 and PostgreSQL 18.6, with an additional whole-workspace Rust 1.95 check. All 27 gates passed, including 102 Rust test executions, fresh native fixture/host builds, real SQLite and PostgreSQL lifecycle and upgrade drills, developer reload/upgrade, full-workspace Clippy with warnings denied, and formatting. Disposable PostgreSQL shutdown also passed.

The root agent independently checked every gate's exit status, recomputed all 74 scoped source hashes after the run, and verified that the retained report exactly matched the published evidence. No scoped source changed during qualification. The immutable local report is `target/stage2-n65xq5in/stage2-local.passed.json`; the portable record is [evidence/stage2-local.json](evidence/stage2-local.json). Root documentation is outside the code snapshot scope. A separate fresh host dependency-tree check confirmed no example or fixture crate in normal runtime dependencies.

## Reproduce qualification

The complete runner builds fresh host and fixture executables, executes the Stage 1 regression suite, runs explicit Stage 2 integration tests, uses disposable PostgreSQL databases, checks the whole workspace on Rust 1.95, and runs Clippy and formatting checks:

```sh
python3 scripts/check-stage2.py --postgres-bin /usr/lib/postgresql/18/bin
```

For this workspace's extracted PostgreSQL distribution:

```sh
python3 scripts/check-stage2.py \
  --postgres-bin prototypes/storage-fit/.local/pg/usr/lib/postgresql/18/bin \
  --postgres-share prototypes/storage-fit/.local/pg/usr/share/postgresql/18 \
  --postgres-lib prototypes/storage-fit/.local/pg/usr/lib/x86_64-linux-gnu
```

The runner records commands, exit codes, test totals, logs, toolchain versions and scoped source hashes in [evidence/stage2-local.json](evidence/stage2-local.json), with an immutable copy under its retained artifact directory. It fails if required checks are missing or source changes during execution. Ignored integration tests require separately built fixture paths and backend setup; ordinary `cargo test` alone is not full qualification.

## Operational boundaries

- Qualified target: one active Linux x86-64 host, SQLite or PostgreSQL. Rust 1.95 is the minimum checked toolchain. Native modules run as the host OS user and are trusted software, not sandboxed code. Protocol capabilities protect host APIs; they do not constrain arbitrary native OS access.
- Installation is static and explicit trust is required before accepting a native package. Loading verifies its immutable digest and launches a separate process. Fixtures are development/test artifacts and are not linked into the production executable.
- Failed or cancelled activation fences the provisional generation. Since guilds share a process, cleanup can temporarily disrupt its other active guilds; desired state supports recovery. Cached status counts can briefly lag a process stop until health/tick refresh; invocation admission is checked independently.
- A forced per-guild cutoff may restart the shared module process. Required consumers are fenced when a provider dies and resume after dependencies recover. Repeated crashes exhaust a bounded restart budget and require operator intervention.
- Upgrades stop and reap the old writer before migration. Incoming artifact intent and committed checkpoints survive failure. Retry the same incoming artifact to resume; ordinary data-version downgrades are rejected.
- Database backups retain module data, inventory and desired activation state, but not executable packages, configuration or secrets. Keep those recovery materials separately. Restores receive a new deployment identity, start paused with load intent cleared, and require the matching trusted artifacts before explicit loading.
- The effect-fencing tests exercise the host-owned transport seam over real loopback TCP. Dynamic Discord/domain operations belong to Stage 3; Gemini execution belongs to Stage 4. This qualification makes no claim of a live Discord module effect, Gemini inference, hardware power-loss certification, or a long-duration release soak.
- All qualification databases and module processes were disposable. The existing live Stage 1 bot was not restarted or migrated for Stage 2 qualification.
