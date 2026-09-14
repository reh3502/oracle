# Dandy's World module

This separately built Oracle module answers game queries from an immutable, attributed wiki snapshot and stores guild-scoped community runs. Queries use the Rust core directly and perform no network access, Python execution, or host callbacks. The module is not installed by the main workspace build.

Build the SDK executable with:

```sh
cargo build --locked --release --manifest-path modules/dandys-world/Cargo.toml --bin dw-module
```

Package `target/release/dw-module` from this directory with `manifest.json` using Oracle's operator package workflow. The module ID is `community.dandys-world`. Its v3 manifest requires host API 1.4, protocol 1.2 and an operator-configured absolute runtime data directory. Wiki answers and new run publication require a validated catalog; existing runs and maintenance can continue when that catalog is unavailable. Never put credentials or unrelated files in that directory.

The offline importer and `dw-query publish --catalog FILE --store DIRECTORY` create the validated store separately. Loading pins a complete snapshot for each query. A lifecycle-tracked monitor adopts published updates and rollbacks within about one second; in-flight queries retain their original catalog. Startup can recover the recorded previous snapshot if the current one is corrupt. Local operator commands `stage`, `review`, `approve`, `discard`, `rollback`, `backup`, `restore` and `recover` manage candidate review and recovery. Approvals require the exact active and candidate SHA-256 pair. Backup output must be an absent directory; restore preserves original source validation timestamps.

Version 0.3.0 writes a versioned active/previous pointer. Older 0.2.x executables cannot read that pointer: retain a pre-upgrade store backup when rolling back the executable. Corpus files and operator settings remain separate from the package.

The host must enable member-read access and configure the wiki citation prefix `https://dandys-world-robloxhorror.fandom.com/index.php?oldid=`. The host owns guild/channel/role policy, quotas, interaction destinations and mention suppression. The module's six public typed routes are:

- `/dw search query:Pebble` — find entities and choose a result.
- `/dw lookup name:Pebble kind:toon field:health` — show sourced facts.
- `/dw compare left:EXACT_ID right:EXACT_ID field:health` — compare compatible fields.
- `/dw ask question:How does research work?` — supported deterministic game questions.
- `/dw sources name:Pebble kind:toon` — wiki sources and when they were checked.
- `/dw status` — cached catalog availability.

Replies use Discord cards with an answer first and wiki links below it. Dropdowns resolve ambiguous names and select details; Next and Back navigate without typing IDs or offsets. Ask a question opens a text form. Controls belong to the person who opened the card and expire after ten minutes or a module reload. `lookup` and `sources` also accept an `offset` for direct command use. Entity kind distinguishes Toons, Twisteds, NPCs, floors, machines, mechanics, trinkets, items, events and other topics. Exact IDs also resolve ambiguity. The private `health` operation is operator-only and has no public route. Wiki operations expose no AI tools, grants, or storage callbacks to members.

Replies preserve complete facts, their conditions, uncertainty warnings and all cited source revisions. Oversized facts or facts needing more than five citations produce an explicit wiki navigation link without a partial game claim. Comparisons keep both sides together. Cached data older than the core's refusal window is not presented as a verified current fact. Wiki-derived text is attributed to wiki contributors under CC BY-SA 3.0; imported corpus files remain separate from this source package.

Single-entity lookup and Ask cards can show the article’s main wiki image. Configure the host runtime `image_prefix` as `https://static.wikia.nocookie.net/dandys-world-robloxhorror/images/` before publishing an image-bearing catalog. Acquisition stores page-image and File-page metadata in checksummed `media.json`; it does not download image bytes. Images attach only to their own canonical entity article, never to child entities sharing a source page. Missing images or checks older than seven days produce text-only cards. The Image link credits the File-page revision separately from the wiki text license. New, changed, and removed images require candidate review; unchanged metadata rechecks may refresh normally.

Run operations are private host integration points; no run slash commands or shared-card transport are published by this package. They require separate `member_mutations` opt-in, `storage.own`, and authenticated host identity. Owners manage their own runs. The host can map configured moderator roles to the declared `manage_all_runs` permission. Operation JSON cannot supply an actor or guild. Run reads permit only `document_get`; mutations additionally permit `document_batch` on the three declared run collections. Existing wiki MemberRead operations still permit no callbacks.

