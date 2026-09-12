"""Revision-pinned offline DW parser experiment; not a general MediaWiki engine.

No wiki template, Lua, HTML, or source instructions are executed. Unimplemented
templates stay explicit. Numeric adapters require the reviewed template bytes.
"""
import argparse
import copy
import hashlib
import html
import json
import platform
import re
import resource
import statistics
import time
from pathlib import Path

import mwparserfromhell as mw
from mwparserfromhell import nodes

MAX_SOURCE_BYTES = 4 * 1024 * 1024
MAX_CATALOG_BYTES = 16 * 1024 * 1024
MAX_DEPTH = 30
STAT_HASH = "8629aabe5ba771b1ccdc644f44934e24e219054847ef4f1e1e069f71859ff8f4"
ABILITY_HASH = "da6e0123746d45520b324c6d243c26668f7cd5b0b8f90556ff81020d53d47757"
AI_HASH = "5cd7b41b0a74c18c4da93ba7002d8b2af0f09f3a0a4e6201ecdf56be8a73d4c0"


class SourceError(ValueError):
    pass


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def load_sources(path):
    if path.stat().st_size > MAX_CATALOG_BYTES:
        raise SourceError("fixture limit")
    sources = {}
    for row in json.loads(path.read_text()):
        if row["title"] in sources or digest(row["wikitext"]) != row["excerpt_sha256"]:
            raise SourceError("duplicate title or corrupt excerpt")
        if len(row["wikitext"].encode()) > MAX_SOURCE_BYTES:
            raise SourceError("source limit")
        if not row["revision_id"] or not row["source_url"] or not row["retrieved_at"]:
            raise SourceError("missing provenance")
        sources[row["title"]] = row
    return sources


def evidence(row, locator, raw, references=None):
    return {
        key: row[key] for key in (
            "title", "pageid", "revision_id", "revision_timestamp", "retrieved_at",
            "source_url", "revision_url", "license", "license_url", "full_source_sha256",
        )
    } | {"locator": locator, "excerpt_selection": row["selection"], "raw": raw,
         "references": references or []}


class Renderer:
    def __init__(self, page, sources):
        self.page = page
        self.sources = sources
        self.unresolved = set()
        self.dependencies = set()
        self.references = []

    def dependency(self, title, expected=None):
        row = self.sources.get(title)
        if row is None or (expected is not None and digest(row["wikitext"]) != expected):
            return None
        self.dependencies.add(title)
        return row

    def render(self, value, depth=0):
        if depth > MAX_DEPTH:
            raise SourceError("template nesting limit")
        out = []
        for node in mw.parse(str(value)).nodes:
            if isinstance(node, nodes.Text):
                out.append(str(node))
            elif isinstance(node, nodes.HTMLEntity):
                out.append(html.unescape(str(node)))
            elif isinstance(node, nodes.Wikilink):
                title = str(node.title)
                if not title.lower().startswith(("file:", "category:")):
                    out.append(self.render(node.text if node.text is not None else node.title, depth + 1))
            elif isinstance(node, nodes.Heading):
                out.append("\n" + self.render(node.title, depth + 1) + "\n")
            elif isinstance(node, nodes.ExternalLink):
                # Retain source reference separately; never fetch a source-provided URL.
                self.references.append({"url": str(node.url)})
                if node.title is not None:
                    out.append(self.render(node.title, depth + 1))
            elif isinstance(node, nodes.Tag):
                tag = str(node.tag).lower()
                if tag == "ref":
                    self.references.append({"name": str(node.get("name").value) if node.has("name") else None,
                                            "wikitext": str(node.contents)})
                elif tag in {"br", "p", "div", "li"}:
                    out.append(" " + self.render(node.contents, depth + 1) + " ")
                elif tag in {"small", "span", "b", "i", "u", "strong", "em"}:
                    out.append(self.render(node.contents, depth + 1))
                elif tag in {"gallery", "script", "style", "iframe"}:
                    self.unresolved.add("tag:" + tag)
                else:
                    self.unresolved.add("tag:" + tag)
                    out.append("[unresolved markup]")
            elif isinstance(node, nodes.Template):
                name = str(node.name).strip()
                def arg(key, default=""):
                    return str(node.get(key).value) if node.has(key) else default
                if name in {"Small", "Center"}:
                    out.append(self.render(arg(1), depth + 1))
                elif name in {"CI", "TI", "II", "CDI", "Type"}:
                    # An explicit source entity label, not generic expansion of icon templates.
                    out.append(self.render(arg(1), depth + 1))
                elif name == "Tape":
                    out.append("Tapes")
                elif name == "Heart":
                    out.append("Heart")
                elif name == "PAGENAME":
                    out.append(self.page["title"])
                elif name in {"Stub", "MissingInformation", "Unreleased", "Limited"}:
                    # Quality flags are separately retained on the page.
                    continue
                elif name == "Debuff":
                    out.append(self.render(arg(1) + " " + arg(2), depth + 1))
                elif name == "AI":
                    alias = self.dependency("Template:AI", AI_HASH)
                    source = self.dependency("Template:AbilityIcon", ABILITY_HASH)
                    if alias and source:
                        switch = next(t for t in mw.parse(source["wikitext"]).filter_templates()
                                      if str(t.name).strip().startswith("#switch:"))
                        if switch.has(arg(1)):
                            out.append(self.render(switch.get(arg(1)).value, depth + 1))
                        else:
                            self.unresolved.add(name)
                    else:
                        self.unresolved.add(name)
                elif name in {"FloorTypeAmount", "FloorAmount"}:
                    source = self.dependency("Template:" + name)
                    if source and re.fullmatch(r"\s*\d+\s*", source["wikitext"]):
                        out.append(source["wikitext"].strip())
                    else:
                        self.unresolved.add(name)
                else:
                    self.unresolved.add(name)
                    # Preserve numeric parameters as source data in evidence, not guessed text.
                    if "Template:" + name in self.sources:
                        self.dependencies.add("Template:" + name)
                    out.append("[unresolved template: " + name + "]")
            elif not isinstance(node, nodes.Comment):
                self.unresolved.add(type(node).__name__)
        return "".join(out)

    def fact(self, raw, locator):
        text = re.sub(r"\s+", " ", self.render(raw)).strip()
        refs = [evidence(self.page, locator, raw, self.references)]
        for title in sorted(self.dependencies):
            row = self.sources[title]
            refs.append(evidence(row, "template source", row["wikitext"]))
        return {"text": text, "status": "unresolved" if self.unresolved else ("supported" if text else "unknown"),
                "unresolved_templates": sorted(self.unresolved), "evidence": refs}


