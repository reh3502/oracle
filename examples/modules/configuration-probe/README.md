# Configuration test fixture

This separately compiled executable tests the host configuration protocol. It is
not a logging module and is not included in the host's runtime dependencies.

Its schema exposes controlled failure switches: prepare rejection, delayed
preparation, mismatched effective readback, and process exit before acknowledgement.
`crash_marker` is a test-owned temporary file used to make that exit happen once
across process and host restarts. No production module should expose these probes.

Run from the repository root:

```sh
cargo build -p oracle-fixture-configuration-probe
ORACLE_CONFIGURATION_PROBE="$PWD/target/debug/oracle-fixture-configuration-probe" \
  cargo test -p oracle-modules --test configuration -- --ignored
```

The tests use temporary SQLite databases by default. To run the same cases on
PostgreSQL, set `ORACLE_TEST_CONFIGURATION_POSTGRES_URL` to a disposable database.
Each case uses a separate guild scope; no deployment database or Discord token is
read. Tests inspect real process responses and durable configuration records.

The `events` feature adds private event delivery and controlled host notifications.
Build it separately so the default configuration fixture keeps its original manifest:

```sh
cargo build -p oracle-fixture-configuration-probe --features events --target-dir target/event-probe
ORACLE_EVENT_PROBE="$PWD/target/event-probe/debug/oracle-fixture-configuration-probe" \
  cargo test -p oracle-modules --test events -- --ignored
```

These cases verify queue bounds, unload fencing, configuration/intent changes
while a send waits, durable notification replay, and stale command bindings.
The transport uses controlled local readiness; it does not contact Discord.
