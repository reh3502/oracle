# Stage 0 P3: pinned Serenity baseline

This standalone workspace qualifies the selected development API and a narrow local fork. It is not the production Discord adapter. Run the checks below to generate local results.

## Reproduce offline

From the repository root:

```sh
python3 prototypes/serenity-baseline/prepare_fork.py
python3 prototypes/serenity-baseline/check.py
```

Requirements: Linux test host, Python 3, Git, Rust **1.95.0** with `rustfmt` and `clippy`, and network access to fetch pinned dependencies. `rust-toolchain.toml` declares the toolchain. Cargo locks are committed for both the upstream diagnostic workspace and the patched adapter. Build output and generated evidence stay in ignored `target/` and `artifacts/` directories.

`check.py` verifies the editable fork, builds/tests the **unmodified** upstream baseline, checks formatting, compiles every patched target, runs Clippy with warnings denied, and runs the adapter fixture suite. Its `artifacts/offline.json` records each command and exit code, toolchain, platform, upstream revision, patch digest and source hashes. Captured logs are adjacent to that report. These checks never read live credentials or invoke `live`.

## Exact source and editable fork

Upstream is `https://github.com/serenity-rs/serenity.git`, revision **98ec74223b0ff77fc4e8085d25569ea59e09a36f**. The source declares package version **0.12.5**, edition **2024**, Rust **1.95**; the commit, rather than the package version, identifies this `next` baseline. Selected features are `gateway`, `model`, `cache`, and `rustls_backend`, with default features disabled. There is no copied stable `client` feature. [Immutable manifest](https://github.com/serenity-rs/serenity/blob/98ec74223b0ff77fc4e8085d25569ea59e09a36f/Cargo.toml).

`upstream/` proves the original Git dependency builds. The main workspace has the same exact Git dependency plus a Cargo source patch to `target/serenity-fork`. `prepare_fork.py` creates a local `oracle-next` branch at that exact base and applies the recorded patch. It refuses to overwrite a different base or additional edits. This is a locally editable fork; **it has not been published as a GitHub fork**. Publish/advance it separately when the adapter is adopted.

### Patch ledger

[0001-preserve-unknown-dispatch.patch](patches/0001-preserve-unknown-dispatch.patch) changes three upstream files for two reproduced compatibility failures:

1. The original `MessageUpdateEvent` wraps a complete `Message`. A valid partial update containing only message/channel IDs cannot become that typed event. Upstream turns it into `DeserializedEvent::Unknown`, then discards it before user handlers. The patch exposes `Event::Unknown` and `FullEvent::Unknown` through the ordinary dispatch path. The adapter recovers only the recognized partial-update shape, validates IDs and content type, and keeps absent content distinct from an explicitly empty string. Other unsupported/malformed events remain unhandled. It does not invent a full message or update cache with absent fields. [Event model](https://github.com/serenity-rs/serenity/blob/98ec74223b0ff77fc4e8085d25569ea59e09a36f/src/model/event/mod.rs), [shard dispatch](https://github.com/serenity-rs/serenity/blob/98ec74223b0ff77fc4e8085d25569ea59e09a36f/src/gateway/sharding/mod.rs).
2. An interaction Gateway envelope whose JSON `d` key precedes `t` triggers a nested `RawValue` deserialization error in this baseline. The patch canonicalizes only the outer `t`/`d` order while preserving raw payload bytes. The same interaction fixture reproduces the failure against unmodified upstream and passes in both key orders with the patch. [Gateway decoding](https://github.com/serenity-rs/serenity/blob/98ec74223b0ff77fc4e8085d25569ea59e09a36f/src/model/event/mod.rs).

Keep these patches until upstream supports the same regression fixtures without them; then remove the corresponding patch and rerun both workspaces. There is no upstream issue or submitted PR yet. Raw unknown payloads remain untrusted and may contain interaction tokens; the adapter neither logs nor returns their full contents.

## Evidence boundaries

| Requirement | Offline evidence |
|---|---|
| Exact baseline on declared compiler | Separate unmodified Git workspace and metadata verification |
| Current dispatch API | Compiled `EventHandler::dispatch(&self, &Context, &FullEvent)` and real Gateway-to-FullEvent conversion |
| Partial/unknown events | Delete snowflakes, partial update missing/empty distinction, malformed/unknown refusal, upstream failure diagnostics |
| Interactions | Real command interaction model in both JSON key orders; ephemeral defer builder encodes type 5 / flags 64; normalization excludes token |
| HTTP and commands | Real Serenity HTTP client, channel/command builders, route/body capture, create/list/delete response parsing against loopback |
| Visibility gaps | Missing-name channel payload fails explicitly; unknown channel type survives decoding |
| Live Discord | Authorized create/readback/cleanup passed; `artifacts/live-authorized.json` |

The HTTP fixture uses a fake token, disables Serenity's limiter for documented API-proxy mode, disables inherited system proxies and redirects, and maps `discord.com` to a closed loopback port so a proxy bypass fails locally. It does **not** qualify rate-limit concurrency or P2's send-time fence. The current handler is a fixture recorder, not a production bounded event queue. Live Gateway authentication, real interaction timing, role hierarchy, complete visibility coverage and the broader production tests in `docs/DISCORD_INTEGRATION.md` are not claimed.

A diagnostic attempt initially left Serenity's default limiter enabled while configuring `HttpBuilder::proxy`. At this revision, the limiter path bypasses that proxy; a request using **only the invented fixture token** reached Discord and received **401 Unauthorized**. No valid token was involved and no mutation succeeded. The offline transport was then corrected and given the independent loopback DNS guard. This failed attempt is retained as provenance, not counted as a live test. [HTTP routing source](https://github.com/serenity-rs/serenity/blob/98ec74223b0ff77fc4e8085d25569ea59e09a36f/src/http/client.rs).

## Disposable-guild live gate

Use a bot/application and disposable guild explicitly authorized for this test. Provide these environment variables through a local secret mechanism; do not paste or commit tokens:

- `ORACLE_P3_DISCORD_TOKEN`
- `ORACLE_P3_GUILD_ID`
- `ORACLE_P3_APPLICATION_ID` (optional expected identity; discovered from the token otherwise)

Then, from this directory, explicitly invoke:

```sh
cargo +1.95.0 run --locked --bin live -- --execute artifacts/live.json
```

`live --inspect` performs read-only identity/channel/command preflight. Without `--inspect` or `--execute`, the binary exits before reading credentials. Offline checks only compile this binary. The live test inspects existing channels/commands; creates one uniquely named category, text channel and voice channel with explicit everyone-deny/bot-allow visibility overwrites; reads them back; creates one guild command and verifies publication and retention of unrelated command IDs; and cleans up its recorded resource IDs. It never replaces the whole command list, assigns roles or sends messages.

A receipt is persisted after each acknowledged creation. Cleanup runs after both success and failure, refuses changed/populated channels and categories with remaining children, and checks deletion readback. Interrupted processes or uncertain create responses can leave resources: inspect the persisted IDs and distinctive run prefix before any manual action. A failed or incomplete cleanup makes the gate fail. Permission-overwrite readback demonstrates the requested payload, not arbitrary-user effective permissions or immunity to administrator bypass.