Organized runs store host-selected Toon quotas and the host's selected Toon. Casual runs have no quotas and allow repeated optional Toon choices. Publishing signs up the host; both modes stop at eight members. Draft mode changes to casual, cancellations, and member removals use expiring actor/action/revision-bound confirmation tokens. Published eligibility stays pinned even when the wiki changes. New casual publication requires the reviewed playable roster still to match the approved catalog.

Run data version 2 is separate from the filesystem wiki catalog. The `initialize_runs` migration upgrades the empty version-1 namespace; it refuses unexpected documents. Back up the host database before upgrade. A version-1 reader cannot read version-2 run records. Rollback must retain those records and disable run writes rather than downgrading their schema.

Each mutation commits its roster, desired publication revision, indexes, and interaction receipt atomically. A saved publication intent is pending until a host transport confirms delivery. Unresolved published runs remain retained after completion. Maintenance expires inactive drafts after 24 hours, interaction receipts after 24 hours, and resolved terminal records after 30 days; it retains compact ID tombstones for another 30 days. It requires explicit Maintenance subscription and fresh host authority, independently of wiki refresh.

Configuration accepts optional `limits` fields: `drafts_per_owner` (default 1), `published_per_owner` (2), `drafts_per_guild` (20), `published_per_guild` (50), `published_per_day` (20), `retained_terminal` (700), `tombstones` (1024), and `receipts` (1000). Operators may lower these ceilings. Existing records remain readable when limits are lowered. Bounded moderator history retains actor, action, time, and affected member with each run; after 127 entries further moderator edits fail, with one final entry and space reserved for cancellation, or completion when already locked. No live records or receipts are evicted to free capacity.

Verification:

```sh
cargo test --locked --manifest-path modules/dandys-world/Cargo.toml
cargo clippy --locked --manifest-path modules/dandys-world/Cargo.toml --all-targets -- -D warnings
DW_TEST_CATALOG=/absolute/path/to/catalog.json cargo test --locked --manifest-path modules/dandys-world/Cargo.toml --bin dw-module optional_full_catalog_human_reply_qualification
```

The last command checks human response bounds for every entity and distinct field in the supplied corpus. SDK lifecycle tests exercise real framed transport and disk snapshots, including missing data, snapshot adoption and rollback, pinned query readers, refresh denial persistence, activation fencing and callback absence. Native host tests exercise installation, migration, member policy, restart, and maintenance. Real Discord publication and interaction delivery require separate transport qualification.

Refresh runs inside the SDK global task scope. It is disabled unless an operator supplies private `refresh-settings.json` in the data directory with `enabled` and `source_access_qualified` both true, plus absolute `python` and `worker` paths. The worker must be `importer/refresh_worker.py` with its sibling importer files; Python needs the pinned importer requirements. Optional `previous` points to a complete saved source corpus for revision-based reuse. There is no configurable wiki URL. Qualify the source acquisition route before enabling it; the implementation's controlled HTTP tests do not qualify live wiki access.

The first enabled check is immediate; later checks run every six hours with bounded jitter. One worker has a fifteen-minute job deadline, fifteen-second request deadlines and at most two transient retries. Access denials stop the schedule across restarts. Operator configuration changes take effect on module reload; a new configuration identity resets the stopped schedule. Health reports the persisted attempt/result/due state without filesystem paths. Unknown or invalid configuration disables refresh while cached queries remain available.

The worker retains the store writer lock until its subprocess has exited and its staging directory is cleaned. It caps worker memory at 512 MiB and total corpus disk usage at 512 MiB; raw acquisition and candidate output are bounded separately and together. Global shutdown cancels and reaps the subprocess before finishing. A failed refresh cannot publish a partial catalog or advance source freshness. Changed facts require operator review; a successful source check alone does not authorize them.

Run the source/worker checks with the importer environment:

```sh
python -m unittest discover -s modules/dandys-world/importer -p 'test_refresh_*.py'
```

Run the disposable PostgreSQL race and native backup/restore checks with installed PostgreSQL tools:

```sh
python3 modules/dandys-world/tests/qualify_postgres.py --pg-bin /absolute/path/to/postgresql/bin --output target/dw-run-checks
```

Optional `--module-binary` and `--catalog` paths also exercise native host migration, process recovery, maintenance, and existing wiki routes. The runner creates and stops its own Unix-socket-only cluster. It never uses an existing database. Supply `--pg-share` when the PostgreSQL installation requires an explicit shared-data directory.
