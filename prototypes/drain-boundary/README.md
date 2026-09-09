# P2: drain and actual send boundary

This standalone Linux prototype tests a host-controlled effect sender using real loopback TCP requests with Discord-shaped paths. AI, command and job entry points share one admission/lease gate. Quiesce closes new admission; existing leases may drain. Fencing wakes blocked rate waiters immediately and advances the guild activation epoch, and a shared process restart invalidates every old generation lease while recording the other guilds' interruption.

From the repository root run:

```sh
./scripts/check-p2-p6.sh
```

This checks formatting, Clippy with warnings denied, locked release builds, and the P2/P6 executable harnesses. P2 also starts, stops and reloads the actual P1 alpha binary. Generated measurements stay in ignored `artifacts/`.

The key boundary is `Sender::send` in [src/lib.rs](src/lib.rs). After each rate permit, including each 429 retry, it waits for socket writability, takes the same mutex as fence/revocation, validates the lease and writes a bounded request with one nonblocking `try_write` while still holding that mutex. A request either begins before fence or is denied after it. Partial writes and response loss remain unknown outcomes; they are never blindly retried. The append/fsync ledger records uncertainty before bytes can leave and remains readable after reopening, including after a torn final append. Per-effect ownership serializes concurrent duplicates across admission, dispatch and response handling. A 429 is an explicit rejected attempt, so only it permits another rate-controlled attempt.

The harness verifies stalled initial writes for all three caller kinds, draining existing work, independent guild activity, revalidation on a stalled 429 retry, lost-response recovery, revocation/expiration/completion, shared-generation disruption, physical process restart and 100 concurrent admission/quiesce/fence rounds. Checks observe the receiving TCP fixture and verify old PID disappearance.

This is a boundary experiment, not a complete HTTP/TLS client or Discord rate limiter. The semaphore is a deterministic rate-limit wait under test control. The fixture parser handles only its bounded canned HTTP responses. A production HTTPS integration must place the same fence check at its actual transport dispatch and every retry; the stock Serenity rate limiter is not covered by this sender. Effect IDs are host-owned; shared sender clones serialize each effect. Cross-process worker claims belong to the production core ledger. This append-only prototype journal is not a replacement for P4's database adapters.
