# Oracle durable storage

`Storage` implements the host-only `oracle_core::Repository` and `ModuleRepository` ports with concrete SQLx SQLite/PostgreSQL adapters. It does not authorize untrusted requests; those enter `CoreService`. Every operation/effect lookup and transition includes its guild scope, and effect foreign keys include the guild. New operation/effect input contracts carry revision zero; persisted records start at revision one.

## Startup and ownership

`Storage::open(DatabaseConfig)` acquires exclusive host ownership before migration or recovery. SQLite uses a lock beside the canonical database path; do not bypass the API with alternate database copies or hard links. PostgreSQL retains a dedicated connection holding a database-scoped advisory lock. All PostgreSQL write transactions run on that same connection. Its termination aborts outstanding transactions, and its replacement is never silently reconnected. Read/status/backup operations check the owning session and advisory lock, with a five-second health deadline. A lost owner is fenced until a new `Storage` is opened.

`close(&self)` drains repository operations, closes pools, and releases file/session ownership. It can be called again if an earlier close future was interrupted. Do this before reopening the same deployment or stopping the Tokio runtime.

[0001.sql](migrations/0001.sql) is immutable, shared SQL supported by both concrete backends. The migration transaction records its SHA-256 in `oracle_migrations`; unknown versions or changed checksums reject startup. The first migration creates deployment identity, guild state, operations and effects, including the unique guild/purpose reservation. Fresh initialization has no installed or loaded modules and no AI-provider dependency. Backend-specific immutable `0002-sqlite.sql` / `0002-postgres.sql` add module inventory, desired state, scoped documents and resumable migration tickets. Do not edit a shipped migration to change them.

SQLite uses an actual local file, WAL, `synchronous=FULL`, foreign keys, four pooled connections and a serialized `BEGIN IMMEDIATE` writer. PostgreSQL has five read connections and a serialized write connection that owns the advisory lock. Its writes use `synchronous_commit=on`. This ownership choice deliberately limits write concurrency; the P4 experiment separately establishes the scoped query contract under concurrent workloads.

## Recovery rules

On ordinary startup, `Sent` effects become `Unknown`; `Running` operations become `RecoveryRequired`. An unknown guild/purpose reservation is retained. It cannot transition back to `Sent`, and reserving the same purpose returns the original record. Prepared effects belonging to interrupted operations appear in recovery, and cannot be sent until the operation is explicitly resolved. No startup worker automatically replays effects.

`Verified`/`Failed` effect transitions require a receipt. An operation cannot be marked successful while any of its effects remains unverified, or marked failed while an effect remains prepared, sent or unknown. Scoped recovery queries are limited to 100 records.

## Native backup and isolated restore

```rust,ignore
let manifest = storage.backup(&new_bundle_directory, &PgTools::default()).await?;
let restored = Storage::restore(new_empty_target, &new_bundle_directory, &tools).await?;
```

A backup drains the host mutation barrier and captures the **entire native database**: SQLite uses `VACUUM INTO`; PostgreSQL uses a custom-format `pg_dump` archive. This includes all tables, not a hand-picked JSON export. The bundle manifest records the native file hash, ordered migration checksums, source deployment identity, host package version and table counts. The directory is newly created and mode 0700 on Unix. Files and directory metadata are synced before success.

Configure `PgTools` with explicit `pg_dump`/`pg_restore` executable paths when they are not on PATH. Native connection parameters, passwords and TLS settings are supplied through libpq environment variables, never a DSN in process arguments. URI percent-encoded passwords and TLS certificate paths are preserved. Use certificate file references for native PostgreSQL tooling. Credentials, PostgreSQL roles/cluster settings, external provider state and filesystem configuration are not database backups.

Restore accepts only a new SQLite filename or empty PostgreSQL database. It verifies the file hash before import, then native integrity/foreign keys, the migration checksum, source identity and all table counts. PostgreSQL restore runs in one transaction with errors fatal. The restored deployment receives a new identity; every existing guild is paused, and newly initialized guilds default to paused. Prepared and sent effects become unknown because the source may have performed their external effects after the backup snapshot. Resuming a guild does not clear unknown reservations.

A durable SQLite sidecar or PostgreSQL guard table is installed **before import**. Startup refuses a target whose restore was interrupted or failed, even if the native database itself was completely copied. The guard is removed only after validation, identity rotation and recovery fencing succeed. On failure, preserve the target for diagnosis and retry into a different fresh target. Do not delete a guard just to make the target boot.

Native subprocesses are killed if their waiting future is cancelled. A cancelled dump has no complete manifest; a cancelled restore remains guarded. The original database is never overwritten by restore.

