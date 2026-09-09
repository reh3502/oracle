# Oracle

Oracle is a Rust Discord bot framework under development. It provides scoped human controls, a durable operation/effect ledger, SQLite or PostgreSQL storage, and runtime loading of separately installed native modules. It ships **zero feature modules** and requires no Gemini key. The bootstrap and dynamic module runtime are implemented. AI management remains a later roadmap stage.

## Build and initialize

Linux and Rust 1.95 or newer are required. Prepare the pinned, locally editable Serenity checkout before invoking Cargo:

```sh
python3 scripts/prepare-serenity.py
cargo build --locked -p oracle
mkdir -p deployment
./target/debug/oracle --config deployment/oracle.json init
./target/debug/oracle --config deployment/oracle.json status
./target/debug/oracle --config deployment/oracle.json serve
```

`init` creates a private JSON config and database and refuses to overwrite an existing config. SQLite defaults to `state/oracle.sqlite` relative to the config directory. The state directory and control socket are private to the host OS user. `serve` holds deployment ownership, recovers unfinished operations, and emits a JSON `ready` event. SIGINT/SIGTERM stops admission, cancels and joins tracked tasks, and closes the host. Use an OS service manager for unattended operation.

For PostgreSQL, create a dedicated empty database and supply its connection URL through an environment variable:

```sh
./target/debug/oracle --config deployment/oracle.json init --postgres-url-env ORACLE_DATABASE_URL
```

Set `ORACLE_DATABASE_URL` in the process environment first. Config files contain the variable name, never the connection string. A PostgreSQL advisory lock prevents two hosts from opening the same deployment. Both backends use checksummed host migrations and optimistic revision checks.

## Human control and Discord

Edit the initialized config to add the guilds and specific users permitted to pause/resume Oracle. IDs are decimal strings. An example SQLite config:

```json
{
  "version": 1,
  "state_dir": "state",
  "database": { "backend": "sqlite", "path": "state/oracle.sqlite" },
  "guilds": [{ "guild": "123456789012345678", "operators": ["234567890123456789"] }],
  "discord": { "token_env": "DISCORD_TOKEN" }
}
```

Set the referenced bot token in the host environment. Oracle does not automatically source `.env` or a text document. Configure `discord: null` for an entirely offline host. Provider credentials are not read by the host, and no secrets are stored in the database or sent to a module.

```sh
./target/debug/oracle --config deployment/oracle.json serve
# In another terminal, while the host is running:
./target/debug/oracle --config deployment/oracle.json publish-commands
./target/debug/oracle --config deployment/oracle.json status --guild 123456789012345678
./target/debug/oracle --config deployment/oracle.json control --guild 123456789012345678 pause
./target/debug/oracle --config deployment/oracle.json control --guild 123456789012345678 resume --expected-revision 2
./target/debug/oracle --config deployment/oracle.json recovery --guild 123456789012345678
```

The running host reconciles `/oracle` and explicitly declared commands from active modules. `/oracle` includes status, pause/resume, structure operations, and module configuration. `publish-commands` requests an immediate reconciliation through the running host. The exact earlier status/control definition is upgraded in place; unrelated or changed definitions are conflicts. Unchanged commands keep their IDs. Discord controls require both the configured operator allowlist and Manage Server/Administrator permission. Replies are ephemeral; commands cannot select another guild or obtain credentials. Discord's default command permission is Manage Server.

Local control authenticates through the OS user's private Unix socket and can operate while the host is running. Module management, structure/configuration operations, and command publication require a running host. Basic status, control, recovery inspection, and backup can acquire exclusive deployment ownership when the host is stopped. Keep one config/state directory per deployment; restart to apply config changes. The local OS operator is trusted to administer all configured guilds.

An effect is recorded as `sent` before reaching its adapter. Cancellation, transport ambiguity and restart retain recovery state; an uncertain effect is never automatically resent. Recovery inspection is bounded. Structure plans retain partial outcomes and logical resource reservations; uncertain creates cannot be repeated blindly.

## Server structure and module configuration

Structure changes use saved plans. For example, this plans a Minecraft category with text and voice channels:

```sh
./target/debug/oracle --config deployment/oracle.json structure --guild 123456789012345678 plan --input '{"channels":[{"key":"minecraft.category","name":"Minecraft","kind":"category"},{"key":"minecraft.chat","name":"minecraft-chat","kind":"text","parent":"minecraft.category"},{"key":"minecraft.voice","name":"Minecraft Voice","kind":"voice","parent":"minecraft.category"}]}'
```

Review the returned plan and its permission changes. Use `structure --guild GUILD show --plan PLAN_ID` to retrieve it. If approval is required, use `approve --plan PLAN_ID --hash EXACT_HASH`; then use `apply --plan PLAN_ID`. These subcommands follow the same `structure --guild GUILD` prefix. Plans expire, bind to their author and deployment, and reject changed server facts. Repeating a completed setup reuses the saved resource IDs. Partial or uncertain outcomes remain recorded for inspection.

Discord exposes the same executor through `/oracle structure` and `/oracle module-config`. Large results arrive as ephemeral JSON attachments. Module configuration uses `inspect`, `plan`, `apply`, and `recover`; stored settings are verified against the running module's effective revision.

Modules explicitly declare their slash routes. Activation publishes only routes whose capabilities were granted. Deactivation and reload revoke the old runtime identity immediately, even while Discord still displays an old command. `status` reports command publication and event delivery state. See the separately installed [activity logging example](examples/modules/activity-log/README.md) for configuration and delivery checks.

## Runtime modules

