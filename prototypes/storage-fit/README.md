# P4 — scoped storage fit

This standalone, disposable Rust/SQLx experiment runs the **same contract against real file-backed SQLite and PostgreSQL**. A missing or failing backend fails the run. It does not alter the root workspace, the P1 prototype, or a production database.

The checked-in [machine report](artifacts/p4-report.json) records matching semantic digests and the actual query plans. [P4_REPORT.md](P4_REPORT.md) maps the gate to the evidence and its limits.

## Reproduce

On the tested Ubuntu 26.04 x86-64 host, with Rust, Cargo, `apt-get`, and `dpkg-deb` available:

```sh
./prototypes/storage-fit/scripts/run.sh
cargo fmt --manifest-path prototypes/storage-fit/Cargo.toml --check
cargo clippy --manifest-path prototypes/storage-fit/Cargo.toml --all-targets -- -D warnings
```

The script downloads exactly `postgresql-18`, `postgresql-client-18`, and `libpq5` version `18.6-0ubuntu0.26.04.1`, and extracts them into the ignored `.local/pg` directory. It uses no sudo, package installation, container daemon, or system service. It creates a fresh UTF-8/C-locale cluster and a fresh SQLite file per run. PostgreSQL listens **only on a Unix socket in a private directory**. The EXIT trap stops the cluster, including failed harness runs; disposable data and server logs remain in `.local/run-*` for inspection. To reclaim those files, delete the relevant stopped run directory.

The downloaded PostgreSQL package hashes are in [postgres-packages.sha256](artifacts/postgres-packages.sha256). These identify the actual native artifacts used; there is no container-image digest because no container was used. The bootstrap depends on those exact packages remaining in the Ubuntu mirror. On another system, provision an equivalent disposable PostgreSQL database and run the binary directly:

```sh
P4_POSTGRES_URL='postgresql://USER@localhost/DISPOSABLE_DB?host=/PRIVATE/SOCKET&port=PORT' \
  cargo run --locked --release --manifest-path prototypes/storage-fit/Cargo.toml -- \
  /ABSOLUTE/output.json /ABSOLUTE/new.sqlite
```

The database must be empty and disposable. The harness creates fixed tables and refuses an existing SQLite filename. Do not point it at an application database. PostgreSQL connection strings are read from the environment and are not written into the report.

## Contract exercised

- The host creates a `Store` with fixed module, tagged scope, and guild identity. Requests contain collection/key/input only. Payload fields named `module` or `scope_id` cannot change that identity.
- Documents use conditional revisions. Insert expects revision zero; update/delete require the exact existing revision. The document and extracted indexes are updated in one transaction, with foreign-key cascade on deletion. An injected late batch conflict rolls back earlier writes.
- The fixed declared index shapes are `subject_created` (string equality, signed integer order) and `label` (UTF-8 bytewise string order). SQLite uses `BINARY`; PostgreSQL columns explicitly use `C`. Keyset order includes the document key to break ties. No caller SQL, arbitrary JSON paths, or unindexed predicates are accepted.
- Cursor strings are random, opaque host references. Their bounded table has 128 entries and a 60-second lifetime; each cursor is bound to module/scope/collection/index/equality. Successful use consumes the reference. Scope/query tampering, forgery, and expiry are checked. Cursor state does not survive a host restart.
- Logging streams have scoped producer dedup keys and monotonic sequence numbers. Dedup retries return the original sequence and body; sequence gaps are allowed because reservations may be consumed by duplicate appends. Indexed kind/timestamp reads are also exercised.
- SQLite has four real pool connections, WAL, foreign keys on every connection, a serialized writer lane, `BEGIN IMMEDIATE`, and a five-second busy timeout. PostgreSQL has five independently verified backend connections and per-stream row serialization through `UPDATE` inside transactions. Sixteen client tasks race on both engines.
- Limits are 100 mutations, 512 KiB encoded batch, 64 KiB document/stream body, 100 records and 512 KiB serialized records per returned page. SQL fetches at most 100 candidate rows, so host-side candidate buffering is additionally bounded at roughly 6.4 MiB; the smaller wire-page limit does not pretend to be the candidate-buffer limit.
- Module migration runs in explicitly quiesced harness traffic. Data, rebuilt indexes, and a durable key checkpoint commit atomically. Batches stop at their row or body-byte budget. An injected failure before commit rolls back, and a newly opened connection pool resumes from the durable checkpoint. Unsupported downgrade requests fail.

The transport prototype's 1 MiB envelope is a separate P1 boundary; this experiment measures the module storage payload limits. The `512 KiB` migration budget counts transformed document bodies within the host transaction, not an RPC envelope. It does not expose a database transaction or arbitrary closure to a module.

## Artifact record

- SQLx **0.9.0**, concrete `SqlitePool`/`PgPool`; no `Any` driver. [SQLx API](https://docs.rs/sqlx/0.9.0/sqlx/)
- The exact dependency graph and registry checksums are in [Cargo.lock](Cargo.lock).
- Tested bundled SQLite **3.51.3**, `libsqlite3-sys` **0.37.0**. The complete `sqlite_source_id()` is recorded in the machine report. This prototype qualifies that exact library, not every SQLite version or platform.
- Tested PostgreSQL **18.6**, Ubuntu package build **18.6-0ubuntu0.26.04.1**. [PostgreSQL initialization options](https://www.postgresql.org/docs/18/app-initdb.html)
- [toolchain.txt](artifacts/toolchain.txt), [build.sha256](artifacts/build.sha256), and the report contain the tested compiler, executable hash, dependency lock hash, and schema hash.

All checks run inside the executable harness; `cargo test` alone does not run the two database contracts. Elapsed times in the report describe one local correctness run and are not comparative throughput benchmarks.
