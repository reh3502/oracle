# Oracle module SDK

A module is a separate, operator-trusted native executable. Implement `Module` and call `serve_stdio(Arc::new(module))`. The SDK uses `oracle-rpc` over stdin/stdout; reserve stdout for protocol traffic. It provides source-level Rust ergonomics, not an in-process ABI or a native-code sandbox.

`Module::initialize(mode, global)` receives a tracked global task scope. Modules opting into protocol 1.1 through `protocol_minor_min: 1` instead receive `initialize_with_runtime(mode, global, RuntimeConfiguration)`. Its default implementation calls the original hook. `RuntimeConfiguration::data_directory` is an optional `PathBuf` chosen and validated by the host operator; a module that requires it must fail initialization when it is absent. It is not supplied by invocation input or inherited environment, and it does not sandbox a native module. `activate(GuildContext)` receives a fresh guild scope and epoch. Scope spawning is fenced atomically during quiescence; scopes cancel cooperatively, then abort and join yielding tasks after a shared two-second grace period. Keep task names static and errors free of secrets. Native work that never yields still requires the host process supervisor to kill and reap the executable.

`invoke(CallContext, operation, input)` receives an opaque invocation lease. `document_get`, `document_batch`, and `contract_invoke` attach that lease to typed requests. The context exposes no SQL, credentials, arbitrary RPC method, or replaceable guild authority. The host independently validates the live invocation, guild, generation, grants, schemas and CAS revisions. A retained context expires with its original RPC invocation. Guild quiescence also cancels ongoing calls and denies further callbacks.

The handshake order is `hello`, `initialize`, then `activate` before normal invocations. The SDK rejects mismatched session/generation and stale guild epochs. A migration-mode process admits only migration transforms and lifecycle/health calls; transforms receive documents and return revision-preserving `DocumentWrite` values, with no host client. The host validates and commits the migration.

`quiesce` fences and joins either one guild or the whole module. `deactivate` fences the guild before its hook. `shutdown` drains scopes and runs the shutdown hook before acknowledging. The host closes stdin after this response, and `serve_stdio` exits after transport handlers and scopes are joined. Unexpected EOF also drains scopes. Health reports safe task counts globally and per guild; payloads and panic text are excluded.

The executable examples live under `examples/modules/counter` and `examples/modules/dependent`. SDK tests use real duplex framed transport to cover scoped callbacks, retained-context expiry, migration mode isolation, per-guild quiescence, epoch fencing, and task joining. Host packaging, installation, bindings and process-group cleanup are tested separately by the host manager.

## Compatibility policy

The Rust SDK is a source API, currently workspace version `0.1.0` and unpublished. Pin SDK and contract dependencies to a reviewed Oracle commit and keep your lockfile. No stable Rust binary ABI, independently published crate support window, or automatic compatibility across source revisions is promised. Rebuild and run the module contract/lifecycle suite when updating that pin. Any future source API break must be documented with migration instructions before declaring a supported release.

The process contract is versioned separately from the crate version:

| Field | Current acceptance rule |
| --- | --- |
| `manifest_version` | Host package validation selects the supported manifest version; the SDK returns the declared manifest unchanged |
| `protocol_major` | Exactly `1` |
| `protocol_minor_min` | Exactly `0` or `1`; the host hello must select that exact minor |
| `host_api` | Valid SemVer requirement matching the deploying host API; checked by the host |
| `target` | Exact host target triple; example staging targets `x86_64-unknown-linux-gnu` |
| `version` | Valid module SemVer; independent of document schema version |
| Provided/consumed contracts | Explicit contract name and SemVer compatibility, plus a valid guild binding |

Protocol 1.0 keeps its strict `initialize` payload `{session,generation,mode}` and original hook. Protocol 1.1 requires `{session,generation,mode,runtime}`, where `runtime` is `{}` when no directory is configured, or `{"data_directory":"/absolute/operator/path"}`. A null `data_directory` also means absent. Unknown initialization/runtime fields, a missing or null runtime object in 1.1, any runtime field in 1.0, unknown versions, and session/generation mismatches are rejected before module code runs. There is no silent downgrade. A 1.0 module does not receive 1.1 fields.

Unknown manifest fields are rejected. Do not assume adding a field is backward-compatible. A breaking wire change requires a new major and explicit host/module support; extensions require qualification on both sides before claiming compatibility. Installation checks a manifest and artifact, and execution still requires a matching handshake, fresh identity and granted capabilities. A successful install alone does not qualify module behavior.

Document data versions are an independent persistence contract. Declare the current version, readable versions and explicit forward migration steps; the current version must be readable and future readable versions are invalid. The host refuses unsafe downgrades and preserves migration checkpoints. Advertising a readable older schema does not itself implement reverse migration or authorize loading against newer data. Keep old packages and a verified backup before upgrades; test interruption and resumption on both supported databases.

## Author a module

Start from the [counter or dependent executable](../../examples/modules/README.md) and its manifest, retaining the SDK transport and tracked scopes. Give your module a unique ID and declare bounded input/output schemas, operation deadlines, own-document collections and only needed capabilities. Explicitly declare dependencies, commands, event subscriptions and required intents. Keep Discord credentials, SQL and arbitrary outbound effect transport in the host; use scoped SDK callbacks. AI metadata currently exposes reviewed inspection operations and cannot grant authority.

Use document revisions for compare-and-swap updates and handle conflicts explicitly. Do not persist invocation contexts or spawn detached work: callbacks expire with their invocation and quiescence fences scopes. Keep stdout exclusively for framed RPC. Log only bounded, redacted diagnostics to stderr. A native module can bypass SDK discipline, so installation remains an explicit operator trust decision.

For configurable modules, implement the declared configuration hooks and report the actual applied revision. Validate schemas and destination requirements before claiming success. For event consumers, bound queues and retained data, deduplicate stable event IDs, expose drops and coverage gaps, and provide maintenance for expiry. The [activity-log example](../../examples/modules/activity-log/README.md) demonstrates configuration, retention and delivery receipts.

Before distributing a package, test schema rejection, missing grants/dependencies, cross-guild denial, invocation cancellation, stale callbacks, crash recovery, graceful/forced unload and upgrade interruption. Exercise real host install/load/activate/invoke/deactivate/unload, then restart and isolated restore. Keep modules separately distributed; adding an example must not install it into a default host. Package trusted executable bytes with their manifest, checksum and source/license provenance using the [development workflow](../../examples/modules/README.md). Record target/toolchain and qualification results under the [release policy](../../RELEASE.md).