def stat_fact(raw, row, sources, field):
    templates = mw.parse(raw).filter_templates(recursive=False)
    if len(templates) != 1 or str(templates[0]).strip() != raw.strip():
        return None
    template = templates[0]
    expected_kind = {"skill_check": "Skill", "movement_speed": "Move", "stamina": "Stam",
                     "stealth": "Stealth", "extraction_speed": "Extract"}[field]
    source = sources.get("Template:StatComp")
    if str(template.name).strip() != "StatComp" or source is None or digest(source["wikitext"]) != STAT_HASH:
        return None
    if len(template.params) != 2 or str(template.get(1).value).strip() != expected_kind:
        return None
    stars_text = str(template.get(2).value).strip()
    if stars_text not in {"1", "2", "3", "4", "5"}:
        return None
    stars = int(stars_text)
    value = {"stars": stars}
    # Reviewed formulas in Template:StatComp. Never eval wiki-provided expressions.
    if expected_kind == "Move":
        value.update(walk=stars * 2.5 + 7.5, sprint=stars * 2.5 + 17.5)
    elif expected_kind == "Stam":
        value["capacity"] = (stars + 3) * 25
    elif expected_kind == "Stealth":
        value["priority"] = (stars - 1) * 5
    elif expected_kind == "Extract":
        value["rate"] = (stars**4 - 10 * stars**3 + 47 * stars**2 - 38 * stars + 360) / 480
    return {"value": value, "status": "supported", "conditions": "base StatComp display; no ability/item modifiers applied",
            "evidence": [evidence(row, "infobox." + field, raw), evidence(source, "template source", source["wikitext"])]}


