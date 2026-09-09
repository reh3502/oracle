# P6: Minecraft setup operation contract

This standalone Rust prototype executes the Minecraft scenario against a stateful Discord fixture. The operation service inspects current scoped state, prepares an owner-bound plan, reserves effects durably, creates or reuses resources, and checks fresh readback of the entire plan before reporting completion. No model or live Discord credentials are involved in this deterministic gate.

From the repository root:

```sh
./scripts/check-p2-p6.sh
```

The shared script checks formatting, Clippy with warnings denied, locked release builds and both executable acceptance harnesses. Generated results stay in ignored `artifacts/`.

[The executor](src/lib.rs) creates one Minecraft category, two text channels and one voice channel using fixed host-approved Minecraft/staff audience profiles. Information posting is staff-only; ordinary Minecraft members can chat/connect elsewhere; everyone has no access. Existing compatible channels are reused with custom fields preserved. Host policy, actor/bot capability, hierarchy, guild/principal/run ownership and snapshot freshness are checked outside model output. Incomplete visibility and ambiguous candidates block writes.

A scoped purpose reservation is fsynced before each create. If the response is lost, reopening the store in a new run still blocks a duplicate; a name match cannot prove that an ambiguous create is resolved. Successful receipt IDs are stored before readback. Permission loss after one write preserves the partial state and stops remaining work. A successful-looking create response followed by incompatible readback does not produce success. Final verification checks resource cardinality even when a duplicate has a different audience, and catches earlier resources modified while later creates were in flight.

[The harness](src/main.rs) checks fresh and pre-existing layouts, repeat no-op after store reopen, hidden resources, permission/hierarchy denials, duplicate candidates, stale snapshots, copied plans, lost responses, partial revocation and corrupted readback. Expected final names, cardinalities, parenting and access values are asserted separately from the planner.

Scope: this validates the scenario's operation and permission decisions for the documented approved-profile fixture. The `Access` map is a normalized profile, not a general implementation of Discord's role/overwrite bitfield algorithm. Live Discord conformance is P3's separate gate. The store assumes one serialized mutation executor for a guild; cross-process locking, generalized profiles, durable resumable receipts and provider orchestration are later core implementation work. Unknown creates intentionally require reconciliation rather than speculative automatic matching.
