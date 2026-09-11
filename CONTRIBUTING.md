# Contributing to Oracle

Oracle currently implements stages 1–3: the durable host, trusted native module runtime, and shared human operations. Feature modules are separately installed. Stage 4 is in progress: `oracle-ai` contains provider, discovery, usage-accounting and durable-run foundations; the host agent loop and human controls are not yet integrated.

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

This runs Stage 2 qualification (including Stage 1) and Stage 3 qualification, using disposable databases, freshly built native fixtures, SQLite/PostgreSQL backup and restore drills, the Rust 1.95 toolchain, formatting and Clippy. Install that toolchain with `rustup toolchain install 1.95.0 --profile minimal`. For extracted PostgreSQL distributions, also supply `--postgres-share PATH` and `--postgres-lib PATH`. Individual stage runners remain available for diagnosis. Reports and subprocess logs remain under ignored `target/` and `evidence/` directories.

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
| `oracle-ai` | Gemini provider boundary, bounded discovery, usage accounting and agent run state |
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
