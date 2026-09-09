# Executable module fixtures

These are small SDK clients for lifecycle, durable document and upgrade checks. They contain no Discord or database client and are never linked into the host.

- `oracle-example-counter` identifies as `fixture.counter`, owns the `counters` collection, and provides `counter/v1` through `get`. `increment` reads key `main` and writes with its observed revision. Version 1 stores `{ "count": n }`; feature `v2` selects `manifest-v2.json`, stores `{ "total": n }`, and supplies the forward `count_to_total` migration from data version 1 to 2.
- `oracle-example-dependent` identifies as `fixture.dependent`. Its `read_counter` operation calls `counter/v1` through the host binding. The module and operation declare both `contracts.invoke` and `storage.own`: the latter permits the provider's document read under the inherited capability ceiling. The host still binds storage to the actual provider module and guild; the consumer cannot choose another namespace.

The counter starts one tracked global waiter and one tracked waiter per active guild. `wait` waits for cooperative invocation cancellation. `uncooperative` deliberately blocks a native worker indefinitely and exists only to exercise forced process termination. Do not invoke that probe outside a process supervisor or copy it into production modules.

## Local build and immutable staging

From the workspace root:

```sh
python3 scripts/module-dev.py --stage-only --profile counter
python3 scripts/module-dev.py --stage-only --profile dependent
python3 scripts/module-dev.py --stage-only --profile counter --v2
```

Stage-only is the default. It builds with `cargo build --locked`, using a separate target directory per fixture/profile so v2 cannot replace a v1 build output. It creates an immutable package directory under ignored `.local/module-dev/packages/` containing `package.json`, the executable, and `source-provenance.json`. Package metadata includes binary SHA-256, exact scoped source/lockfile hashes, git revision, Rust/Cargo toolchain, and license provenance. The repository currently declares no license, so the default records that explicitly; supply `--license` when an actual license is assigned.

The runner checks source hashes before and after compilation. Concurrent source edits fail staging and require a rerun. Repeated identical staging verifies and reuses the existing package. The reported `staging_sha256` identifies the staged JSON bytes; the host's authoritative installed digest comes from its install response and may differ.

The checked-in manifests target `x86_64-unknown-linux-gnu`; staging rejects a different native toolchain target. Building and staging does not execute the module or contact a host, database, Discord, or Gemini.

## Reload a running local host

Use a disposable development deployment with the guild already configured and the host running. The runner requires explicit native-code execution trust:

```sh
python3 scripts/module-dev.py --reload --profile counter \
  --config /absolute/path/oracle.json --guild 100 --trust-native

python3 scripts/module-dev.py --reload --profile dependent \
  --config /absolute/path/oracle.json --guild 100 --trust-native
```

`--oracle /absolute/path/oracle` selects another host executable; the default is `target/debug/oracle`. The runner checks for the existing control socket, builds and stages the fixture, installs it with explicit trust, unloads its previous generation if present, loads the returned installed digest, observes any activation restored by the host, and reports health. It calls `activate` only when the target guild is not already active, so repeated reloads preserve the restored activation and fresh epoch without a conflicting second activation. It grants exactly the fixture manifest's capabilities. The dependent profile binds `counter/v1` to `fixture.counter`, which must already be active in the same guild.

Repeating a profile rebuilds and replaces that module while the host remains alive. The runner refuses a blind replacement when module health is unavailable or the module is active in another guild. Host dependency checks can also reject unloading a provider used by another active module; failures propagate instead of forcing unrelated module changes.

## Explicit counter v1 → v2 upgrade

With v1 already loaded, request the data-version advance explicitly:

```sh
python3 scripts/module-dev.py --reload --profile counter --v2 --upgrade \
  --config /absolute/path/oracle.json --guild 100 --trust-native
```

`--upgrade` is required in addition to `--v2 --reload` and native-code trust. The runner builds and installs the immutable v2 artifact, then calls `module upgrade --module fixture.counter --digest INSTALLED_DIGEST --grace-ms 5000`. It never substitutes unload/load for this migration path. The host fences the old generation, performs the forward document migration, starts the replacement generation, and restores desired activations. The runner observes the restored guild epoch rather than activating it again.

Ordinary reload remains restricted to the same data version. Missing `--upgrade` rejects a v2 reload before contacting the host. If a newer data version is installed, an ordinary v1 reload fails before any installation or lifecycle mutation; the runner never attempts an implicit downgrade. The conservative check against other active guilds also applies to this fixture workflow. Use the host's explicit lifecycle commands when intentionally managing multiple guilds.

Durable documents are never rewritten by either Python script. If upgrade fails, the report retains the staged package, installed digest, configuration/state directory paths, the attempted command, and its safe structured error code when available. It does not claim rollback or delete recovery data; inspect the host's recovery state before proceeding.

Every build/run records a JSON report and build log under `.local/module-dev/runs/`. A failed host command returns a nonzero exit code and identifies the failed step and report path. A failure after unloading leaves that fact in the report; the script does not claim the old generation was restored or the new one activated. Raw host errors and environment variables are not copied into reports.

## Isolated reload verification

After building the current `oracle` executable, run:

```sh
python3 scripts/check-module-dev.py
```

The check creates and later removes a temporary SQLite deployment with no Discord configuration, scrubs provider/bot credential variables from subprocesses, starts one host, and runs two real trusted counter reloads followed by an explicit v2 upgrade. It verifies persistent value 7, fresh generation/epoch after both replacement paths, automatic restoration of activation, and an unchanged host PID/deployment. Read-only checks of this disposable SQLite file confirm `{ "count": 7 }` became `{ "total": 7 }`, namespace and document data versions are 2, and migration checkpoints are cleared. A subsequent ordinary v1 reload must be rejected before mutation while the v2 module continues serving value 7. Finally the check sends SIGTERM and requires joined shutdown with zero remaining tracked tasks. Reports and host logs remain under `.local/module-dev/checks/`; no existing deployment or `.env` file is used.
