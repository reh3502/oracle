# P5 Gemini Interactions contract prototype

The local contract checks and **authorized beta/3.8 live gate pass**. The three-request run completed two sequential tool rounds with exact native continuation/signature replay; see [live-success.json](live-success.json). Earlier HTTP 429 failures remain historical diagnostics. Stable v1/3.7 is a separate unqualified live profile. This is a standalone prototype workspace.

```sh
cargo test --locked --manifest-path prototypes/gemini-contract/Cargo.toml
cargo clippy --locked --manifest-path prototypes/gemini-contract/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path prototypes/gemini-contract/Cargo.toml --check
```

Thirteen checks cover two sequential tool rounds; exact native JSON step replay, including signature-only thoughts, unknown fields and numeric lexical representation; complete result batches and matching call IDs; duplicate/unknown calls; malformed arguments and descriptor schemas; terminal/truncated responses; prose refusal; unknown usage; the downloaded OpenAPI seams; and live authorization guards. The five response fixtures are **synthetic**, not captured Gemini outputs.

`Session` holds raw provider steps and normalized calls separately. Every request resends history, tools and trusted policy with `store=false`, `stream=false`, and no `previous_interaction_id`. Entire call batches validate before admission. Result batches match the original call order. Supported schema keywords are deliberately limited to `type`, `description`, `properties`, `required`, `additionalProperties:false`, `items` and `enum`; unsupported constraints fail closed. Integers are limited to signed/unsigned 64-bit values. This is not a complete JSON Schema implementation or a general Gemini provider.

## Authorized live probe

Once an operator explicitly configures `GEMINI_API_KEY`, run:

```sh
ORACLE_GEMINI_PROFILE=beta-3.8 ORACLE_GEMINI_LIVE=I_AUTHORIZE_3_REQUESTS cargo run --locked --manifest-path prototypes/gemini-contract/Cargo.toml --bin live
```

This opts into at most **three potentially billed requests**, each capped at 1,024 output tokens with a 90-second HTTP timeout. The explicitly selected `beta-3.8` profile uses Google's `/v1beta/interactions` with `gemini-3.8-flash`, following the user's supplied endpoint/model. The original `stable-v1-3.7` profile remains separately selectable and is the default when the profile variable is absent. A successful beta run is not evidence for stable-v1 conformance. There is no automatic beta fallback, redirect, retry, Discord operation or arbitrary tool execution. The read-only local function passes a newly generated receipt from round one into round two; a final third turn must complete. At least one native signature must be observed and replayed exactly. It writes only digest/model/usage/status metadata to this directory's ignored `live-report.json`; no raw signatures, credentials or response prose are persisted. Token caps do not constitute a monetary spending guarantee.

See [P5_REPORT.md](P5_REPORT.md) and [local-report.json](local-report.json) for current evidence and qualification limits.

Non-2xx responses append restricted diagnostics to `live-failures.jsonl`: status, response digest, allowlisted quota identifiers/numeric values, retry delay, and fixed message-category labels. Raw message prose, quota dimensions and credentials are omitted. Both success and failure metadata bind the probe to compile-time source/schema hashes and include a timestamp.

Latest qualification: the authorized beta/3.8 retry passed all three requests, both dependent tool rounds and exact signature replay. Earlier rejected requests are retained as historical diagnostics.
