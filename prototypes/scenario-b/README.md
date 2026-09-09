# P7 logging configuration scenario

This standalone Rust workspace exercises the Stage 0 logging-module contract with a **real, separately compiled subprocess** and deterministic local delivery/subscription fixtures. It does not connect to Discord or implement the production logging data plane.

```sh
python3 prototypes/scenario-b/verify.py
```

The verifier runs locked tests, Clippy with warnings denied and formatting checks, then records command output, compiler versions and executable/source digests in [local-report.json](local-report.json). It requires the sibling P1 process-runtime source dependency. The activity-log executable is installed by copying its built artifact only after the test host starts; there is no compile-time module registry.

The module owns `moderate/v1`, publishes configuration tool metadata, validates the preset and required capabilities, prepares configuration, activates a revision, and serves independent effective-state readback. The host owns destination/actor checks, scoped SQLite configuration and receipt transactions, compare-and-swap, runtime generations, and test delivery/readback. Configuration is committed before activation; a module crash before acknowledgement therefore leaves a durable desired revision and an explicitly unknown effective revision.

Five end-to-end tests cover successful setup and idempotent same-plan retry; module crash before acknowledgement followed by process/host restart and receipt recovery; stale process handles/plans and concurrent human configuration changes; unknown presets, missing privileged intent and unsafe destinations; and failed sends, missing delivery readback and unhealthy subscriptions. The independent assertions check every advertised preset setting, including the 14-day metadata retention and 15-minute membership-summary settings.

See [P7_REPORT.md](P7_REPORT.md) for acceptance mapping and limits.
