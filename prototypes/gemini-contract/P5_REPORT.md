# P5 acceptance report — 2026-09-08

**Status: confirmed for the explicitly selected beta/3.8 adapter.** Thirteen offline tests pass. Following the operator's credit update and authorized retry, `/v1beta/interactions` with `gemini-3.8-flash` completed all three requests, two sequential tool rounds and exact signature/native-step replay. [The successful live record](live-success.json) contains current source/schema hashes, model identity and usage metadata. Earlier HTTP 429 failures remain historical evidence; they do not describe the current successful run. Stable v1/3.7 is a separate profile and remains unqualified live.

| Requirement | Evidence | State |
|---|---|---|
| v1 OpenAPI versus rendered route | Downloaded unmodified schema; v1 rendered reference inspected | Confirmed inconsistency |
| Stable model choice | Official catalog identifies 3.8 Flash as stable; live response confirms the user-selected model on beta Interactions | Confirmed for selected profile |
| Two sequential tool rounds | `two_rounds_preserve_native_steps_bytes_and_signatures` | Confirmed locally and live on beta/3.8 |
| Stateless replay and signatures | Raw JSON step bytes survive requests two and three, including a thought without summary | Confirmed locally and live on beta/3.8 |
| Malformed schema and arguments | Unsupported constraints, missing fields, wrong types, unknown tool, duplicate IDs denied | Local confirmed |
| Refusal and truncation | Prose refusal emits no call; incomplete/failed/cancelled statuses release no calls; malformed wire fails | Local confirmed; provider-specific refusal discriminator unavailable |
| Artifact and schema record | Checked-in schema, standalone Cargo.lock, synthetic fixtures and `local-report.json` hashes | Confirmed |
| Real authorized endpoint/API/model request | User-selected beta/3.8 profile completed three requests and two tool rounds with matching model identity and exact replay | **Confirmed for beta/3.8** |

## Primary-source findings

The [v1 OpenAPI](https://ai.google.dev/static/api/interactions-v1.openapi.json), retrieved on 2026-09-08, has `info.version=v1`, revision `0`, a global Google server and parameterized `/{api_version}/interactions`. Its embedded shell examples use `/v1/interactions`. The [rendered v1 reference](https://ai.google.dev/api/interactions-api-v1) instead labels its top POST endpoint `/v1beta/interactions`. This experiment explicitly chooses stable v1 and will fail without silently switching versions. Source hash: `3c25941e544ff0d96125faf65983b36152f91e0e6e2c4dc88b051fd687ca3f52` (183,756 bytes).

The [model catalog](https://ai.google.dev/gemini-api/docs/models) labels `gemini-3.7-flash` stable and also lists a newer stable 3.8 model. The probe retains the design's 3.7 candidate as a separate profile; the qualified user-selected profile uses 3.8. The schema enumerates names with `models/`; its examples and catalog use short IDs. The successful beta/3.8 run confirms the short model ID for that profile; the v1/3.7 combination remains unqualified.

Google's [stateless function calling guide](https://ai.google.dev/gemini-api/docs/function-calling#stateless-function-calling) requires replaying all generated steps with matching function results. The implementation retains raw steps rather than reconstructing thought blocks or signatures. It never uses thought text to establish completion.

The downloaded schema has unresolved local references to `SafetySetting`, `SpeakerConfig` and `SpeechConfig`. Consequently this report claims the exercised text/function subset, not complete OpenAPI validation. The schema exposes `incomplete` status but no dedicated refusal step or status. A refusal expressed as ordinary completed text is preserved as provider completion with zero callable work; the host must retain its independent goal-verification gate. No undocumented safety error code or English text heuristic is invented.

## Verification and limits

Executed with rustc 1.98.1 (`48a229cea`, 2026-09-01), cargo 1.98.1 (`797e8a9bc`, 2026-08-05); the package declares minimum Rust 1.95. Exact crate versions are in `Cargo.lock`. `cargo test --locked` passed thirteen integration tests; Clippy with warnings denied and formatting checks passed. The live binary's absent-authorization and absent-key paths both exit 1 before networking.

This probe is non-streaming and does not implement production retries, rate-limit scheduling, persistence, context compaction, multi-provider switching or a full JSON Schema dialect. HTTP wire behavior, account permissions, actual model signatures and real provider refusal/truncation responses are not proved by synthetic fixtures. The authorized live run now qualifies the explicit beta/3.8 profile. This is a documented provisional beta adapter decision, not proof of stable v1 wire conformance.

## Explicit user-requested beta profile

The user supplied `/v1beta/interactions` with `gemini-3.8-flash`. This is implemented as the explicit `beta-3.8` profile, with the original `stable-v1-3.7` profile preserved separately. No automatic fallback occurs. The [official beta reference](https://ai.google.dev/api/interactions-api) identifies `/v1beta/`; its linked [OpenAPI snapshot](https://ai.google.dev/static/api/interactions.openapi.json) was downloaded unchanged (379,890 bytes, SHA-256 `c3993507e6928c16dca47817038d32a8ef853c1e1c049f154d101c8b0c9b1d21`). Its model enum does not yet list 3.8, while unknown values are explicitly allowed and the official catalog lists 3.8 as stable. The subsequent successful live contract probe qualifies this specific model/endpoint combination.

Both snapshots agree on the exercised function call/result fields. Beta changes the referenced thought-summary content schema, which remains opaque in this adapter. The added local test runs both profiles through two synthetic tool rounds and exact signature replay. The safe error parser distinguishes structured status/quota/retry metadata from fixed, non-authoritative prose categories; raw provider prose and echoed credentials are never emitted. No quota or billing cause is inferred solely from HTTP 429.

## Latest live evidence

An earlier `beta-3.8` contract attempt failed on its first request with HTTP 429. `live-failures.jsonl` retains the exact request/response digests, selected beta schema digest, timestamp and compile-time source hashes; the sanitized diagnostic is `message_category=billing`. It contains no structured quota limit or retry delay. This identifies billing language in the provider response, without inventing an account-specific quota explanation. The same response digest occurred in the earlier stable and beta attempts. No tool round succeeded in those rejected attempts. A later operator-authorized retry succeeded after the credit update, as recorded above.

A separate authenticated **read-only** GET of `/v1beta/models/gemini-3.8-flash` returned HTTP 200 with matching model identity, as reported by the coordinating agent and preserved with that provenance in `artifacts/model-metadata-observation.json`. Metadata access confirms that the credential can reach that control-plane resource; it does not prove that inference billing or Interactions is available. The subsequent successful three-request test, rather than this metadata observation, qualifies the user-requested beta profile. The profile remains an explicit provisional beta adapter choice.
