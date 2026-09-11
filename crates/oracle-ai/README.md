# Oracle agent

This crate provides the Gemini boundary, bounded coordinator, tool discovery, admission accounting, and durable semantic run/call records. The host composes these with shared structure/configuration services and authenticated CLI/Discord controls.

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

Native reasoning is not persisted in run records. Restarted runs must reconcile semantic records and outstanding reservations before beginning a new provider session. The coordinator enforces this ordering and rechecks authority, cancellation and deadlines before dispatch. Only host-verified receipts and fresh state can close a run successfully; model text is labelled unverified when retained as a question.

`ToolHost` supplies an authorized immutable catalog, shared operation execution, and fresh reconciliation. Search updates the active shortlist. Completed tool rounds can compact to a fresh provider session rebuilt from host receipts. At most two retries are allowed for transient/rate-limited provider failures, with each attempt durably charged. Host tool calls are serialized and have one recorded outcome each. Pending or unknown external effects are never blindly replayed.

The host exposes module configuration independently of slash commands. Native module tools require explicit reviewed `ai` metadata; inspection cannot claim notification capabilities, and verification requires a host effect receipt. This remains under the trusted-native-module model. Models cannot install packages, grant capabilities, approve plans or obtain general SQL/HTTP tools.

## Verification

```sh
cargo test --locked -p oracle-ai
cargo clippy --locked -p oracle-ai --all-targets -- -D warnings
cargo +1.95.0 check --locked -p oracle-ai --all-targets
```

Tests cover native replay, dynamic shortlists, unknown usage, bounded HTTP, redacted failures, cancellation, scope isolation, stale writers, restart accounting, call deduplication and concurrent spending admission. Provider tests use synthetic fixtures and local TCP servers. Storage's contract suite additionally exercises migration 0004 and agent records on SQLite and PostgreSQL. No ordinary test makes paid Gemini calls.

## Module tool projections

Operations are hidden from the model unless their installed manifest explicitly opts in:

```json
"ai": { "kind": "inspection", "success_pointer": null }
```

`verification` also requires `discord.notify` and a JSON pointer to a boolean result postcondition. Inspection postconditions must accept an empty input object for fresh host readback. Inspection cannot declare `discord.notify`; neither projection can declare `contracts.invoke` or `host.echo`. The host still checks current capability grants, actor/guild scope, session, generation, activation epoch and canonical schemas. These descriptors describe reviewed native code; they do not sandbox module code or grant new authority.

A verification result must include `receipt.host_effect_id` from the host notification API. The host checks the real effect state, purpose, destination and delivery receipt, then refreshes configuration, subscriptions and destination policy. A module's boolean alone cannot prove delivery. The logging example declares separate inspection and verification projections; it remains an optional installed artifact.
