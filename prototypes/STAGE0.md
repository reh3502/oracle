# Stage 0 qualification

Stage 0's local prototype suite passes. **All seven required gates are confirmed within their stated prototype scopes.** P5's authorized beta/3.8 retry completed all three requests, both tool rounds and exact signature replay. Earlier rejected requests remain historical diagnostics. P8 is an optional comparison and is not required for the selected trusted-process architecture.

| Gate | Status | Evidence |
| --- | --- | --- |
| P1 Process runtime | Confirmed | [Report](process-runtime/P1_REPORT.md): hot load/unload, nested RPC, sockets, crash/reaping, fenced handles, 60 measured release cycles and mixed compiler experiment |
| P2 Drain boundary | Confirmed for host-controlled transport seam | [Report](drain-boundary/P2_REPORT.md): actual loopback sends, rate wait/retry fencing, durable uncertainty, duplicate ownership, physical process replacement |
| P3 Serenity baseline | Confirmed, including authorized live guild operations | [Evidence](serenity-baseline/EVIDENCE.md): pinned upstream builds, narrow fork patch, six dispatch/HTTP/command tests and live create/readback/cleanup |
| P4 Storage fit | Confirmed on both actual backends | [Report](storage-fit/P4_REPORT.md): scope/CAS/index/order parity, concurrent clients, bounded migrations and index-backed query plans |
| P5 Gemini contract | Confirmed for explicit beta/3.8 adapter | [Report](gemini-contract/P5_REPORT.md): 13 tests, lossless two-round replay, explicit v1/3.7 and user-selected v1beta/3.8 profiles; live beta/3.8 two-tool-round test passed |
| P6 Minecraft scenario | Confirmed for deterministic operation fixtures | [Report](scenario-a/P6_REPORT.md): fresh/pre-existing/hidden/hierarchy/unknown-outcome conditions, repeat no-op and whole-plan readback |
| P7 Logging scenario | Confirmed with actual logging subprocess | [Report](scenario-b/P7_REPORT.md): dynamic discovery, exact preset, durable CAS, crash-before-ack recovery, stale-generation and delivery-failure handling |

## Reproduce

On the recorded Linux environment with Rust/rustup, Python 3 and the PostgreSQL bootstrap requirements described in P4:

```sh
./scripts/check-stage0.py --offline-only
```

This runs every local gate with locked dependencies, including the actual SQLite/PostgreSQL and process experiments. It does not issue live Discord or Gemini requests. Generated logs and `stage0.json` are under `prototypes/artifacts/` and ignored by Git. Without `--offline-only`, exit status also requires current separately recorded live P3/P5 evidence; it still does not initiate remote writes or paid requests. The command returns 2 if any required local gate or current live evidence is missing; all required evidence is now present.

Each standalone workspace has its own lockfile and can be exercised separately from its README. P1 remains the root workspace. P2 and P7 depend on the same tested P1 runtime by path. The P4 script extracts exact native PostgreSQL packages into its ignored local directory, uses a fresh private Unix-socket cluster, and stops it on exit; no global database service is installed.

## Retained records

- [P1 original qualification](process-runtime/evidence/p1-2026-09-08.json)
- [P2](evidence/p2-2026-09-08.json), [P3](evidence/p3-2026-09-08.json), [P4](evidence/p4-2026-09-08.json), [P6](evidence/p6-2026-09-08.json): source/binary/schema hashes and raw acceptance results. Live guild/resource IDs remain in ignored local P3 evidence.
- [P5 local contract](gemini-contract/local-report.json) and [P7 executable scenarios](scenario-b/local-report.json)
- [Combined local run](evidence/stage0-local-2026-09-08.json)

The host toolchain is Rust 1.98.1; P3 builds pinned Serenity on its declared Rust 1.95.0. SQLite 3.51.3 and native PostgreSQL 18.6 were exercised through SQLx 0.9.0. Exact build/source identities and environment details are in the records, not inferred from package names alone.

## Decisions established

Keep the trusted Linux process boundary, bounded bidirectional RPC, host-issued expiring leases and explicit process-group supervision. Effect transport must revalidate after every rate wait and serialize the actual dispatch with fencing. Durable effect uncertainty outlives the module, and concurrent owners must not overwrite it.

The exact Serenity `next` baseline needs a narrow patch: preserve unknown Gateway dispatch for the adapter's partial-update fallback, and make interaction decoding insensitive to JSON key order. Keep the fork locally editable and pinned by upstream SHA plus patch digest. The live guild qualification created a category/text/voice and guild command, then removed all owned resources and independently verified the original channel/command counts.

The scoped storage contract fits the three representative workloads on both backends without exposing arbitrary SQL. Scenario success must be based on fresh whole-state readback, not accumulated successful calls. Logging's stored revision and independently observed effective revision remain separate.

The Interactions rendered reference and v1 OpenAPI disagree about routes. The user-selected beta/3.8 combination is an explicit provisional profile, not a claim of stable v1 conformance. The real authorized beta/3.8 run succeeded with two tool rounds and exact native continuation/signature preservation. Stable v1/3.7 remains a separate unqualified live profile; this does not block the explicitly selected beta adapter.

## Scope

These are focused prototypes, not the production bot or stable SDK. P2's HTTP fixture does not qualify a complete TLS implementation or stock Serenity rate limiter. P6's normalized approved profiles are not a general Discord permission engine. P7's delivery and subscriptions are deterministic collaborators; its queue/retention configuration is not a finished logging data plane. P4 does not claim production backup/restore or activation/migration coordination. Those distinctions preserve the roadmap's later stages; no Stage 1–5 completion is implied.
