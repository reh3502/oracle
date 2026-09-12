# Contributing to Oracle

Oracle provides the durable host, trusted native module runtime, shared human operations and opt-in Gemini coordinator with scoped dynamic tools, durable accounting and CLI/Discord run controls. Feature modules are separately installed. Deterministic qualification and paid live-model evaluations are separate gates.

## Local workflow

Use Linux, Rust 1.95 or newer, and Python 3.11 or newer. Install Rust's `rustfmt` and `clippy` components. From the repository root:

```sh
python3 scripts/prepare-serenity.py
cargo build --locked -p oracle
python3 scripts/check.py
```

The check command validates crate boundaries, tests the Python checker, verifies the pinned Serenity patch, checks formatting, runs the ordinary workspace tests, and runs Clippy with warnings denied. It stops on the first failure. No live deployment configuration is loaded. On a fresh checkout, dependency preparation needs network access; subsequent builds can use Cargo's cache.

For a focused change, use `cargo test --locked -p CRATE` and `cargo fmt -p CRATE`. To exercise separately installed modules, follow [the module development guide](examples/modules/README.md) and [the SDK guide](crates/oracle-module-sdk/README.md).

The ordinary workspace suite skips subprocess qualification fixtures and PostgreSQL checks that require explicit setup. Before merging changes to lifecycle, persistence, authorization, or transport, run the full gate:

```sh
python3 scripts/check.py --full --postgres-bin /usr/lib/postgresql/18/bin
```

This runs Stage 2 qualification (including Stage 1) Stage 3 qualification, and the Stage 4 offline agent gate, using disposable databases, freshly built native fixtures, SQLite/PostgreSQL backup and restore drills, the Rust 1.95 toolchain, formatting and Clippy. Install that toolchain with `rustup toolchain install 1.95.0 --profile minimal`. For extracted PostgreSQL distributions, also supply `--postgres-share PATH` and `--postgres-lib PATH`. Individual stage runners remain available for diagnosis. Reports and subprocess logs remain under ignored `target/` and `evidence/` directories.

## Code boundaries

| Crate | Responsibility |
| --- | --- |
| `oracle-contracts` | Serializable values and versioned process contracts |
| `oracle-core` | Authorization, durable effect orchestration, repository ports |
| `oracle-task-scope` | Tracked task ownership and cancellation |
| `oracle-rpc` | Bounded bidirectional RPC framing and dispatch |
| `oracle-process` | Trusted executable supervision and process termination |
| `oracle-module-sdk` | Module-side API; no host implementation or SQL/Discord adapters |
| `oracle-modules` | Installation, generations, activations, configuration and event routing |
| `oracle-operations` | Structure planning/execution and durable command reconciliation |
| `oracle-storage` | SQL adapters, migrations and native backups |
| `oracle-discord` | Discord ingress, presentation, observation and fenced transport |
| `oracle-ai` | Gemini provider boundary, bounded coordinator, discovery and durable accounting |
| `oracle` | CLI, local control and concrete host composition |

`scripts/check-architecture.py` checks production and build dependencies, including target-specific and renamed dependencies. Adapter dependencies used only by tests are allowed. When introducing a crate or moving a boundary, update the checker deliberately and explain the dependency direction in the change.

Keep policy independent of concrete I/O. Introduce traits at replaceable ports; use concrete types and enums for internal implementation. Separate parsing and presentation from shared execution so local and Discord commands keep the same authority checks. Keep public exports stable when moving implementation into private modules.

Preserve these runtime rules during cleanup:

- Guild authority, activation epochs, generation identity and registry revisions are distinct and must be checked at their existing boundaries.
- Cancellation and revocation must still reach queued sends. An uncertain effect cannot be blindly replayed.
- SQL batches and migration checkpoints remain atomic. Do not rewrite applied migration files; add a new migration for schema changes.
- The host owns credentials and connections. Module RPC exposes scoped operations and documents, never raw SQL or unrestricted Discord endpoints.
- Shutdown joins tracked work and reaps module processes. Native modules remain explicitly trusted software running as the host user.

## Repository hygiene

Commit source, source fixtures, reusable checks and maintainer-facing instructions. Keep credentials, deployment state, databases, build output, local design documents, agent instructions, stage records and verification reports ignored. Preserve the exact Serenity base and recorded patch; the preparation script rejects unexpected fork edits rather than overwriting them.

## Updating the Serenity fork

The upstream revision is pinned in both `Cargo.toml` and `scripts/prepare-serenity.py`; `patches/serenity-preserve-unknown-dispatch.patch` records the entire permitted local diff. The editable checkout lives at ignored `target/serenity-fork`. The preparation script refuses unexpected bases or edits and never overwrites them.

Before updating, preserve any local fork work and inspect its diff. Prepare a separate scratch checkout of the proposed upstream commit, apply or rework the narrow patch there, and review changes to gateway events, HTTP, command schemas and dispatch fencing. Change the revision in both pin locations, regenerate the recorded patch in the format produced by `git diff --abbrev=8`, and regenerate `Cargo.lock` through Cargo. Preserve the previous checkout outside the canonical fork location before preparing the new one; do not delete unreviewed edits to satisfy the verifier.

Run `python3 scripts/prepare-serenity.py` against the replacement checkout, then the full developer gate and Stage 5 qualification. Include real disposable-guild checks before calling the new pin release-qualified: offline fixtures cannot prove Discord parity. Commit pin, patch and lockfile changes together with any necessary adapter changes, explaining why the patch is still needed. Never commit the generated fork itself. Follow [release qualification](RELEASE.md) and [deployment updates](OPERATIONS.md) before operating the new build.
