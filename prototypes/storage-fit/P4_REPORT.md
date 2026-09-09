# P4 acceptance report

**Result: PASS for the representative storage-fit gate.** Both real backends return the same canonical semantic digest. See the current [raw report](artifacts/p4-report.json) for run-specific timing, dependency hashes, SQLite source ID, PostgreSQL build, and unmodified EXPLAIN output.

| Gate requirement | Direct evidence |
|---|---|
| SQLite and PostgreSQL, concurrent clients | Identical harness; 16 tasks per backend; four SQLite connections with actual WAL file and per-connection FK checks; five distinct PostgreSQL backend PIDs. |
| Twitch notification dedup/CAS | Sixteen concurrent updates expect the same revision: one succeeds, fifteen return Conflict, final revision is two. A two-write batch whose second CAS fails leaves its first insertion absent. |
| Same module/guild scope | Other-module and other-guild reads are empty. Forged scope fields in document payloads have no effect. Stream dedup keys are independent across guilds. Cursor tampering/forgery/expiry fail. |
| Moderation lookup and deterministic indexes | 6,000 seeded cases; subject equality returns exactly 120 matching cases through keyset pages. Signed integers include both i64 extrema and ties; UTF-8 labels have identical bytewise ordering. Index update removes old entries; delete cascades. Orphan index insertion fails with a real FK violation. |
| Logging streams | Sixteen clients each append 32 distinct events and immediately retry every append. Exactly 512 records remain, duplicate calls return the original sequence, read sequences strictly increase, and indexed kind/timestamp queries preserve their order and bounds. |
| Bounded queries | All data SELECTs have a primary-key equality or indexed prefix/keyset plus SQL LIMIT <=100. Returned record JSON is <=512 KiB. Twelve 60 KiB documents require multiple pages without loss. Invalid page size, document size, batch count/bytes, index name, and index scalar type fail. |
| Bounded migration | 137 documents commit in batches 37/37/37/26. Failure before commit leaves version one; reopening a real pool resumes at the durable checkpoint. Final records have exactly one revision increment and version two, with rebuilt indexes. Twelve large documents split into 8/4 despite a 100-row request. Invalid downgrade fails. |
| No representative full scans | SQLite `EXPLAIN QUERY PLAN` must show indexed SEARCH, and rejects SCAN. PostgreSQL `EXPLAIN (ANALYZE, BUFFERS)` must show index access and rejects Seq Scan. The checks cover Twitch key lookup, moderation index join, logging sequence page, logging kind/time page, and migration keyset page. No planner knobs disable sequential scans. |

The explicit SQLx adapter contract fits these workloads without an API for arbitrary relational queries. PostgreSQL's plan uses an index-only lookup plus keyed document access for moderation; SQLite uses its covering lookup index and document primary key. This supports continuing with the scoped API for these representative operations.

## Limits of this result

This is not production storage readiness. Scope construction is the trusted host seam; P1/P2 own RPC identity and fencing. The harness quiesces migration traffic by sequencing operations; it does not implement the production activation/migration coordinator. The interrupted migration uses deterministic transaction rollback plus pool reopen, not power-loss fault injection. Framework startup migration locks/checksums, migration artifact admission, full installation quotas/retention, cancellation, backup/restore, remote-effect recovery, and backend transfer remain their own implementation gates. The prototype's fixed schema is created once in a fresh database, and its schema digest is recorded.

The exact SQLite library and native PostgreSQL package build above are tested; the result does not promise compatibility with all SQLite versions, operating systems, or PostgreSQL deployment configurations. The disposable database contains no user data. No live Discord/Twitch connection is required or used for the local storage semantics.
