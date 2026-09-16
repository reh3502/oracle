# Windows launcher

Publish with .NET 10 SDK (Windows targeting works on Linux):

```sh
dotnet publish tools/windows-launcher/OracleLauncher.csproj -c Release -o /absolute/output
```

Ship `Start Oracle.exe` alongside `oracle-host.exe`, `.env`, and:

- `payload/oracle.json`: fresh deployment template, with AI disabled and the recipient's guild/operator policies.
- `payload/catalog/`: active snapshot store (`active` plus hash-named catalog JSON).
- `payload/module-package/`: complete Windows module package including `package.json` and executable.
- `payload/python/` and `payload/refresh-worker/`: embedded Python and the fixed wiki acquisition scripts.

`.env` contains one `DISCORD_TOKEN=...` assignment. Blank lines, comments and matching single/double quotes are supported. No shell expansion or interpolation occurs. The token is passed only in the host child's environment, never as an argument, in generated config, or in logs. Keep the release archive private.

Double-clicking opens the window and starts the bot. Start/Stop buttons control it. Closing waits for a graceful stop; if an initial command publication is already in progress, it first waits for that bounded operation to finish. If stopping fails the window remains open. The bot must stay open for Discord commands to work. Only one launcher instance is allowed per Windows session.

On first start, the launcher creates `%LOCALAPPDATA%/OracleSister` with a protected current-user/SYSTEM ACL, copies the catalog/module package, creates a fresh SQLite configuration, installs and activates Dandy's World, then publishes the Discord commands. Automatic publication is deferred until this explicit publication succeeds, preventing startup changes from interrupting the first publication. Each later startup explicitly synchronizes commands before enabling normal watching. Later starts reuse that deployment and its run data. No original database is included. Moving or replacing the release folder does not erase local data. AI is always disabled; an edited runtime config that enables it is rejected.

The wiki begins with the bundled catalog and runs bounded background source refresh. Source denial stops acquisition; material content changes may require operator review before publication. Existing freshness rules remain in force, including restrictions on stale wiki facts and run eligibility.

Verification:

```sh
dotnet run --project tools/windows-launcher/tests/LauncherTests.csproj
```

On Windows, `"Start Oracle.exe" --smoke-test C:\absolute\isolated-test-directory` exercises offline installation, module loading, accepting-generation health, stop, restart and another health/stop cycle. It disables Discord/refresh and never reads `.env`. Activation and guild queries require Discord services and are intentionally outside this check. Use a fresh directory. Results are written to `smoke-result.txt`, with exit code 0/1.

`"Start Oracle.exe" --live-check C:\absolute\fresh-live-check-directory` performs the normal authenticated startup using the adjacent `.env`, installs/activates the module, publishes Discord commands, verifies the loaded wiki and a cached lookup, then stops. It requires a fresh directory, a valid recipient token and the bot invited to the configured server. This mode can publish commands and acquire wiki data; it does not send chat messages. The sanitized result is written to `check-result.txt`; credentials and raw child output are never written. Run it only with authorization for the bot/server. A successful check verifies API-level behavior, not a human Discord client interaction.

## Assemble a private release

Build the host and DW module for the same target, then stage a fresh package. Linux maintainers need the MinGW x64 linker and Rust's Windows GNU standard library. Windows 11 x64 is the intended desktop target.

```sh
python3 scripts/prepare-serenity.py
rustup target add x86_64-pc-windows-gnu
cargo build --locked --release -p oracle --target x86_64-pc-windows-gnu
cargo build --locked --release --manifest-path modules/dandys-world/Cargo.toml --target x86_64-pc-windows-gnu --bin dw-module
dotnet publish tools/windows-launcher/OracleLauncher.csproj -c Release -o target/windows-launcher
python3 scripts/prepare-windows-python.py --output target/windows-python
python3 scripts/package-windows.py --host target/x86_64-pc-windows-gnu/release/oracle.exe --module modules/dandys-world/target/x86_64-pc-windows-gnu/release/dw-module.exe --launcher 'target/windows-launcher/Start Oracle.exe' --python-runtime target/windows-python --catalog /absolute/validated/catalog-store --guild GUILD_ID --operator USER_ID --channel RUN_CHANNEL_ID --env-file /absolute/private/.env --output target/releases/Oracle-Windows --zip
python3 scripts/verify-windows-package.py target/releases/Oracle-Windows --private
```

Use a new output directory for each release. The Python runtime script verifies pinned download hashes and bundles the source-pinned parser without system Python dependencies. Packaging copies only the selected immutable wiki catalogs, executables and deployment template; it does not copy an existing database, run records, credentials from another deployment, or refresh-worker state. Credentials are excluded from checksums and source control. Keep private ZIPs in ignored local output directories.

Run the offline launcher smoke test twice against the same private test directory to verify initial installation and restart. Smoke mode disables Discord and source acquisition. Native Windows validation is still necessary for OS behavior that Wine cannot reproduce, including named-pipe first-instance exclusivity and Windows symbolic links. Windows publication flushes files and uses atomic write-through moves; no Unix directory-fsync power-loss equivalence is claimed.

`"Start Oracle.exe" --close-check C:\absolute\fresh-close-check-directory` opens the actual WinForms window in offline mode and triggers Close after 50 ms while startup is in progress. It exercises the real FormClosing/cancellation/shutdown path, verifies that the owned host exited, and writes `close-result.txt` with exit code 0/1. No token is read.