## Verification

SQLite, independent of external tools:

```sh
cargo test --locked -p oracle-storage sqlite_contract -- --nocapture
cargo test --locked -p oracle-storage native_credentials_and_tls_are_environment_only
```

Both PostgreSQL URLs must name empty, disposable databases. The test also creates and drops private databases to exercise failed-restore quarantine, so its PostgreSQL test account needs `CREATEDB` (the disposable cluster helper supplies this):

```sh
ORACLE_TEST_POSTGRES_URL='...' \
ORACLE_TEST_POSTGRES_RESTORE_URL='...' \
ORACLE_TEST_PG_BIN=/absolute/postgresql/bin \
  cargo test --locked -p oracle-storage postgres_contract -- --nocapture
```

[The helper](scripts/test-postgres.sh) starts a fresh private Unix-socket cluster and stops it with an EXIT trap. It accepts `ORACLE_TEST_PG_BIN` or uses the PostgreSQL 18 binary extraction from the P4 prototype. It installs no system service. If the PostgreSQL variables are absent, the PostgreSQL test reports that it was not run; a default `cargo test` is not evidence that both backends were exercised.

The shared contract tests cover 16-client CAS contention, scope-negative reads/transitions, restart identity/recovery, immutable reservations, native backup/restore preserving an extra table, paused restored/new guilds, altered backup rejection, valid-file-hash but forged source-ID/count metadata, refusal to boot failed restores, migration checksum mismatch, and PostgreSQL owner-session termination. Native credential/TLS mapping has a separate unit check. These tests exercise process interruption/reopen and transaction boundaries, not a power-loss or hardware-corruption certification.

## Schema upgrades and module persistence

Fresh databases initialize both checksummed migrations atomically. Existing schema 1 databases require a successful full native backup **before** applying schema 2. `Storage::open_with_options(config, &tools, migration_backup_root)` creates a unique schema-1 bundle under the specified root. `open(config)` chooses the SQLite file's sibling `oracle-migration-backups` directory; PostgreSQL schema 1 requires explicit options and fails instead of guessing tools or a backup location. A backup failure leaves schema 1 intact. Existing schema 2 opens need no upgrade backup.

New native bundles use manifest format 2 with the ordered version/checksum list. Restore also accepts original format 1 schema-1 bundles. It validates incoming native data, identity, checksums and counts before upgrading under the restore guard; the validated original native bundle is retained as the pre-upgrade backup. Restored module inventory, documents, grants, bindings and desired guild activations survive. Every module's `loaded` desire is cleared, requiring explicit operator loading, and every guild remains paused until resumed. Database backups contain package inventory metadata; module executable artifacts belong in the host's broader backup package.

Installations bind an immutable package to a SHA-256 digest. Desired loading and guild activation are persisted intent, not proof of live module health or authorization. The host module manager validates manifests, collection schemas, capabilities and readable data versions before calling the host-only repository.

Documents are scoped by module, guild, collection and key. A batch permits at most 100 unique keys, each JSON value at most 64 KiB and the serialized batch at most 512 KiB. `expected_revision: None` creates only; `Some(revision)` updates or deletes by exact CAS. Any conflict rolls back the whole batch. Normal writes require the namespace's committed data version and are refused while its migration is active.

Namespaces start empty at version zero. Beginning a migration binds source/target versions and artifact digest; an identical begin resumes, a different digest or downgrade is refused. `migration_page` returns a sorted `(collection,key)` keyset page limited to 100 documents and 512 KiB and persists its read set and opaque next-cursor ticket. A commit must match digest, prior cursor, issued next cursor and completion flag, and cover every page document with exact CAS. Bounded additional writes allow rename/split transformations. Transformed/new documents carry the target version, so subsequent pages select only source-version documents. Page writes and cursor advancement are one transaction. Only a final issued page with no remaining source-version documents publishes the target namespace version. An empty zero-to-target migration follows the same page/commit protocol. Page tickets and progress survive closing and reopening storage.

Stage 2 verification extends the shared SQLite/PostgreSQL contract with immutable inventory; desired-state backup behavior; document scope negatives, 16-writer CAS and late-conflict rollback; count/value/batch bounds; digest, cursor, read-set and premature-completion rejection; a 153-document migration resumed after full pool close/reopen; byte-limited pages containing large documents; schema-1 backup failure preserving the old schema; mandatory pre-upgrade native backup; and original format-1 restore to isolated, paused schema 2. PostgreSQL uses a fresh disposable local PostgreSQL 18 cluster, not the running deployment.
