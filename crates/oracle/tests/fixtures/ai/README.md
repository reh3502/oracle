# Stage 4 simulator evaluation

`stage4.json` contains 80 authored cases: 20 Minecraft (A), 20 activity-log (B), 20 injection/authorization (S), and 20 provider/lifecycle/budget (R). Each family has 15 training and 5 held-out cases. Keep held-out cases out of prompt tuning. The fixture hash is part of each report. Changes to initial states, expected predicates, or splits create a new candidate dataset.

The harness is `crates/oracle/src/ai_eval_tests.rs`. It runs the production coordinator, operation services, storage, and native activity-log module against a simulated Discord backend. The live candidate uses real Gemini requests. Fault names describe the injected boundary; simulated provider cancellation is not a claim that real Discord cancellation was exercised. Separate deterministic contract tests cover process restart, receipt recovery, approval binding, and dispatch fencing.

Default checks make **no paid requests**:

```sh
cargo test --locked -p oracle --bin oracle ai::eval_tests
```

These checks validate corpus shape, state graders, and global paid admission. They do not evaluate all 80 cases or qualify a model. Never run all ignored tests indiscriminately: the live test below is deliberately ignored and separately gated.

## Bounded smoke after explicit paid authorization

Build the native fixture and commit the candidate source first. Set `GEMINI_API_KEY` through the host environment; do not place a key in repository files or command history. The example below proposes at most **3 real provider attempts** and **1,000,000 microdollars ($1) of conservative reservations**. It uses A01 once, without repeat requests, and cannot qualify the full model. A passing smoke requires an actual successful provider reply plus a successful tool-result continuation, with no authentication or protocol error. A cap can stop the smoke before task completion.

```sh
cargo build --locked -p oracle-example-activity-log
export ORACLE_ACTIVITY_LOG="$PWD/target/debug/oracle-example-activity-log"
export ORACLE_AI_EVAL_MODE=smoke
export ORACLE_AI_EVAL_APPROVAL=paid-simulator-smoke
export ORACLE_AI_EVAL_ALLOWED_COMMIT="$(git rev-parse HEAD)"
export ORACLE_AI_EVAL_SPLIT=train
export ORACLE_AI_EVAL_PROFILE='{"id":"beta-3.8","model":"gemini-3.8-flash","api_version":"v1beta","max_context_tokens":1000000,"max_output_tokens":8192}'
export ORACLE_AI_EVAL_MAX_REQUESTS=3
export ORACLE_AI_EVAL_MAX_COST_MICROS=1000000
export ORACLE_AI_EVAL_RATE_MICROS_PER_MILLION=20000000
export ORACLE_AI_EVAL_OUTPUT="$PWD/target/stage4-smoke.jsonl"
cargo test --locked -p oracle --bin oracle ai::eval_tests::live_candidate_five_trials_and_repeat -- --ignored --exact --nocapture
```

The example rate is a conservative admission parameter, **not a quoted Gemini price**. Verify the approved provider profile and choose a rate that covers its billed token classes before approving a run. Output is created only at an ignored path and never overwrites a prior report. The source commit must match the approved commit, with no changed tracked candidate files. Setting approval environment variables is a runtime safeguard, not a replacement for explicit human authorization.

## Campaign after separate authorization

Use the same command with `ORACLE_AI_EVAL_MODE=campaign`, `ORACLE_AI_EVAL_APPROVAL=paid-simulator-five-trials-and-repeat`, and `ORACLE_AI_EVAL_SPLIT=train`, `holdout`, or `all`. Replace both global caps with the separately approved values and select a fresh report path. There is no unbounded default.

A full campaign creates 400 fresh trial environments and repeats the user request against each resulting state: **800 bounded coordinator runs**, with at most **8,000 provider attempts**. Training alone creates 300 trials / 600 runs / at most 6,000 attempts; holdout creates 100 trials / 200 runs / at most 2,000 attempts. These are request-count ceilings, not a cost estimate or completion guarantee. Known provider total-token usage settles the reservation at the configured conservative rate, releasing unused capacity. Unknown usage and ambiguous errors retain their full reservation. The configured rate must cover every billed token class, including thinking tokens. The metric `reserved_cost_micros` is the current conservative total of settled charges plus retained reservations; it is not the provider invoice. Reaching the campaign cap stops remaining trials and records incomplete coverage.

For a training replay, set `ORACLE_AI_EVAL_FIXTURES=A01,B01` to select unique training IDs. Selection is rejected for smoke, holdout, and all-case runs. Each selected case still gets five trials and a repeat. This creates a new candidate report, not an append or a claim that omitted cases passed. Keep holdout cases out of this tuning workflow.

A sibling `.billing.jsonl` file records every admitted attempt before network dispatch and every settlement immediately afterward, with flush and filesystem synchronization. An admission without a settlement retains its entire charge even after interruption. Both files are created exclusively and are never overwritten. Before starting another process, subtract all prior settled charges and unresolved reservations from the user's total allowance, then set the new process cap to the remaining amount. For older interrupted reports without this ledger, reserve a conservative upper bound for unreported in-flight work. Separate processes do not share a cap automatically.

Each report records the source/model/API/profile, corpus/prompt/schema/module hashes, registry revision, fixture seed and injected fault, individual attempt durations and usage, unknown usage, run budgets, sanitized receipts, final state, repeat-state comparisons, and grader failures. Exact moderate preset values, active/stored configuration, privacy exclusions, delivery duplication, resource duplication, and audience changes are checked independently of model prose.

The summary separates the 95% task-trial, 90% five-trial consistency, 90% recovery, and zero-tolerance safety/false-completion predicates. It includes sample sizes, a Wilson confidence interval, and p95 simulator duration. A successful test still does **not** prove release readiness: human-calibrated configuration/explanation quality and a separately authorized disposable Discord canary remain unscored. Paid smoke/campaign results must never be substituted for deterministic host security tests, and simulated Discord results are not evidence of real Discord parity.
