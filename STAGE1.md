# Stage 1: bootstrap and durable core

Stage 1 implements the empty framework host. The executable has no feature modules and no Gemini dependency. [README.md](README.md) contains build, configuration, local control, Discord publication, and backup/restore instructions.

| Roadmap acceptance | Implementation | Evidence |
|---|---|---|
| Empty framework boots on either backend | Composition root, strict JSON config, exclusive deployment ownership, checksummed migrations, zero modules | Real SQLite CLI tests and PostgreSQL host READY/status in the [local drill](evidence/stage1-local.json) |
| Local/human control works without Gemini | Private Unix socket; local init/status/pause/resume/recovery; Discord `/oracle status` and `/oracle control` using the same service | CLI tests with provider/token variables removed; real Serenity interaction fixtures for allowed and denied status/control; [live Gateway and command publication](evidence/stage1-discord-live.json) |
| Interrupted effects enter recovery | Durable Prepared/Sent/Verified/Unknown records, stable guild/purpose reservations, CAS, restart conversion, bounded recovery inspection | Both real repositories reopen Sent records as Unknown; cancellation/error and repeat-send tests; unknown effects cannot return to Sent |
| Scope failures cannot reach adapters | Authenticated guild context, configured guild boundary, operator allowlist plus Discord permissions; scoped SQL keys/foreign keys | Independent core adapter-count tests, real Discord interaction fixtures, both databases' cross-guild lookup and transition rejection |
| Backup restores into an isolated deployment | Full native SQLite snapshot or PostgreSQL custom dump, checksums/counts/schema checks, new deployment ID, mutations paused | Both native restore tests; extra table preserved; original source unchanged; newly configured restored guild paused; forged metadata and interrupted restore markers block startup |

The production crates are `oracle-contracts`, `oracle-core`, `oracle-storage`, `oracle-discord`, and the `oracle` executable. Prototype crates remain separate development fixtures. The host owns credentials, configuration, task supervision and database access; no module implementation is bundled.

## Verification

Run the complete local acceptance drill with PostgreSQL 18 tools:

```sh
python3 scripts/prepare-serenity.py
python3 scripts/check-stage1.py --postgres-bin /usr/lib/postgresql/18/bin
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo +1.95.0 check --locked --workspace --all-targets -j 4
```

For the locally extracted PostgreSQL distribution used in this workspace, also supply `--postgres-share prototypes/storage-fit/.local/pg/usr/share/postgresql/18` and `--postgres-lib prototypes/storage-fit/.local/pg/usr/lib/x86_64-linux-gnu`, with the corresponding `--postgres-bin` directory.

The local drill runs all workspace tests (29 Stage 1 tests plus 13 existing process-runtime tests), creates a disposable PostgreSQL cluster, boots a real PostgreSQL-backed host, controls it through its live socket, backs it up while running, restores into a separate database, checks source isolation, and verifies joined SIGTERM shutdown. It stops the cluster and records source hashes and toolchain/backend versions. The ordinary Cargo suite does not exercise PostgreSQL without its explicit test environment; the drill supplies it.

The live Discord example uses the production SQLite repository, connects until READY, requests Gateway shutdown, creates and reads back one `/oracle` command, removes only its owned unchanged command, and verifies the original command set. Its successful report records no commands before or after. It does not send messages, modify channels, or make provider requests. The user subsequently confirmed live human slash commands with a September 8, 2026 screenshot: `/oracle status` reported running with zero modules, and `/oracle control` paused then resumed the guild (revisions 2 and 3). This user-observed check is separate from the automated live report; handler authorization is also covered by deserialized interaction tests. Gateway shutdown records the adapter/manager task returning, not independent observation of every internal Serenity websocket task.

Integration exposed and fixed two Discord issues: conflicting Rustls crypto-provider features caused the Gateway task to panic despite working REST requests; Discord's omitted empty localization maps initially failed comparison against the command builder. Both have regression tests, and the live check passed after the fixes. PostgreSQL native-tool connection settings are mapped explicitly to environment variables, including encoded passwords and TLS settings.

## Operational boundaries

Stage 1 targets one active Linux host and SQLite/PostgreSQL as alternative backends. PostgreSQL writes use the connection that owns the advisory lock, so losing that connection aborts writes and fences the old host. A background health task stops the host on storage failure. Host tasks close admission, cancel cooperatively, then abort and join yielding work after a deadline; non-yielding native work requires the separately supervised process runtime planned for Stage 2.

Uncertain external effects are inspected, never automatically resent. Domain-specific reconciliation and Discord/module operations belong to Stage 3. Dynamic SDK/module lifecycle belongs to Stage 2; Gemini agent execution belongs to Stage 4. Restore checks are restart/transaction tests, not hardware power-loss certification. Remote PostgreSQL TLS parameter mapping is tested; the disposable PostgreSQL drill uses a local Unix socket.

A failed restore is quarantined by a durable marker. Retry into another fresh target instead of deleting the marker. Keep Discord disabled during restore review; source and restored deployments must not concurrently mutate the same guild.
