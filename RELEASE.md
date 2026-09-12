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

## Release decision

Require passing deterministic authority/adversarial checks with **zero unauthorized effects**, both database drills, executable unload/reaping, joined shutdown, durable restart recovery, and reproducible module staging/reload/upgrade. Verify the framework starts with zero feature modules installed and AI disabled without a provider key. Review measured resource limits for the recorded workload rather than inventing universal throughput or memory limits.

In a disposable Discord guild, verify command publication and stable command IDs, actual effects and permissions, fencing during queued sends, and logging coverage from real events. Run repeated Gemini structure and logging scenarios, including repeated requests, module removal/replacement, missing modules, permission changes, cancellation, budget exhaustion and ambiguous external outcomes. Judge outcomes using receipts and fresh state; model prose is not evidence. A synthetic notification establishes delivery only, not real moderation-event coverage.

Resolve every outstanding process, drain, Discord, storage, Gemini and scenario qualification blocker before declaring the first release qualified. Preserve failed runs as well as successes; do not average away a safety failure. Live evaluations require deliberately supplied test credentials, a disposable guild and an explicit spending limit. Routine developer checks must remain offline and credential-free.

Keep evidence, stage records and generated artifacts in ignored local directories. Publish only a reviewed, redacted summary if needed. Follow [operations and diagnostics](OPERATIONS.md) for recovery and [SDK compatibility](crates/oracle-module-sdk/README.md#compatibility-policy) for module release constraints.
