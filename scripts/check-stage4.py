#!/usr/bin/env python3
"""Qualify Stage 4 offline contracts with frozen-source evidence and disposable databases.

This does not run or claim live Gemini quality, paid evaluations, or Discord parity.
Reports and logs stay in ignored target/stage4-checks. PostgreSQL 18 is opt-in.
"""
import argparse
import datetime
import importlib.util
import os
import pathlib
import pwd
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("stage3_gate", ROOT / "scripts/check-stage3.py")
stage3 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage3)
COORDINATOR_TESTS = [
    "model_completion_is_not_receipt_backed_success",
    "pending_verification_gets_one_fresh_semantic_continuation",
    "premature_completion_cannot_loop_or_exceed_budget_for_verification",
    "full_round_receipts_survive_compaction_without_policy_promotion",
    "duplicate_call_identity_never_replays_effect",
    "validates_entire_batch_before_first_effect",
    "registry_change_fences_old_tool_before_dispatch",
    "unknown_effect_requires_reconciliation_and_is_not_replayed_on_resume",
    "cancellation_interrupts_inflight_effect_and_fences_resume",
    "interrupted_daily_admission_settles_original_attempt_without_resend",
    "expired_run_stops_before_provider_admission",
    "retries_consume_separate_reservations_and_stop_after_two_retries",
    "daily_cap_rejects_network_dispatch",
    "overlarge_tool_batch_rejects_every_effect",
    "final_effect_is_verified_even_without_budget_for_another_model_turn",
    "operational_failure_is_a_durable_paused_diagnostic",
    "host_receipt_resolves_unknown_call_without_redispatch",
]
HOST_TESTS = [
    "agent_minecraft_receipts_and_repeat_request_reuse_real_operations",
    "agent_forged_scope_and_approval_prose_cannot_create_effects",
    "agent_host_rejects_reference_copied_from_another_run",
    "agent_permission_expansion_waits_for_exact_authenticated_approval",
    "agent_receipt_gate_detects_drift_after_completed_operation",
    "agent_permission_revoked_after_first_write_preserves_only_completed_effect",
    "agent_conflicting_parallel_applies_serialize_and_preserve_receipts",
    "agent_creation_preserves_large_inventory_without_reusing_ids",
]
RECOVERY_TESTS = [
    "host_startup_pauses_interrupted_discord_runs_and_settles_cancelled_spend_without_ai",
    "agent_restart_resolves_only_owned_apply_with_fresh_complete_receipt_without_replay",
]
LOGGING_TESTS = [
    "agent_logging_moderate_verifies_native_configuration_delivery_and_current_health",
    "agent_logging_public_destination_is_denied_without_configuration_or_delivery",
    "agent_logging_missing_inactive_and_unloaded_catalogs_never_authorize_stale_apply",
    "agent_malicious_module_guide_cannot_read_or_exfiltrate_host_secret",
]


def isolated_environment(source):
    env = dict(source)
    for key in list(env):
        if key.startswith(("ORACLE_", "PG", "DISCORD_", "GEMINI_", "GOOGLE_")) or any(
            part in key.upper() for part in (
                "TOKEN", "PASSWORD", "KEY", "SECRET", "CREDENTIAL", "DATABASE_URL", "CONNECTION_STRING"
            )
        ):
            env.pop(key, None)
    env.pop("CARGO_BUILD_TARGET", None)
    env.pop("LD_PRELOAD", None)
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    env["CARGO_NET_OFFLINE"] = "true"
    return env


