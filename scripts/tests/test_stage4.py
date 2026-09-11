"""Fail-closed environment and executed-test checks for the offline qualifier."""
import importlib.util
import pathlib
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("stage4", pathlib.Path(__file__).resolve().parents[1] / "check-stage4.py")
stage4 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage4)


class Stage4Tests(unittest.TestCase):
    def test_ambient_credentials_and_live_targets_are_removed(self):
        source = {"PATH": "/bin", "HOME": "/home/test", "RUST_LOG": "info",
                  "ORACLE_TEST_AI_POSTGRES_URL": "live", "PGHOST": "live", "GEMINI_API_KEY": "secret",
                  "DISCORD_TOKEN": "secret", "GOOGLE_APPLICATION_CREDENTIALS": "file", "MY_CONNECTION_STRING": "live",
                  "CARGO_NET_OFFLINE": "false", "CARGO_TARGET_DIR": "/other", "LD_PRELOAD": "code.so"}
        env = stage4.isolated_environment(source)
        self.assertEqual(env, {"PATH": "/bin", "HOME": "/home/test", "RUST_LOG": "info",
                               "CARGO_NET_OFFLINE": "true", "CARGO_TARGET_DIR": str(stage4.ROOT / "target")})
        self.assertEqual(source["GEMINI_API_KEY"], "secret")

    def verify_log(self, log):
        with tempfile.TemporaryDirectory() as folder:
            gate = stage4.Qualification(pathlib.Path(folder), {})

            def command(name, argv, extra, minimum):
                self.assertEqual(minimum, 1)
                self.assertIn("--exact", argv)
                path = pathlib.Path(folder) / "test.log"
                path.write_text(log)
                gate.report["checks"].append({"status": "passed", "log": str(path)})

            gate.command = command
            gate.test("contract", "oracle-ai", "expected::contract")

    def test_exact_executed_test_is_observed(self):
        self.verify_log("test expected::contract ... ok\ntest result: ok. 1 passed; 0 failed\n")

    def test_renamed_ignored_or_missing_test_is_not_evidence(self):
        for log in ("test other::contract ... ok\n", "test expected::contract ... ignored\n",
                    "test result: ok. 0 passed; 0 failed; 0 ignored\n"):
            with self.subTest(log=log), self.assertRaises(RuntimeError):
                self.verify_log(log)


if __name__ == "__main__":
    unittest.main()
