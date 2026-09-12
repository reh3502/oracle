# Dandy's World module

This separately built Oracle module answers game queries from an immutable, attributed wiki snapshot. Queries use the Rust core directly and perform no network access, Python execution, or host callbacks. The module is not installed by the main workspace build.

Build the SDK executable with:

```sh
cargo build --locked --release --manifest-path modules/dandys-world/Cargo.toml --bin dw-module
```

Package `target/release/dw-module` from this directory with `manifest.json` using Oracle's operator package workflow. The module ID is `community.dandys-world`. Its manifest requires host API 1.1, protocol 1.1 and an operator-configured runtime data directory containing a published catalog. Missing or invalid snapshots fail initialization with an actionable error. Never put credentials or unrelated files in that directory.

The offline importer and `dw-query publish --catalog FILE --store DIRECTORY` create the validated store separately. Loading retains one snapshot for the process lifetime, so publishing a different active snapshot does not change in-flight or subsequent answers in that process. Reload the module to adopt it. Automated wiki refresh is a later stage.

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

The last command checks human response bounds for every entity and distinct field in the supplied corpus. SDK lifecycle tests exercise real framed transport and disk snapshots, including missing data, immutable snapshot retention, activation fencing and callback absence. Host installation, Discord publication, policy enforcement and real interaction delivery require separate host integration checks.
