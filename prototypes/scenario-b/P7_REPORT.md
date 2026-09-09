# P7 acceptance report — 2026-09-08

**Confirmed for the Stage 0 subprocess/configuration scenario with deterministic delivery and subscription collaborators.** This is not a live Discord logging qualification or a finished feature module. All five scenario tests, Clippy with warnings denied and formatting checks pass; the replayable evidence and exact artifact hashes are in [local-report.json](local-report.json).

| Requirement | Fresh executable evidence |
|---|---|
| Separate logging executable, dynamic load during a run | Host begins with zero modules; test copies the executable after startup, loads it through P1, then discovers its published tools and preset. Host PID stays unchanged. |
| Exact `moderate/v1` values | Independent test asserts all enabled and excluded event classes, 15-minute membership summaries, 14-day database metadata retention, content/attachment exclusions, staff destination, coalescing, queue limit, dropped-event reporting and self-origin exclusion. |
| Configuration CAS and preserving unrelated settings | SQLite update predicates on scope and expected revision; stale plans cannot overwrite a concurrent operator note, which survives replan. Desired config and pending receipt commit in one transaction. |
| Desired versus observed effective state | Separate module activation and health RPCs; complete requires exact effective config/revision, healthy fixture subscriptions, and delivery readback. Desired revision alone never succeeds. |
| Crash before acknowledgement | Module activates in memory and exits 73 before replying. Host persists `activation_unknown`, retains desired revision, sends no test message, waits for process reap, reopens the same SQLite file and recovers through a newly loaded executable. No extra configuration revision is created. |
| Old generation fencing | Old process handles fail; old plans fail after reload. Plans also bind a unique host epoch, so a host restart cannot accidentally accept a coincidentally equal process generation. |
| Destination and privacy | Public-reader, foreign-guild and actor-hidden destinations fail before config write. Module requires all declared intents/permissions, including `guild_members`, at both prepare and apply. Unknown presets are rejected. |
| Delivery failure remains partial | Failed sends, absent readback and unhealthy subscriptions cannot yield complete receipts. Retry uses a stable delivery identity and preserves one message in the same delivery fixture. |
| Framework telemetry separation | Host tracing configuration remains unchanged during logging setup. Module event semantics live in the separate executable. |

## Exact scope

`activity-log` is ordinary Rust code started over stdin/stdout RPC using the existing trusted Linux P1 runtime. SQLite is a real bundled database with WAL and `synchronous=FULL`; no mock database replaces CAS or crash persistence. Temporary databases and installation paths are isolated per test. The dependency lockfile and compiled executable digest are retained in the evidence record, together with the three P1 source-file digests used by this build.

The delivery adapter and subscription-health observations are deterministic fixtures. No actual Discord event, privileged-intent grant, role hierarchy, network delivery or real moderation action is exercised. The queue, coalescing, exclusion and retention settings are validated configuration metadata; their full event-processing and retention-job implementations belong to the later feature module. A synthetic successful delivery therefore does not imply that a real moderation event was observed.

Delivery deduplication in this fixture lasts for the lifetime of its adapter. The tested crash-before-ack path occurs before any delivery; this report does not claim to solve a host crash after a real Discord send with an unknown remote outcome. The separate scenario/effect-ledger work must cover that boundary. PostgreSQL portability is P4, while live Discord adapter qualification is P3.

Reproduce with `python3 prototypes/scenario-b/verify.py`. No credentials, paid provider calls or Discord writes are needed.
