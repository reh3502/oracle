# Oracle agent foundations

Stage 4 is in progress. This crate provides the Gemini boundary, bounded tool discovery, admission accounting, and durable semantic run/call records. The coordinator and host CLI/Discord agent controls are not yet integrated.

## Provider boundary

`ModelProvider::prepare` performs no I/O. It builds bounded provider-native bytes and returns a conservative token reservation. The host must persist request and daily-spend reservations before calling `send`. `send` performs one potentially billable request; retry policy belongs to the host.

`GeminiProvider` supports explicitly selected `v1` / `gemini-3.7-flash` and `v1beta` / `gemini-3.8-flash` profiles. It has no endpoint fallback. The earlier P5 live qualification covered beta/3.8; the production adapter currently has offline and loopback contract evidence. A new production live qualification remains a separate bounded, authorized check.

Requests use `store=false`, `stream=false`, a configured output limit, and low thinking by default. `ThinkingLevel` permits an explicit override. Profile limits and operator-supplied prices are configuration, not hardcoded claims about model capacity or billing. Keys, prepared bodies, native continuation and provider metadata have no `Debug` representation. Redirects, environment proxies and transport retries are disabled.

Native steps remain raw JSON, preserving signatures, unknown fields and numeric representation. Complete function-result batches retain the original call order. The active tool shortlist can change after a batch; historical definitions remain private and are not resent as active schemas. Reusing an old alias with changed meaning requires a fresh session from reconciled semantic facts. Profile/policy changes also require a fresh session.

The narrow portable schema dialect rejects unsupported keywords. The host still validates canonical operation arguments and current authority before admitting effects. Provider schema validation is not authorization.

These choices follow the [Gemini Interactions contract](https://ai.google.dev/gemini-api/docs/interactions-overview) and the repository's [P5 prototype](../../prototypes/gemini-contract/README.md). Provider-native continuation is execution state, not an operational log.

## Usage and durable state

- `Catalog` selects at most 12 tools and 32 KiB of schemas from a host-authorized snapshot. Discovery and receipt tools remain pinned. Revision and descriptor/binding hashes reject stale selections.
- `Budget` reserves tokens and estimated cost before every attempt, retains charges when usage is unknown, reserves verification capacity, and bounds requests, tools and unchanged turns. Reported overages remain charged and stop further admission.
- `SpendStore` uses atomic scoped CAS to admit concurrent runs against one guild's UTC-day spending limit. Settlement uses the original day; unknown attempts cannot refund the reservation. A bounded daily record currently admits at most 128 distinct runs per guild/day.
- `RunStore` binds goals to authenticated principals and guilds. Separate call records keep summaries small. Only a newly inserted call admission may dispatch; reused IDs return their original outcome. Interrupted admissions become unknown recovery work.

Native reasoning is not persisted in run records. Restarted runs must reconcile semantic records and outstanding reservations before beginning a new provider session. The coordinator must enforce this ordering and recheck authority, cancellation and deadlines at dispatch; these primitives alone do not execute a goal.

## Verification

```sh
cargo test --locked -p oracle-ai
cargo clippy --locked -p oracle-ai --all-targets -- -D warnings
cargo +1.95.0 check --locked -p oracle-ai --all-targets
```

Tests cover native replay, dynamic shortlists, unknown usage, bounded HTTP, redacted failures, cancellation, scope isolation, stale writers, restart accounting, call deduplication and concurrent spending admission. Provider tests use synthetic fixtures and local TCP servers. Storage's contract suite additionally exercises migration 0004 and agent records on SQLite and PostgreSQL. No ordinary test makes paid Gemini calls.
