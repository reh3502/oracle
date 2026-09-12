"""A green live exit cannot substitute for required readback and cleanup."""
import importlib.util
import pathlib
import unittest

SPEC = importlib.util.spec_from_file_location("live", pathlib.Path(__file__).resolve().parents[1] / "check-discord-live.py")
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


class LiveTests(unittest.TestCase):
    def test_every_required_live_check_must_pass(self):
        report = {"schema": 1, "purpose": "stage5_structure_live", "passed": True, **dict.fromkeys(live.CHECKS, True)}
        live.validate_report(report)
        for check in live.CHECKS:
            with self.subTest(check=check), self.assertRaises(ValueError):
                live.validate_report({**report, check: False})

    def test_only_explicit_canary_credentials_are_used(self):
        self.assertEqual(live.credentials({"DISCORD_TOKEN": "private", "GUILD_ID": "100", "GEMINI_API_KEY": "ignored"}, None),
                         {"DISCORD_TOKEN": "private", "GUILD_ID": "100"})
        with self.assertRaises(ValueError):
            live.credentials({"DISCORD_TOKEN": "private", "GUILD_ID": "not-a-guild"}, None)


if __name__ == "__main__":
    unittest.main()
