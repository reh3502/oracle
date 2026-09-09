# P2 acceptance report — 2026-09-08

**Confirmed for the host-controlled transport boundary prototype.** The locked release harness, formatting and Clippy checks pass. [Retained evidence](../evidence/p2-2026-09-08.json) records exact source/compiler/binary hashes and every acceptance check.

| Requirement | Observed evidence |
| --- | --- |
| Race AI/command/job admission against quiesce | All entry kinds share the gate; quiesce denies new leases; 100 concurrent admission/fence rounds pass |
| No new send after fencing | Stalled initial requests and a stalled 429 retry finish fenced with no later send; sender validates under the fence mutex immediately around nonblocking socket write |
| Cancel queued work promptly | Pending initial and retry calls resolve within 300 ms after fence without releasing a rate permit |
| Already-sent ambiguity retained | Lost response remains `UnknownOutcome` after journal reopen; duplicate retry does not resubmit |
| Crash-safe uncertainty and duplicate ownership | Torn final append recovers earlier fsynced uncertainty; two deterministically concurrent same-effect calls produce one request |
| One-guild forced-restart disruption explicit | Receipt names both interrupted guilds and new generation; actual shared fixture process is reaped and replaced; old handle fails and other-guild work uses the replacement |

Three review regressions were reproduced before correction: a fenced waiter remained blocked without a new rate permit; a torn append prevented reopening the journal; concurrent same-effect calls produced two sends. All corresponding checks pass in the final executable.

The transport is real loopback TCP with a bounded HTTP fixture and deterministic semaphore rate waits. No claim is made that stock Serenity's internal limiter has these semantics, or that this is a production HTTPS adapter. The learned contract is to retain host control at the final dispatch/retry boundary and to keep effect ownership/uncertainty independent of module lifetime. See [README.md](README.md) for reproduction and integration assumptions.
