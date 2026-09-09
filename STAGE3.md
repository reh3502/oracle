# Stage 3: shared Discord and module operations

Implementation is in progress. The acceptance contract is Stage 3 in `docs/ROADMAP.md`.

## Work sequence

1. Shared permission rules and durable workflow records on SQLite and PostgreSQL.
2. Structure inspection, exact saved plans, approvals, logical resource reservations,
   guarded Discord writes, readback, and partial outcome recovery.
3. Module configuration prepare/commit/effective readback with generation fencing.
4. Human CLI/Discord commands and dynamic command reconciliation.
5. A separately installed logging module with the documented moderate/v1 preset,
   real event routing, destination checks, bounded queues and retention.
6. Integrated verification and operator documentation.

Each completed slice will be tested and committed. The live deployment stays on its
current build until a Stage 3 deployment is explicitly arranged.

## Required evidence

- Minecraft category/text/voice setup through the same operation service used by humans.
- No duplicate create after retry, restart, uncertain response or incomplete visibility.
- Actor and bot permissions, role hierarchy, exact permission approval and expiry checks.
- No send after fencing while queued or retrying; sent uncertainty remains durable.
- Config CAS, exact stored/effective revisions, failed prepare and interrupted apply recovery.
- Commands reconcile without deleting unrelated commands or admitting stale generations.
- Logging preset values, actual subscriptions, destination permissions and delivery readback.
- Both databases, prior stage regression checks, minimum Rust version and clean shutdown.

No Stage 3 completion is claimed by this progress record.
