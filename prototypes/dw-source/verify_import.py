"""Qualify the saved API snapshot as a bounded offline source import."""
import argparse
import collections
import hashlib
import json
import re
from pathlib import Path

from source_probe import MAX_SOURCE_BYTES, SourceError, load_sources


def checked_path(root, relative):
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()) or not path.is_file():
        raise SourceError("missing or escaping import path")
    return path


def verify(root, fixtures):
    manifest = json.loads((root / "manifest.json").read_text())
    catalog = json.loads((root / "catalog.json").read_text())
    if len(catalog) > 10000:
        raise SourceError("page count limit")
    discovered = {}
    for namespace in manifest["namespaces"].values():
        for row in json.loads((root / f"index-{namespace}.json").read_text()):
            if row["pageid"] in discovered:
                raise SourceError("duplicate discovery identity")
            discovered[row["pageid"]] = row
    if len(catalog) != manifest["pages"] or len(catalog) != len(discovered):
        raise SourceError("incomplete import")
    seen, titles, counts = set(), {}, collections.Counter()
    byte_sizes, raw_total, redirected, quality = [], 0, 0, collections.Counter()
    for row in catalog:
        pageid = row["pageid"]
        if pageid in seen or pageid not in discovered:
            raise SourceError("duplicate or unexpected page")
        seen.add(pageid)
        path = checked_path(root, row["path"])
        size = path.stat().st_size
        if size > MAX_SOURCE_BYTES:
            raise SourceError("page size limit")
        raw = path.read_text()
        record = json.loads(checked_path(root, row["record_path"]).read_text())
        revision = record["revisions"][0]
        if raw != revision["slots"]["main"]["*"]:
            raise SourceError("raw/record mismatch")
        if hashlib.sha256(raw.encode()).hexdigest() != row["content_sha256"] or row["content_sha256"] != record["content_sha256"]:
            raise SourceError("source hash mismatch")
        if row["title"] != discovered[pageid]["title"] or record["title"] != row["title"]:
            raise SourceError("page title mismatch")
        if row["revision_id"] != revision["revid"] or row["namespace"] != record["ns"]:
            raise SourceError("revision or namespace mismatch")
        if not row["retrieved_at"] or not row["revision_timestamp"] or not row["source_url"]:
            raise SourceError("missing source metadata")
        titles[row["title"]] = (row, raw)
        counts[row["kind"]] += 1
        raw_total += size
        byte_sizes.append((size, row["title"]))
        if row["namespace"] == 0:
            redirected += bool(re.match(r"\s*#redirect\b", raw, re.I))
            # Inventory only, not a semantic statement about the article.
            for flag in ["Unreleased", "MissingInformation", "Stub", "Limited"]:
                quality[flag] += bool(re.search(r"\{\{\s*" + flag + r"\s*[|}]", raw, re.I))
    if dict(counts) != manifest["counts"]:
        raise SourceError("namespace coverage mismatch")
    if raw_total > 128 * 1024 * 1024:
        raise SourceError("corpus size limit")
    for title, fixture in fixtures.items():
        row, raw = titles[title]
        selection = fixture["selection"]
        if fixture["revision_id"] != row["revision_id"] or fixture["full_source_sha256"] != row["content_sha256"]:
            raise SourceError("fixture revision does not match import")
        if raw[selection["start"]:selection["end"]] != fixture["wikitext"]:
            raise SourceError("fixture is not the claimed source excerpt")
    return {"status": "passed", "pages": len(catalog), "counts": dict(counts),
            "raw_bytes": raw_total, "largest_pages": sorted(byte_sizes, reverse=True)[:5],
            "main_namespace_redirects": redirected, "quality_marker_inventory": dict(quality),
            "verified_fixture_excerpts": len(fixtures), "source_manifest_sha256": hashlib.sha256((root / "manifest.json").read_bytes()).hexdigest(),
            "source_license": json.loads((root / "siteinfo.json").read_text())["query"]["rightsinfo"],
            "limits": {"max_page_bytes": MAX_SOURCE_BYTES, "max_corpus_bytes": 128 * 1024 * 1024, "max_pages": 10000},
            "limitations": "Coverage is against stored exhausted discovery indexes. This does not re-query live wiki changes or qualify automated access permission."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = verify(args.corpus, load_sources(Path(__file__).parent / "fixtures/wiki.json"))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
