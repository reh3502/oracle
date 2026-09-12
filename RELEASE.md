# Release qualification

A passing offline suite qualifies only the behavior it exercised. A first release also requires current live Discord checks and repeated Gemini evaluations against the candidate executable. Historical prototype results, a supported profile name, or a successful synthetic logging probe do not establish current end-to-end qualification.

Run the reproducible local gates from a clean checkout:

```sh
python3 scripts/prepare-serenity.py
python3 scripts/check.py --full --postgres-bin /usr/lib/postgresql/18/bin
python3 scripts/check-stage5.py --postgres-bin /usr/lib/postgresql/18/bin --soak-cycles 300
```

The Stage 5 runner writes its report under ignored `target/stage5-checks/run-*/report.json`. A successful offline run can exit zero while `release_passed` is false. For a release gate, provide `--gemini-evidence JSONL --quality-a JSON --quality-b JSON --discord-evidence JSON --require-release`; missing or invalid required live evidence must fail that gate. The runner does not automatically make paid requests. Use `--help` for workload and evidence options. Retain reports locally with the exact Git revision, dirty-tree state, executable/package digests, dependency lockfile, OS/CPU/memory, toolchain, backend versions, workload, elapsed time and resource observations. A short soak is a regression check, not a production capacity promise. Qualify the final candidate again after material changes.

## Compatibility and qualification matrix

This is the supported candidate surface and the evidence required to mark it tested. A release's local qualification report supplies actual versions and pass/fail results; this table alone makes no claim of a completed live run.

| Surface | Candidate contract | Required evidence |
| --- | --- | --- |
| Host OS and native target | One active Linux host; example packages target `x86_64-unknown-linux-gnu` | Host build, actual module launch/unload, process soak and shutdown on recorded hardware |
| Rust | MSRV 1.95; edition 2024 | Locked workspace build/tests on Rust 1.95 and the recorded development toolchain; formatting and Clippy |
| Python | 3.11 or newer | Checker unit tests and acceptance runners on recorded Python version |
| SQLite | Version resolved by the locked SQL dependency and runtime | Storage contracts, running-host restart, native backup and isolated restore |
| PostgreSQL | Qualification runner accepts an explicit installation; PostgreSQL 18 is the documented baseline | Same contracts and drills using disposable databases; record server and `pg_dump`/`pg_restore` versions |
| Serenity | Exact revision in `Cargo.toml` and `scripts/prepare-serenity.py`, plus recorded patch | Fork verification, adapter fixtures and fresh disposable-guild checks |
| Module wire/host API | Manifest 1, protocol 1.0, host API 1.0.0 | SDK transport, package rejection, migration, scoped authority and lifecycle contracts |
| Gemini beta | Explicit `v1beta` / `gemini-3.8-flash` profile | Repeated live scenarios, bounded spend, verified host receipts and failure cases |
| Gemini stable profile | Explicit `v1` / `gemini-3.7-flash` profile | Separate provider conformance and repeated live scenarios; beta results do not qualify this pair |

Gemini profile identifiers are accepted configuration choices, not a guarantee of current provider availability. There is no implicit model fallback. Record the exact model/API, prompt and price revisions and usage for each evaluation. Missing, stale or unavailable evidence remains unqualified. macOS, Windows, multi-host operation, arbitrary native targets and other model/backend versions are not implied by this matrix.

## Bounded live Discord check

Use a disposable guild and a private file containing literal `DISCORD_TOKEN=...` and `GUILD_ID=...` assignments. The file is parsed as data, never sourced by a shell. Run this separately when you intend to create and clean up live fixture resources:

```sh
python3 scripts/check-discord-live.py --execute --env-file /private/path
```

The runner builds the canary and writes `target/stage5-live/run-*/report.json`. Supply that report to the release runner's `--discord-evidence` option. The canary checks Gateway readiness/shutdown, nonce-named command creation/readback/deletion, and shared structure planning, application, receipt readback and repeated no-op behavior. It verifies cleanup of its owned resources and preservation of the surrounding inventory. It does not call Gemini or load an existing deployment.

This proves the listed live adapter paths. It does **not** exercise a human slash-command invocation, real moderation-event logging coverage, or Gemini against live Discord. Those remain separately described evidence scopes; do not relabel this canary as those checks. A failed or interrupted run requires cleanup inspection before another attempt; see [operations](OPERATIONS.md#live-canary-failures).

The canary uses a paced workload and records its inspection/mutation interval. This is not a burst-throughput qualification. Discord route limits vary: a measured channel-inventory route allowed ten reads per minute, while the production adapter bounds each read at fifteen seconds. A longer rate-limit wait can therefore produce an unavailable or partial outcome. Preserve its receipts and reconcile before retrying. Keep failed burst runs and any separate owned-resource recovery report alongside the paced qualification evidence.

## Release decision

Require passing deterministic authority/adversarial checks with **zero unauthorized effects**, both database drills, executable unload/reaping, joined shutdown, durable restart recovery, and reproducible module staging/reload/upgrade. Verify the framework starts with zero feature modules installed and AI disabled without a provider key. Review measured resource limits for the recorded workload rather than inventing universal throughput or memory limits.

The deterministic suites verify command reconciliation and stable identities, queued-send authority fencing, event routing and logging behavior under controlled inputs. The bounded live Discord canary separately verifies Gateway readiness/shutdown, nonce-command lifecycle and shared structure effects with receipt readback, repeat handling and owned cleanup in a disposable guild. Its report qualifies only those live paths. Human slash-command invocation and real moderation-event logging are broader deployment-specific coverage and remain unqualified unless separately exercised; a synthetic notification establishes delivery only.

Run repeated Gemini structure and logging scenarios against the simulator, including repeated requests, module removal/replacement, missing modules, permission changes, cancellation, budget exhaustion and ambiguous external outcomes. Judge outcomes using receipts and fresh state; model prose is not evidence. Keep live-model simulator results distinct from the bounded Discord canary and from any additional deployment checks.

Resolve every outstanding process, drain, Discord, storage, Gemini and scenario qualification blocker before declaring the first release qualified. Preserve failed runs as well as successes; do not average away a safety failure. Live evaluations require deliberately supplied test credentials, a disposable guild and an explicit spending limit. Routine developer checks must remain offline and credential-free.

Keep evidence, stage records and generated artifacts in ignored local directories. Publish only a reviewed, redacted summary if needed. Follow [operations and diagnostics](OPERATIONS.md) for recovery and [SDK compatibility](crates/oracle-module-sdk/README.md#compatibility-policy) for module release constraints.
