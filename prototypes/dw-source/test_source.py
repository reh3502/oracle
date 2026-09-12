"""Stage 0 contracts derived from the attributed revision-pinned source excerpts."""
import copy
import json
import tempfile
import unittest
from pathlib import Path

from source_probe import Catalog, SourceError, build, load_sources, parse_page
from verify_import import checked_path


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.sources = load_sources(Path(__file__).parent / "fixtures/wiki.json")
        self.pages = {p["title"]: p for p in build(self.sources)}

    def test_character_stats_and_nested_ability_keep_provenance(self):
        pebble = self.pages["Pebble"]
        stats = pebble["facts"]["movement_speed"]
        self.assertEqual(stats["value"], {"stars": 5, "walk": 20.0, "sprint": 30.0})
        self.assertEqual(pebble["facts"]["stamina"]["value"], {"stars": 4, "capacity": 175})
        self.assertEqual(pebble["facts"]["extraction_speed"]["value"], {"stars": 1, "rate": 0.75})
        self.assertEqual(stats["evidence"][0]["revision_id"], 261239)
        self.assertEqual(stats["evidence"][1]["title"], "Template:StatComp")
        ability = pebble["facts"]["ability_1"]
        self.assertIn("Speak!", ability["text"])
        self.assertIn("-40", ability["text"])
        self.assertIn("cooldown of 60", ability["text"])
        self.assertIn("Template:AbilityIcon", [e["title"] for e in ability["evidence"]])
        # Infobox heart slots are display markup, not a supported health calculation.
        self.assertNotIn("health", pebble["facts"])

    def test_unlocks_stay_separate_complete_requirements(self):
        facts = self.pages["Pebble"]["facts"]
        self.assertEqual(facts["requirement_1"]["text"], "3750 Ichor")
        self.assertEqual(facts["requirement_2"]["text"], "100% Research on Twisted Pebble")
        self.assertIn("all Mastery Quests on Toodles", facts["requirement_3"]["text"])

    def test_twisted_speed_is_explicitly_unexpanded_not_a_guessed_formula(self):
        page = self.pages["Twisted Pebble"]
        self.assertEqual(page["kind"], "twisted")
        speed = page["facts"]["speed"]
        self.assertEqual(speed["status"], "unresolved")
        self.assertIn("TSpeed", speed["unresolved_templates"])
        self.assertEqual(speed["evidence"][0]["references"][0]["name"], "stats")
        self.assertIn("circular vision", page["facts"]["detection_range"]["text"])
        self.assertIn("hearing radius", page["facts"]["detection_range"]["text"])

    def test_floor_and_mechanic_conditions_survive(self):
        floor = self.pages["Dyle's Floor"]["facts"]["section"]
        self.assertIn("Floor 9+", floor["text"])
        self.assertIn("only available on odd Floors", floor["text"])
        self.assertIn("25 Machines", floor["text"])
        self.assertIn("regardless of what Floor", floor["text"])
        self.assertIn("missinginformation", self.pages["Dyle's Floor"]["warnings"])
        research = self.pages["Research"]["facts"]["section"]["text"]
        self.assertIn("limited to once per floor", research)
        self.assertIn("Rodger gains double", research)
        self.assertIn("not client-sided", research)
        self.assertIn("unless you are on the Dyle's Floor", self.pages["Floors"]["facts"]["section"]["text"])

    def test_trinket_and_item_restrictions_survive(self):
        effect = self.pages["Bone"]["facts"]["effect"]["text"]
        self.assertIn("25%", effect)
        self.assertIn("4 seconds", effect)
        self.assertIn("caps at 40 speed", effect)
        self.assertIn("Does not include Capsules and Tapes", effect)
        item = self.pages["Items"]["facts"]["effect"]
        self.assertIn("restores 1 Heart", item["text"])
        # The table excerpt deliberately omits multi-row price headers.
        self.assertNotIn("price", self.pages["Items"]["facts"])

    def test_unknowns_unreleased_and_missing_data_are_not_current_facts(self):
        page = self.pages["Pebble's Floor"]
        self.assertEqual(page["status"], "unreleased")
        altered = copy.deepcopy(self.sources["Bone"])
        altered["wikitext"] = "{{Trinket|effect={{NewMechanic|99}}|requirement_1=}}"
        page = parse_page(altered, self.sources)
        self.assertEqual(page["facts"]["effect"]["status"], "unresolved")
        self.assertEqual(page["facts"]["requirement_1"]["status"], "unknown")

    def test_changed_dependency_and_duplicate_fields_fail_closed(self):
        sources = copy.deepcopy(self.sources)
        sources["Template:StatComp"]["wikitext"] += "changed"
        page = parse_page(sources["Pebble"], sources)
        self.assertEqual(page["facts"]["movement_speed"]["status"], "unresolved")
        page = copy.deepcopy(sources["Bone"])
        page["wikitext"] = "{{Trinket|effect=one|effect=two}}"
        with self.assertRaises(SourceError):
            parse_page(page, sources)

    def test_markup_instructions_are_data_never_executed(self):
        page = copy.deepcopy(self.sources["Bone"])
        page["wikitext"] = '{{Trinket|effect=<script>steal()</script>{{#invoke:Exploit|run}}<ref>https://invalid.test</ref>}}'
        fact = parse_page(page, self.sources)["facts"]["effect"]
        self.assertEqual(fact["status"], "unresolved")
        self.assertNotIn("steal()", fact["text"])
        self.assertTrue(fact["unresolved_templates"])

    def test_immutable_catalog_checksums_and_exact_kind_lookup(self):
        catalog = Catalog.encode(list(self.pages.values()))
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "catalog.json"
            path.write_bytes(catalog)
            loaded = Catalog.open(path)
            self.assertEqual(loaded.lookup("pebble", "toon")["title"], "Pebble")
            self.assertIsNone(loaded.lookup("pebble", "twisted"))
            self.assertEqual(loaded.lookup("Twisted Pebble", "twisted")["title"], "Twisted Pebble")
            copy_ = loaded.lookup("Pebble", "toon")
            copy_["title"] = "corrupted"
            self.assertEqual(loaded.lookup("Pebble", "toon")["title"], "Pebble")
            decoded = json.loads(catalog)
            decoded["pages"][0]["title"] = "tampered"
            path.write_text(json.dumps(decoded))
            with self.assertRaises(SourceError):
                Catalog.open(path)

    def test_fixture_corruption_is_detected_before_parsing(self):
        rows = list(copy.deepcopy(self.sources).values())
        rows[0]["wikitext"] += " altered"
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fixture.json"
            path.write_text(json.dumps(rows))
            with self.assertRaises(SourceError):
                load_sources(path)

    def test_duplicate_identity_and_unknown_catalog_schema_are_rejected(self):
        page = self.pages["Pebble"]
        with self.assertRaises(SourceError):
            Catalog([page, page])
        encoded = json.loads(Catalog.encode([page]))
        encoded["schema"] = 999
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "catalog.json"
            path.write_text(json.dumps(encoded))
            with self.assertRaises(SourceError):
                Catalog.open(path)

    def test_offline_import_rejects_path_and_symlink_escape(self):
        with tempfile.TemporaryDirectory() as temp:
            parent = Path(temp)
            root = parent / "corpus"
            root.mkdir()
            outside = parent / "outside.txt"
            outside.write_text("unrelated data")
            (root / "escape").symlink_to(outside)
            for name in ["../outside.txt", "escape", "missing.txt"]:
                with self.assertRaises(SourceError):
                    checked_path(root, name)


if __name__ == "__main__":
    unittest.main()
