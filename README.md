# Oracle

Oracle is a Rust Discord bot framework under development. It provides scoped human controls, a durable operation/effect ledger, SQLite or PostgreSQL storage, and runtime loading of separately installed native modules. It ships **zero feature modules** and requires no Gemini key. Stage 2 is implemented and locally qualified; see the [Stage 1 record](STAGE1.md) and [Stage 2 qualification and evidence](STAGE2.md). AI management remains a later roadmap stage.

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
./target/debug/oracle --config deployment/oracle.json publish-commands
./target/debug/oracle --config deployment/oracle.json serve
./target/debug/oracle --config deployment/oracle.json status --guild 123456789012345678
./target/debug/oracle --config deployment/oracle.json control --guild 123456789012345678 pause
./target/debug/oracle --config deployment/oracle.json control --guild 123456789012345678 resume --expected-revision 2
./target/debug/oracle --config deployment/oracle.json recovery --guild 123456789012345678
```

`publish-commands` explicitly creates `/oracle status` and `/oracle control action:pause|resume` in configured guilds. It verifies the command definition and refuses conflicting existing `/oracle` definitions. Starting the Gateway does not publish commands. Discord controls require both the configured operator allowlist and Manage Server/Administrator permission. Replies are ephemeral; commands cannot select another guild or obtain credentials. Discord's default command permission is Manage Server.

Local control authenticates through the OS user's private Unix socket and can operate while the host is running. Without a running host, commands acquire exclusive deployment ownership themselves. Keep one config/state directory per deployment; restart to apply config changes. The local OS operator is trusted to administer all configured guilds.

An effect is recorded as `sent` before reaching its adapter. Cancellation, transport ambiguity and restart retain recovery state; an uncertain effect is never automatically resent. Stage 1 exposes bounded recovery inspection. Domain-specific Discord operations and reconciliation arrive in Stage 3.

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
```

The acceptance drill creates and stops a disposable PostgreSQL cluster and exercises both backends, native backups, isolated restores and the actual running host. Supply `--postgres-share` and `--postgres-lib` when using an extracted PostgreSQL installation. The ordinary Cargo suite skips its PostgreSQL contract unless `ORACLE_TEST_POSTGRES_URL` and `ORACLE_TEST_POSTGRES_RESTORE_URL` name disposable databases; the acceptance drill supplies both.

[Stage 0 qualification](prototypes/STAGE0.md) retains the process, effect-boundary, Discord, storage, Gemini and scenario prototypes. They are development fixtures, not bundled feature modules. Its separate local suite is `python3 scripts/check-stage0.py --offline-only`; live checks require separately supplied credentials. No routine test sends paid provider requests or changes a Discord guild.

Build outputs, credentials, local design documents and agent instructions are excluded from Git.