class Qualification(stage3.Qualification):
    def __init__(self, folder, env):
        super().__init__(folder, env)
        self.report.update(
            scope="offline Stage 4 host/provider contracts; no live Discord or provider calls",
            live_model_qualification="not_run",
            discord_permission_parity="not_run",
        )
        self.save()

    def test(self, name, package, test, extra=None, integration=None, ignored=False):
        argv = ["cargo", "test", "--locked", "-p", package]
        argv += ["--test", integration] if integration else (["--bin", "oracle"] if package == "oracle" else ["--lib"])
        argv += [test, "--", "--exact", "--nocapture", "--test-threads=1"]
        if ignored:
            argv += ["--ignored"]
        self.command(name, argv, extra, minimum=1)
        # Counts alone must not hide renamed filters or a test that only compiled.
        log = pathlib.Path(self.report["checks"][-1]["log"]).read_text(errors="replace")
        if not re.search(r"^test " + re.escape(test) + r" \.\.\. (?:.*\n)*?ok$", log, re.MULTILINE):
            self.report["checks"][-1].update(status="failed", reason="ExpectedTestNotObserved")
            self.save()
            raise RuntimeError("expected exact test was not observed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--postgres-bin", type=pathlib.Path)
    parser.add_argument("--postgres-share", type=pathlib.Path)
    parser.add_argument("--postgres-lib", type=pathlib.Path)
    parser.add_argument("--require-postgres", action="store_true")
    args = parser.parse_args()
    if (args.postgres_share or args.postgres_lib or args.require_postgres) and not args.postgres_bin:
        parser.error("--postgres-bin is required for PostgreSQL options")
    parent = ROOT / "target/stage4-checks"
    parent.mkdir(parents=True, exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    env = isolated_environment(os.environ)
    if args.postgres_lib:
        env["LD_LIBRARY_PATH"] = str(args.postgres_lib.resolve()) + (
            ":" + env["LD_LIBRARY_PATH"] if env.get("LD_LIBRARY_PATH") else ""
        )
    q = Qualification(folder, env)
    q.report["postgres_requested"] = bool(args.postgres_bin)
    cluster = folder / "pg"
    pg = args.postgres_bin.resolve() if args.postgres_bin else None
    start_attempted = False
    error = None
    try:
        q.command("rust-version", ["rustc", "--version"])
        q.command("prepare-serenity", [sys.executable, ROOT / "scripts/prepare-serenity.py"])
        fixtures = {
            "ORACLE_ACTIVITY_LOG": q.fixture("activity-log", "oracle-example-activity-log", "", "ORACLE_ACTIVITY_LOG"),
            "ORACLE_ACTIVITY_LOG_INJECTION": q.fixture("activity-log-injection", "oracle-example-activity-log", "injection-fixture", "ORACLE_ACTIVITY_LOG_INJECTION"),
        }
        q.command("architecture", [sys.executable, ROOT / "scripts/check-architecture.py"])
        q.command("gate-tests", [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests"])
        q.command("workspace-tests", ["cargo", "test", "--locked", "--workspace"], minimum=1)
        q.command("provider-and-agent-contracts", ["cargo", "test", "--locked", "-p", "oracle-ai", "--lib"], minimum=1)
        q.test("sqlite-agent-durability", "oracle-ai", "sqlite_durable_admission_scope_and_recovery", integration="durable_contract")
        q.test("sqlite-migration-restore", "oracle-storage", "tests::sqlite_contract")
        for test in COORDINATOR_TESTS:
            q.test("sqlite-" + test, "oracle-ai", "coordinator::tests::" + test)
        for test in HOST_TESTS:
            q.test("sqlite-" + test, "oracle", "ai::tests::" + test)
        for test in RECOVERY_TESTS:
            q.test("sqlite-" + test, "oracle", "ai::recovery_tests::" + test)
        q.test("catalog-projection-fails-closed", "oracle", "ai::recovery_tests::catalog_unprojectable_or_colliding_module_tools_cannot_remove_core_operations")
        for test in LOGGING_TESTS:
            q.test("sqlite-" + test, "oracle", "ai::logging_tests::" + test, fixtures, ignored=True)
        if pg:
            for name in ["initdb", "pg_ctl", "postgres", "createdb", "pg_dump", "pg_restore"]:
                if not (pg / name).is_file():
                    raise RuntimeError("missing PostgreSQL tool " + name)
            q.command("postgres-version", [pg / "postgres", "--version"])
            version = pathlib.Path(q.report["checks"][-1]["log"]).read_text()
            if not re.search(r"\b18(?:\.|\b)", version):
                raise RuntimeError("qualification requires PostgreSQL 18")
            init = [pg / "initdb", "-D", cluster, "--locale=C", "--encoding=UTF8", "--auth=trust"]
            if args.postgres_share:
                init += ["-L", args.postgres_share.resolve()]
            q.command("initdb", init)
            socket = folder / "socket"
            socket.mkdir(mode=0o700)
            start_attempted = True
            q.command("postgres-start", [pg / "pg_ctl", "-D", cluster, "-l", folder / "postgres.log", "-o", f"-k {socket} -h '' -p 55444", "-w", "start"], timeout=60)
            user = pwd.getpwuid(os.getuid()).pw_name
            urls = {}
            for database in ["agent", "migration", "restore"]:
                q.command("create-" + database, [pg / "createdb", "-h", socket, "-p", "55444", database], timeout=60)
                urls[database] = f"postgresql://{user}@localhost/{database}?host={socket}&port=55444"
            q.test("postgres-agent-durability", "oracle-ai", "postgres_durable_admission_scope_and_recovery", {"ORACLE_TEST_AI_POSTGRES_URL": urls["agent"]}, integration="durable_contract", ignored=True)
            q.test("postgres-migration-restore", "oracle-storage", "tests::postgres_contract", {
                "ORACLE_TEST_POSTGRES_URL": urls["migration"],
                "ORACLE_TEST_POSTGRES_RESTORE_URL": urls["restore"],
                "ORACLE_TEST_PG_BIN": str(pg),
            })
            # Every durable case gets a fresh database. Never run tests concurrently
            # against the same guild fixture or silently substitute SQLite.
            cases = [("oracle-ai", "coordinator::tests::" + test, False) for test in COORDINATOR_TESTS]
            cases += [("oracle", "ai::tests::" + test, False) for test in HOST_TESTS]
            cases += [("oracle", "ai::recovery_tests::" + test, False) for test in RECOVERY_TESTS]
            cases += [("oracle", "ai::logging_tests::" + test, True) for test in LOGGING_TESTS]
            for index, (package, test, ignored) in enumerate(cases):
                database = f"scenario_{index}"
                q.command("create-" + database, [pg / "createdb", "-h", socket, "-p", "55444", database], timeout=60)
                extra = {**fixtures, "ORACLE_TEST_AI_POSTGRES_URL": f"postgresql://{user}@localhost/{database}?host={socket}&port=55444"}
                q.test("postgres-" + test.split("::")[-1], package, test, extra, ignored=ignored)
        q.command("rust-1.95", ["cargo", "+1.95.0", "check", "--locked", "--workspace", "--all-targets", "--all-features"])
        q.command("clippy", ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"])
        q.command("format", ["cargo", "fmt", "--all", "--", "--check"])
    except (OSError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as caught:
        error = type(caught).__name__
    finally:
        if start_attempted:
            try:
                q.command("postgres-stop", [pg / "pg_ctl", "-D", cluster, "-m", "fast", "-w", "stop"], timeout=60)
                if (cluster / "postmaster.pid").exists():
                    raise RuntimeError("PostgreSQL did not stop")
                q.report["postgres_stopped"] = True
            except (OSError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt):
                error = "PostgreSQLCleanupFailed"
                q.report["postgres_stopped"] = False
        after = stage3.snapshot()
        q.report.update(
            source_changed=after != q.report["source_sha256"],
            source_sha256_after=after,
            error=error,
            finished_at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        )
        q.report["passed"] = error is None and not q.report["source_changed"] and all(c["status"] == "passed" for c in q.report["checks"])
        q.save()
        print("Report:", folder / "report.json", flush=True)
    return 0 if q.report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
