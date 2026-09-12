# Dandy’s World source probe

Offline qualification prototype for attributed wiki text. It parses a small set
of revision-pinned excerpts into a checksummed immutable sample catalog. It is
not the production module or a general MediaWiki implementation.

From the repository root, use Python 3.11+ with a working C compiler:

```sh
python3 -m venv target/dw-stage0-venv
target/dw-stage0-venv/bin/python -m pip install --require-hashes --no-binary=mwparserfromhell -r prototypes/dw-source/requirements.txt
target/dw-stage0-venv/bin/python -m unittest discover -s prototypes/dw-source -v
target/dw-stage0-venv/bin/python prototypes/dw-source/source_probe.py --output target/dw-stage0/source
```

The sample covers a Toon, its Twisted counterpart, a trinket, an item row, named
floor and floor rules, research, machines, a developer-only character, events,
cards, and an unreleased floor. Nested ability labels and base StatComp values
carry supporting template evidence. Conditions stay attached to source text.
Changed formula templates, missing fields and unsupported transclusions remain
unresolved rather than acquiring made-up values. Templates and Lua are never
executed. The selected source text is licensed separately; see
[fixture attribution](fixtures/ATTRIBUTION.md).

The full local source snapshot is intentionally absent from Git. Given a saved
snapshot in the collector's `manifest.json`, `catalog.json`, `index-*.json`, and
per-page JSON/text layout, validate it and the exact fixture selections with:

```sh
target/dw-stage0-venv/bin/python prototypes/dw-source/verify_import.py --corpus /absolute/path/wiki-corpus --output target/dw-stage0/import.json
```

The importer is offline. API reachability and text licensing do not establish
permission for ongoing automated collection; confirm that separately before
adding an online refresh service.

The measurement output records build/load/lookup timings, catalog size, parser
version, fixture checksum, platform, and whole-process peak RSS. This is a small
single-threaded experiment, not a production latency or concurrency guarantee.
There is no search engine, automatic fact conflict resolver, complete table
parser, health-icon conversion, or generic numeric-expression evaluator here.
