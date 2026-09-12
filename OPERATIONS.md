# Operating Oracle

Oracle runs as one Linux OS user with exclusive ownership of one deployment. Native modules execute with that user's authority. Use a dedicated unprivileged service account, private state/config/backup directories and only reviewed packages. The framework installs no feature modules by default.

## Deploy and update

Build the pinned checkout with `python3 scripts/prepare-serenity.py` and `cargo build --locked --release -p oracle`. Initialize a new deployment using `target/release/oracle --config /absolute/path/oracle.json init`. For PostgreSQL, first create a dedicated empty database and use `init --postgres-url-env ORACLE_DATABASE_URL` with that variable set. Add guild/operator allowlists and optional Discord/AI settings using the [configuration guide](README.md). Oracle reads secrets from named environment variables; it does not source `.env` files.

Run `target/release/oracle --config /absolute/path/oracle.json serve` under a service manager as the dedicated account. Set an explicit working directory and absolute config path, supply secrets through the service environment, and capture JSON output privately. Allow time for SIGTERM shutdown and verify the host and module processes exit before starting a replacement. Restart applies config changes. Keep database ownership exclusive; do not run two configs against the same deployment.

Before an update, record the current executable/package digests, take an Oracle backup and rehearse an isolated restore. Stop the service, replace the executable with the qualified candidate, then start it and check `status`, `module health` and `recovery --guild GUILD`. Reconcile commands with `publish-commands` while the host is running. Keep the previous executable and original immutable module packages. Do not assume an older host can read a newer database schema or that an older module can read upgraded documents. If binary rollback is incompatible, restore the pre-update backup into a new isolated target and reconcile external state before switching over.

Backups omit package executables, configuration and credentials. Preserve those separately. Follow [the restore procedure](README.md#backup-and-isolated-restore), keeping Discord disabled and guilds paused in the restored target. Never remove a failed restore's durable marker or connect both copies to mutate the same guild.

## Partial failures and recovery

| Observation | Meaning and next action |
| --- | --- |
| Effect is `sent` or uncertain after timeout/restart | Discord may already have performed it. Inspect recovery records and actual external state before any new request; do not blindly resend. |
| Structure plan partially applied | Completed resource IDs and reservations survive. Inspect the saved plan and receipts; resolve uncertainty before attempting remaining work. |
| Module configuration unavailable or revision differs | Stored intent is not proof of effective configuration. Inspect/recover configuration and verify effective revision and delivery health. |
| Module crash or quarantine | Admission for the failed generation is revoked. Inspect safe health and fix the cause; explicit load allows a retry after bounded automatic recovery is exhausted. |
| Forced guild deactivation kills a shared process | Other guilds can be temporarily disrupted while desired activations recover on a new generation. Check each affected guild's health. |
| Upgrade migration fails | The module stays unavailable with resumable checkpoints. Repair/resume the forward upgrade; do not substitute an implicit downgrade or edit stored documents. |
| Stale command still visible in Discord | Publication can lag runtime revocation. Old runtime authority is denied; inspect publication status and reconcile commands. |
| Agent run incomplete, cancelled or budget-limited | Inspect saved receipts and reason. Cancellation blocks new work but an admitted effect can still settle. Resume cannot reset original limits or bypass uncertainty. |
| Logging drops or lacks attribution | Inspect queue/drop/coverage counters and retention health. A delivery probe cannot establish event coverage or invent an actor. |

Pause a guild before investigating unexpected mutations with `control --guild GUILD pause`. Pause does not undo effects already sent. Retain the private deployment and recovery records; avoid deleting evidence or clearing state to make health look green.

## Safe diagnostics

Start with local `status`, `module health`, `recovery --guild GUILD`, saved structure-plan inspection, module-configuration inspection and `agent --guild GUILD inspect RUN_ID` as appropriate. Use the running host's private control socket; stopped-host inspection must obtain exclusive ownership. Never make paid inference a health probe.

For a bug report include the host revision and digest, OS/toolchain/backend versions, safe error code, failing command shape, lifecycle phase, and whether an external effect is uncertain. Replace deployment/guild/user/channel IDs and paths with consistent placeholders. Preserve only the minimal non-sensitive receipt relationships needed to reproduce the problem. A useful report says, for example, `module upgrade --module MODULE --digest DIGEST failed; code=data_version_mismatch; module remains unavailable; checkpoint retained`.

Review every report before sharing. Configuration inspection, agent goals, module documents and Discord content can contain private data even when credentials are excluded. Do not attach raw environment dumps, database files, connection URLs, bot/provider keys, unrestricted stderr, panic payloads or provider responses. Module stderr is untrusted and may contain secrets. Share a redacted description and a source fixture instead. Keep original logs and generated qualification reports private and ignored.
