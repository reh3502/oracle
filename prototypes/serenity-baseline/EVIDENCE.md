# P3 evidence — 2026-09-08

Verdict: **offline gates and authorized live disposable-guild gate passed**.

- Upstream: `98ec74223b0ff77fc4e8085d25569ea59e09a36f`, package 0.12.5, Rust 1.95.0, exact locked Git source.
- Local fork patch SHA-256: `ee7823d7efbd17d10a9caf0034a3914f513049d2a720545c9660ae1bc3a70a34`.
- `python3 check.py`: exit 0. Exact upstream build and one diagnostic test passed; patched all-target compile, formatting, Clippy with warnings denied, and six adapter fixtures passed.
- The upstream diagnostic confirms two existing failures; it is not a claim those failures are acceptable. The patched tests recover partial updates and deserialize the interaction fixture in both key orders.
- Authorized live run passed: one category, one text channel, one voice channel and one guild command were created and read back, then all four owned IDs were deleted and absence verified. Cleanup errors: none. Independent post-cleanup inspection confirmed the original four visible channels and zero commands.
- `artifacts/live-authorized.json` records UTC, resource IDs, post-cleanup observation and matching binary/source/patch digests. The token was read from the user-provided `.env` only into subprocess environment variables and was not written to reports.

Detailed command logs, environment, resolved sources and source hashes: `artifacts/offline.json`. Reproduce with the commands in [README.md](README.md); generated artifacts are not committed.

The initial fake-token 401 caused by the upstream limiter bypassing its API proxy is recorded in README failure provenance. Subsequent HTTP fixtures have an independent loopback DNS guard, no inherited proxy and no redirects. The HTTP tests do not qualify Serenity's limiter or live permissions.
