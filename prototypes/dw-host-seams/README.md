# Dandy’s World host seam probe

An isolated Rust workspace that sketches member read authorization, typed command
inputs, bounded human replies, and an explicit external module data directory.
It tests current manifest/route decoding against the real `oracle-contracts`
crate. It does not modify or enable any production host behavior.

From the repository root:

```sh
cargo test --locked --manifest-path prototypes/dw-host-seams/Cargo.toml
cargo clippy --locked --manifest-path prototypes/dw-host-seams/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path prototypes/dw-host-seams/Cargo.toml -- --check
cargo run --locked --manifest-path prototypes/dw-host-seams/Cargo.toml
```

The proposed member lease has no host callback capabilities, even when an
operator invokes a member route. The public corpus can be read from the module’s
own immutable files. In particular, `storage.own` is not suitable for public read
authority because it currently grants both reads and writes.

The model receives authenticated identities/current policy from its caller.
Production policy freshness, atomic quota accounting, deadline/cancellation
concurrency, filesystem ownership/races, schema correspondence, negotiated
versioning and actual Discord publication remain integration work. Native
modules are trusted executables; callback restrictions are not an OS sandbox.
