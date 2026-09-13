# Dandy's World module

This separately built Oracle module answers game queries from an immutable, attributed wiki snapshot. Queries use the Rust core directly and perform no network access, Python execution, or host callbacks. The module is not installed by the main workspace build.

Build the SDK executable with:

```sh
cargo build --locked --release --manifest-path modules/dandys-world/Cargo.toml --bin dw-module
```

Package `target/release/dw-module` from this directory with `manifest.json` using Oracle's operator package workflow. The module ID is `community.dandys-world`. Its manifest requires host API 1.1, protocol 1.1 and an operator-configured runtime data directory containing a published catalog. Missing or invalid snapshots fail initialization with an actionable error. Never put credentials or unrelated files in that directory.

The offline importer and `dw-query publish --catalog FILE --store DIRECTORY` create the validated store separately. Loading pins a complete snapshot for each query. A lifecycle-tracked monitor adopts published updates and rollbacks within about one second; in-flight queries retain their original catalog. Startup can recover the recorded previous snapshot if the current one is corrupt. Local operator commands `stage`, `review`, `approve`, `discard`, `rollback`, `backup`, `restore` and `recover` manage candidate review and recovery. Approvals require the exact active and candidate SHA-256 pair. Backup output must be an absent directory; restore preserves original source validation timestamps.

Version 0.3.0 writes a versioned active/previous pointer. Older 0.2.x executables cannot read that pointer: retain a pre-upgrade store backup when rolling back the executable. Corpus files and operator settings remain separate from the package.

The host must enable member-read access and configure the wiki citation prefix `https://dandys-world-robloxhorror.fandom.com/index.php?oldid=`. The host owns guild/channel/role policy, quotas, interaction destinations and mention suppression. The module's six public typed routes are:

- `/dw search query:Pebble` — find entities and exact IDs.
- `/dw lookup name:Pebble kind:toon field:health` — show sourced facts.
- `/dw compare left:EXACT_ID right:EXACT_ID field:health` — compare compatible fields.
- `/dw ask question:How does research work?` — supported deterministic game questions.
- `/dw sources name:Pebble kind:toon` — wiki revisions and validation timestamps.
- `/dw status` — cached catalog availability.

`lookup` and `sources` accept an `offset` copied from the previous reply; retain the other options. Entity kind distinguishes Toons, Twisteds, NPCs, floors, machines, mechanics, trinkets, items, events and other topics. Exact IDs also resolve ambiguity. The private `health` operation is operator-only and has no public route. No operation exposes AI tools, grants, storage callbacks or arbitrary JSON options to members.

Replies preserve complete facts, their conditions, uncertainty warnings and all cited source revisions. Oversized facts or facts needing more than five citations produce an explicit wiki navigation link without a partial game claim. Comparisons keep both sides together. Cached data older than the core's refusal window is not presented as a verified current fact. Wiki-derived text is attributed to wiki contributors under CC BY-SA 3.0; imported corpus files remain separate from this source package.

Verification:

```sh
cargo test --locked --manifest-path modules/dandys-world/Cargo.toml
cargo clippy --locked --manifest-path modules/dandys-world/Cargo.toml --all-targets -- -D warnings
DW_TEST_CATALOG=/absolute/path/to/catalog.json cargo test --locked --manifest-path modules/dandys-world/Cargo.toml --bin dw-module optional_full_catalog_human_reply_qualification
```

The last command checks human response bounds for every entity and distinct field in the supplied corpus. SDK lifecycle tests exercise real framed transport and disk snapshots, including missing data, snapshot adoption and rollback, pinned query readers, refresh denial persistence, activation fencing and callback absence. Host installation, Discord publication, policy enforcement and real interaction delivery require separate host integration checks.

Refresh runs inside the SDK global task scope. It is disabled unless an operator supplies private `refresh-settings.json` in the data directory with `enabled` and `source_access_qualified` both true, plus absolute `python` and `worker` paths. The worker must be `importer/refresh_worker.py` with its sibling importer files; Python needs the pinned importer requirements. Optional `previous` points to a complete saved source corpus for revision-based reuse. There is no configurable wiki URL. Qualify the source acquisition route before enabling it; the implementation's controlled HTTP tests do not qualify live wiki access.

The first enabled check is immediate; later checks run every six hours with bounded jitter. One worker has a fifteen-minute job deadline, fifteen-second request deadlines and at most two transient retries. Access denials stop the schedule across restarts. Operator configuration changes take effect on module reload; a new configuration identity resets the stopped schedule. Health reports the persisted attempt/result/due state without filesystem paths. Unknown or invalid configuration disables refresh while cached queries remain available.

The worker retains the store writer lock until its subprocess has exited and its staging directory is cleaned. It caps worker memory at 512 MiB and total corpus disk usage at 512 MiB; raw acquisition and candidate output are bounded separately and together. Global shutdown cancels and reaps the subprocess before finishing. A failed refresh cannot publish a partial catalog or advance source freshness. Changed facts require operator review; a successful source check alone does not authorize them.

Run the source/worker checks with the importer environment:

```sh
python -m unittest discover -s modules/dandys-world/importer -p 'test_refresh_*.py'
```