def parse_page(row, sources):
    source = row["wikitext"]
    if len(source.encode()) > MAX_SOURCE_BYTES:
        raise SourceError("source limit")
    parsed = mw.parse(source)
    names = {str(t.name).strip().lower() for t in parsed.filter_templates()}
    warnings = sorted(names & {"stub", "missinginformation", "limited", "unreleased"})
    facts = {}
    if row["selection"]["mode"] == "infobox":
        boxes = [t for t in parsed.filter_templates(recursive=False)
                 if str(t.name).strip() in {"Toons", "Twisted", "Trinket"}]
        if len(boxes) != 1:
            raise SourceError("expected one supported infobox")
        seen = set()
        for param in boxes[0].params:
            key, raw = str(param.name).strip(), str(param.value).strip()
            if key in seen:
                raise SourceError("duplicate infobox field")
            seen.add(key)
            allowed = {"title1", "skill_check", "movement_speed", "stamina", "stealth", "extraction_speed",
                       "ability_1", "ability_2", "type", "speed", "mechanic", "attention_span", "detection_range", "effect"}
            if key not in allowed and not key.startswith("requirement_"):
                continue
            result = stat_fact(raw, row, sources, key) if key in {
                "skill_check", "movement_speed", "stamina", "stealth", "extraction_speed"} else None
            facts[key] = result or Renderer(row, sources).fact(raw, "infobox." + key)
    elif row["selection"]["mode"] == "table":
        # This qualification fixture pins one unmerged row, with each cell on a line.
        # Prices are deliberately not labelled without the multi-row headers.
        cells = source.split("\n|")
        if len(cells) != 8 or "Bandage" not in cells[0]:
            raise SourceError("item table layout changed")
        facts["effect"] = Renderer(row, sources).fact(cells[1], "List of Items / Bandage / Effect")
    elif "unreleased" not in names:
        facts["section"] = Renderer(row, sources).fact(source, row["selection"]["selector"] or "lead")
    return {"title": row["title"], "kind": row["kind"], "facts": facts, "warnings": warnings,
            "status": "unreleased" if "unreleased" in names else "source_excerpt"}


def build(sources):
    return [parse_page(row, sources) for row in sources.values() if row["kind"] != "dependency"]


class Catalog:
    def __init__(self, pages):
        self._pages = copy.deepcopy(pages)
        self._index = {(p["title"].casefold(), p["kind"]): p for p in self._pages}
        if len(self._index) != len(self._pages):
            raise SourceError("duplicate catalog identity")

    @staticmethod
    def encode(pages):
        payload = {"schema": 1, "pages": pages}
        result = canonical(payload | {"sha256": hashlib.sha256(canonical(payload)).hexdigest()})
        if len(result) > MAX_CATALOG_BYTES:
            raise SourceError("catalog limit")
        return result

    @classmethod
    def open(cls, path):
        if path.stat().st_size > MAX_CATALOG_BYTES:
            raise SourceError("catalog limit")
        payload = json.loads(path.read_bytes())
        checksum = payload.pop("sha256")
        if payload.get("schema") != 1 or hashlib.sha256(canonical(payload)).hexdigest() != checksum:
            raise SourceError("unsupported schema or corrupt catalog")
        return cls(payload["pages"])

    def lookup(self, title, kind):
        return copy.deepcopy(self._index.get((title.strip().casefold(), kind)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, default=Path(__file__).parent / "fixtures/wiki.json")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    sources = load_sources(args.fixtures)
    start = time.perf_counter()
    pages = build(sources)
    elapsed = time.perf_counter() - start
    args.output.mkdir(parents=True, exist_ok=True)
    encoded = Catalog.encode(pages)
    catalog_path = args.output / "catalog.json"
    catalog_path.write_bytes(encoded)
    load_start = time.perf_counter()
    catalog = Catalog.open(catalog_path)
    load_ms = (time.perf_counter() - load_start) * 1000
    timings = []
    for i in range(5000):
        page = pages[i % len(pages)]
        before = time.perf_counter_ns()
        assert catalog.lookup(page["title"], page["kind"]) is not None
        timings.append((time.perf_counter_ns() - before) / 1e6)
    report = {"status": "passed", "pages": len(pages), "source_records": len(sources),
              "facts": sum(len(p["facts"]) for p in pages), "catalog_bytes": len(encoded),
              "build_ms": elapsed * 1000, "load_ms": load_ms,
              "lookup_iterations": len(timings), "lookup_p50_ms": statistics.median(timings),
              "lookup_p95_ms": sorted(timings)[int(len(timings) * .95)],
              "process_peak_rss_kib": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
              "python": platform.python_version(), "platform": platform.platform(),
              "parser_version": mw.__version__, "fixture_sha256": hashlib.sha256(args.fixtures.read_bytes()).hexdigest(),
              "limitations": "Single-process offline sample, no concurrency, full-corpus parser, network, Discord, or production runtime qualification."}
    (args.output / "measurement.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
