# Oracle module SDK

A module is a separate, operator-trusted native executable. Implement `Module` and call `serve_stdio(Arc::new(module))`. The SDK uses `oracle-rpc` over stdin/stdout; reserve stdout for protocol traffic. It provides source-level Rust ergonomics, not an in-process ABI or a native-code sandbox.

`Module::initialize(mode, global)` receives a tracked global task scope. `activate(GuildContext)` receives a fresh guild scope and epoch. Scope spawning is fenced atomically during quiescence; scopes cancel cooperatively, then abort and join yielding tasks after a shared two-second grace period. Keep task names static and errors free of secrets. Native work that never yields still requires the host process supervisor to kill and reap the executable.

`invoke(CallContext, operation, input)` receives an opaque invocation lease. `document_get`, `document_batch`, and `contract_invoke` attach that lease to typed requests. The context exposes no SQL, credentials, arbitrary RPC method, or replaceable guild authority. The host independently validates the live invocation, guild, generation, grants, schemas and CAS revisions. A retained context expires with its original RPC invocation. Guild quiescence also cancels ongoing calls and denies further callbacks.

The handshake order is `hello`, `initialize`, then `activate` before normal invocations. The SDK rejects mismatched session/generation and stale guild epochs. A migration-mode process admits only migration transforms and lifecycle/health calls; transforms receive documents and return revision-preserving `DocumentWrite` values, with no host client. The host validates and commits the migration.

`quiesce` fences and joins either one guild or the whole module. `deactivate` fences the guild before its hook. `shutdown` drains scopes and runs the shutdown hook before acknowledging. The host closes stdin after this response, and `serve_stdio` exits after transport handlers and scopes are joined. Unexpected EOF also drains scopes. Health reports safe task counts globally and per guild; payloads and panic text are excluded.

The executable examples live under `examples/modules/counter` and `examples/modules/dependent`. SDK tests use real duplex framed transport to cover scoped callbacks, retained-context expiry, migration mode isolation, per-guild quiescence, epoch fencing, and task joining. Host packaging, installation, bindings and process-group cleanup are tested separately by the host manager.