Module management uses a running host's private control socket. Native modules are trusted
software running as the host OS user, **not sandboxed plugins**. Review their source and
package before explicitly trusting an installation. Installation copies and validates an
immutable package without executing it; `load` starts its process separately.

```sh
./target/debug/oracle --config deployment/oracle.json module install --source /path/to/package --trust-native
./target/debug/oracle --config deployment/oracle.json module load --digest SHA256_FROM_INSTALL
./target/debug/oracle --config deployment/oracle.json module activate --module fixture.counter --guild 123456789012345678 --grant storage.own
./target/debug/oracle --config deployment/oracle.json module invoke --module fixture.counter --guild 123456789012345678 --operation increment --input '{}'
./target/debug/oracle --config deployment/oracle.json module health
./target/debug/oracle --config deployment/oracle.json module deactivate --module fixture.counter --guild 123456789012345678
./target/debug/oracle --config deployment/oracle.json module unload --module fixture.counter
```

The counter is a development fixture, not an installed host feature. Build and stage it with
`python3 scripts/module-dev.py --stage-only`; see [fixture development instructions](examples/modules/README.md)
for reload and explicit upgrade workflows. Grant only the capabilities needed by the intended
operations. Each guild activation gets its own epoch, grants, dependency bindings and documents.

Unload preserves documents and desired guild activations but clears the desired process load.
An explicit later load restores eligible desired activations with fresh epochs. Deactivation
turns off one guild; a forced cutoff may terminate the shared process and restore other desired
guilds on a fresh generation. Required consumers are prevented from using unavailable providers.
Upgrade is explicit: install the incoming package, then use `module upgrade --module ID --digest HASH`.
It stops the old writer before migrations, retains resumable checkpoints, and refuses unsafe
downgrades. A failed migration stays unavailable until repaired or resumed.

Unexpected exits use bounded backoff and quarantine after three restart attempts in a crash
streak. `module health` reports recovery state; explicit load permits an operator retry.
Framework diagnostic effects use opaque invocation authority and a durable ledger; queued or
retried sends recheck that authority at the actual dispatch boundary. Uncertain sends require
reconciliation rather than automatic replay.

## Backup and isolated restore

Database backups include installed package manifests/digests, activation intent and module
records. They do not include executable package files, deployment config or credentials.
Keep the original trusted packages (or a separate copy of the immutable module artifact store),
config and protected secret recovery material alongside the database backup. A restored
new state directory needs those packages reinstalled with their original digest before load.
Restore clears desired module loading and pauses guilds; it never automatically executes code.

```sh
./target/debug/oracle --config deployment/oracle.json backup --output backups/snapshot-001
./target/debug/oracle --config restored/oracle.json restore --backup backups/snapshot-001
```

The output is a new **directory** containing a manifest, checksums and a native database snapshot. Backups can run through the live host's control socket. SQLite uses a consistent native snapshot; PostgreSQL requires compatible `pg_dump` and `pg_restore` executables. Their paths can be set with global `--pg-dump` and `--pg-restore` options. Database credentials are passed to the child process environment, not command-line arguments.

Before restore, manually create a new config with a distinct state directory and an absent SQLite database path or an empty dedicated PostgreSQL database. Do **not** run `init` on the destination: restore requires an empty destination. Keep Discord disabled (`"discord": null`) during the drill. Oracle verifies the manifest and schema, assigns a new deployment ID, pauses mutations, and makes pending effects uncertain because the source may have performed external work after the snapshot. Newly configured guilds in a restored deployment also start paused. Review recovery records and actual external state before explicitly resuming any guild. Never connect source and restored bots to mutate the same guild concurrently.

Failed or interrupted restore targets remain fenced by a durable restore marker. Retry using a new empty target; do not remove the marker to bypass validation.

Keep backup files private, copy them to separate storage, and periodically repeat this isolated restore drill. A copied database file without its verified manifest is not an Oracle backup bundle. Migrations and backups cover host data, not future third-party module artifacts.

## Development and qualification

Production crates separate value contracts, policy/orchestration, SQL repositories, Discord presentation and the executable composition root. The pinned Serenity upstream revision and minimal patch are reproduced by `scripts/prepare-serenity.py`; unexpected local fork edits are preserved and rejected for review.

```sh
cargo test --locked --workspace
python3 scripts/check-stage1.py --postgres-bin /usr/lib/postgresql/18/bin
python3 scripts/check-stage2.py --postgres-bin /usr/lib/postgresql/18/bin
python3 scripts/check-stage3.py --require-postgres --postgres-bin /usr/lib/postgresql/18/bin
```

The acceptance drill creates and stops a disposable PostgreSQL cluster and exercises both backends, native backups, isolated restores and the actual running host. Supply `--postgres-share` and `--postgres-lib` when using an extracted PostgreSQL installation. The ordinary Cargo suite skips its PostgreSQL contract unless `ORACLE_TEST_POSTGRES_URL` and `ORACLE_TEST_POSTGRES_RESTORE_URL` name disposable databases; the acceptance drill supplies both.

The Stage 2 runner covers process loading, activation, effects, upgrades, and developer reloads. The Stage 3 runner builds isolated configuration, event, collision, and logging fixtures; verifies command identities and both database backends; and checks Rust 1.95, Clippy, and formatting. Its reports stay under ignored `target/`.

The `prototypes/` directory contains process, effect-boundary, Discord, storage, Gemini and scenario experiments. They are development fixtures, not bundled feature modules. Its separate local suite is `python3 scripts/check-stage0.py --offline-only`; live checks require separately supplied credentials. No routine test sends paid provider requests or changes a Discord guild.

Build outputs, credentials, local design documents and agent instructions are excluded from Git.
