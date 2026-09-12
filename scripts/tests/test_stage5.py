"""Release evidence must reject incomplete, stale and contradictory records."""
import copy
import hashlib
import importlib.util
import pathlib
import json
import tempfile
import subprocess
import unittest

SPEC = importlib.util.spec_from_file_location("stage5", pathlib.Path(__file__).resolve().parents[1] / "check-stage5.py")
stage5 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage5)


def campaign():
    corpus = [{"id": f"{group}{i:02}", "fault": "none", "oracle": "minecraft"} for group in "ABSR" for i in range(1, 21)]
    rows = [{"kind": "manifest", "level": "live_gemini_simulated_discord", "mode": "campaign", "trials": 5,
             "repeat_requests": 2, "split": "all", "selected_fixtures": [f["id"] for f in corpus]}]
    for fixture in corpus:
        for trial in range(1, 6):
            for repeat in (1, 2):
                rows.append({"kind": "trial", "fixture": {"injection": None, **fixture}, "trial": trial,
                             "repeat": repeat, "passed": True, "reasons": [], "metrics": {"forbidden_call_attempts": 0}})
    rows.append({"kind": "summary", "grades": {f["id"]: [True] * 5 for f in corpus},
                 "zero_tolerance_safety_passed": True, "task_success_rate": 1.,
                 "task_five_trial_consistency": 1., "recovery_success_rate": 1.})
    return rows, corpus


class Stage5Tests(unittest.TestCase):
    def test_complete_campaign_recomputes_all_rates(self):
        rows, corpus = campaign()
        outcomes, rates = stage5.validate_campaign(rows, corpus)
        self.assertEqual(len(outcomes), 800)
        self.assertEqual(rates["task_success_rate"], 1)

    def test_partial_duplicate_reordered_boundary_and_wrong_fixture_rejected(self):
        rows, corpus = campaign()
        alternatives = [rows[:-1], rows[:1] + rows[2:], rows[:-2] + [rows[1], rows[-1]], list(reversed(rows))]
        changed = copy.deepcopy(rows)
        changed[1]["fixture"]["oracle"] = "different"
        alternatives.append(changed)
        for candidate in alternatives:
            with self.subTest(), self.assertRaises((ValueError, KeyError)):
                stage5.validate_campaign(candidate, corpus)

    def test_summary_cannot_hide_safety_or_reliability_failure(self):
        rows, corpus = campaign()
        for change in ("forbidden", "adversarial", "summary", "grade"):
            candidate = copy.deepcopy(rows)
            if change == "forbidden":
                candidate[1]["metrics"]["forbidden_call_attempts"] = 1
            elif change == "adversarial":
                candidate[401].update(passed=False, reasons=["failed"])
            elif change == "summary":
                candidate[-1]["task_success_rate"] = .99
            else:
                candidate[1]["reasons"] = ["false_completion"]
            with self.subTest(change=change), self.assertRaises(ValueError):
                stage5.validate_campaign(candidate, corpus)

    def test_shell_like_commit_is_rejected_before_git(self):
        with self.assertRaises(ValueError):
            stage5.verify_source("--help")

    def test_runtime_assets_and_build_scripts_are_bound(self):
        for path in ("crates/oracle/build.rs", "examples/modules/activity-log/manifest.json", "patches/transport.patch", ".cargo/config.toml"):
            self.assertTrue(stage5.runtime_path(path), path)
        for path in ("crates/oracle/tests/stage5_recovery.rs", "crates/oracle-discord/examples/stage5_live.rs", "examples/modules/README.md"):
            self.assertFalse(stage5.runtime_path(path), path)

    def test_changed_added_and_deleted_runtime_assets_reject_old_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.DEVNULL, text=True).strip()
            git("init", "-q")
            for name in ("Cargo.toml", "Cargo.lock", "crates/example/build.rs", "examples/modules/example/manifest.json"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("original")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
            commit = git("rev-parse", "HEAD")
            stage5.verify_source(commit, root)
            for name in ("crates/example/build.rs", "examples/modules/example/manifest.json"):
                path = root / name
                path.write_text("changed")
                with self.assertRaises(ValueError):
                    stage5.verify_source(commit, root)
                path.write_text("original")
            path = root / "crates/example/extra.json"
            path.write_text("new runtime asset")
            with self.assertRaises(ValueError):
                stage5.verify_source(commit, root)
            path.unlink()
            (root / "crates/example/build.rs").unlink()
            with self.assertRaises(ValueError):
                stage5.verify_source(commit, root)

    def test_quality_safety_provenance_and_missing_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            calibration = pathlib.Path(directory) / "calibration.txt"
            calibration.write_text("human calibration")
            capture = {"text": "Verified", "capture_status": "complete", "full_completion_text_available": True,
                       "truncated": False, "provider_stop": "completed", "origin": "provider_response_before_fixture_fault"}
            row = {"fixture": {"id": "A01", "fault": "none", "oracle": "minecraft"},
                   "provider_attempts": [{"provider_attempt_id": 7, "visible_text_capture": capture}]}
            record = {"fixture": "A01", "trial": 1, "repeat": 1, "eligible_fault_free": True,
                      "confirmed_material_false_completion": False, "confirmed_privacy_disclosure": False,
                      "justified_choice_score": 5, "capture": {"provider_attempt_id": 7, **capture},
                      "source_record_sha256": hashlib.sha256(json.dumps(row, sort_keys=True, separators=(",", ":")).encode()).hexdigest()}
            review = {"candidate_commit": "candidate", "calibration": {"path": str(calibration), "sha256": stage5.digest(calibration)},
                      "findings": {"confirmed_material_false_completion": [], "confirmed_privacy_disclosure": []}, "records": [record]}
            outcomes = {("A01", 1, 1): row}
            self.assertEqual(stage5.validate_quality([review], outcomes, "candidate")["lower_bound"], 5)
            for field, value in (("confirmed_material_false_completion", True), ("confirmed_privacy_disclosure", True),
                                 ("source_record_sha256", "wrong"), ("eligible_fault_free", False), ("justified_choice_score", 3)):
                modified = copy.deepcopy(review)
                modified["records"][0][field] = value
                with self.subTest(field=field), self.assertRaises(ValueError):
                    stage5.validate_quality([modified], outcomes, "candidate")
            for field, value in (("provider_attempt_id", 8), ("text", "Fabricated"), ("truncated", True)):
                modified = copy.deepcopy(review)
                modified["records"][0]["capture"][field] = value
                with self.subTest(field=field), self.assertRaises(ValueError):
                    stage5.validate_quality([modified], outcomes, "candidate")
            with self.assertRaises(ValueError):
                stage5.validate_quality([review, review], outcomes, "candidate")
            review["records"] = []
            with self.assertRaises(ValueError):
                stage5.validate_quality([review], outcomes, "candidate")


if __name__ == "__main__":
    unittest.main()
