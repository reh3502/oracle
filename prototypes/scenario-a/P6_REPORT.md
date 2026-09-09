# P6 acceptance report — 2026-09-08

**Confirmed for the deterministic Minecraft operation scenario.** The locked release harness, formatting and Clippy checks pass. [Retained evidence](../evidence/p6-2026-09-08.json) records exact source/compiler/binary hashes and the acceptance checks.

| Required condition | Observed result |
| --- | --- |
| Fresh guild fixture | Exactly one category, two text channels and one voice channel; expected parent IDs and approved access profiles |
| Pre-existing resources | Compatible category/chat reused; only missing resources created; custom topics preserved |
| Hidden resources | Incomplete inspection returns `VisibilityIncomplete`, with zero writes |
| Role hierarchy and permissions | Actor, bot, overwrite authority, hierarchy and host-policy denials cause zero writes |
| No unauthorized permission expansion | Fixed approved Minecraft/staff profiles exclude everyone, restrict info posting to staff, and retain intended chat/voice access |
| Repeat no-op | After durable store reopen, all four resources are reused and verified with no additional creates |
| Lost response | Create reservation survives reopen and new run; unresolved outcome blocks a duplicate even when a same-name resource is visible |
| Stale/partial/racing state | Stale snapshot or copied plan rejected; permission revocation stops remaining writes; corrupt readback and external duplicates prevent success |

Independent review exposed three false-completion cases, each reproduced before its fix: an exact duplicate introduced during create, an earlier resource edited while later creates were running, and a duplicate with a different audience. Final verification now checks a fresh whole-plan snapshot, unique candidates, bound IDs, parenting and profiles. All regressions pass.

The fixture uses normalized approved access profiles, not a general Discord permission-bitfield implementation. One serialized guild mutation executor owns the journal. Live endpoint compatibility is covered separately by P3; generalized core operation orchestration remains later implementation work. See [README.md](README.md).
