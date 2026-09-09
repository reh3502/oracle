#!/usr/bin/env python3
"""Offline Stage 3 qualification with isolated fixtures and optional private PostgreSQL.

Reports, logs and immutable fixture copies remain under ignored target/stage3-checks.
No deployment configuration, Discord token, or provider credential is read.
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CONFIGURATION = [
    "configuration_exact_readback_preserves_settings_and_cas",
    "configuration_policy_schema_expiry_and_generation_reject_stale_plans",
    "configuration_prepare_rejection_and_readback_mismatch_are_not_success",
    "configuration_real_crash_and_host_restart_recover_same_revision",
    "configuration_cancelled_pending_does_not_replace_active_values",
    "configuration_policy_changed_during_prepare_cannot_commit",
]
EVENTS = [
    "event_queue_is_bounded_and_unload_fences_waiting_notification",
    "event_notification_rechecks_configuration_revision_after_readiness",
    "event_notification_rechecks_intents_after_readiness",
    "event_routes_require_configuration_and_reuse_verified_effect_receipts",
    "command_namespace_collision_is_refused_before_activation_and_scoped_to_guild",
]
HOST_TEST = "command_runtime::integration_tests::published_command_identity_and_explicit_grants_survive_lifecycle_changes"


def sha(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def snapshot():
    paths = set(ROOT.glob("Cargo.*"))
    paths.update(ROOT / name for name in ["rust-toolchain", "rust-toolchain.toml", "rustfmt.toml", ".rustfmt.toml"])
    for name in ["crates", "examples", "scripts", "patches", ".cargo"]:
        for folder, dirs, files in os.walk(ROOT / name):
            dirs[:] = sorted(d for d in dirs if d not in {"target", ".local", ".git", "__pycache__"})
            for filename in files:
                path = pathlib.Path(folder) / filename
                if path.suffix in {".rs", ".toml", ".json", ".sql", ".py", ".sh", ".patch", ".md"}:
                    paths.add(path)
    return {str(p.relative_to(ROOT)): sha(p) for p in sorted(paths) if p.is_file() and not p.is_symlink()}


class Qualification:
    def __init__(self, folder, env):
        self.folder, self.env = folder, env
        self.report = {"passed": False, "started_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                       "scope": "offline fixture processes and disposable databases; no live Discord/provider calls",
                       "source_sha256": snapshot(), "checks": [], "fixtures": {}}
        self.save()

    def save(self):
        temporary = self.folder / "report.tmp"
        temporary.write_text(json.dumps(self.report, indent=2) + "\n")
        temporary.replace(self.folder / "report.json")

    def command(self, name, argv, extra=None, minimum=0, timeout=1200):
        log = self.folder / f"{len(self.report['checks']):02d}-{name}.log"
        record = {"name": name, "command": [str(a) for a in argv], "status": "running", "log": str(log)}
        self.report["checks"].append(record)
        env = self.env.copy()
        env.update(extra or {})
        record["environment_keys"] = sorted(extra or {})
        self.save()
        print(f"[{name}] running", flush=True)
        start = time.monotonic()
        try:
            with log.open("wb") as output:
                process = subprocess.Popen(record["command"], cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = process.wait(timeout=timeout)
                except (subprocess.TimeoutExpired, KeyboardInterrupt):
                    # Descendant fixture builds/tests must not outlive a timed-out gate.
                    import signal
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    raise
            record["exit_code"] = code
            totals = re.findall(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed", log.read_text(errors="replace"))
            record["tests_passed"] = sum(int(passed) for passed, _ in totals)
            record["tests_failed"] = sum(int(failed) for _, failed in totals)
            if code or record["tests_failed"] or record["tests_passed"] < minimum:
                raise RuntimeError("nonzero exit or missing expected executed tests")
            record["status"] = "passed"
        except (OSError, subprocess.SubprocessError, RuntimeError, KeyboardInterrupt) as error:
            record.update(status="failed", reason=type(error).__name__)
            raise
        finally:
            record["seconds"] = round(time.monotonic() - start, 3)
            self.save()
            print(f"[{name}] {record['status']}", flush=True)

    def fixture(self, profile, package, features, variable):
        target = ROOT / "target/stage3-build" / profile
        argv = ["cargo", "build", "--locked", "--no-default-features", "-p", package, "--target-dir", target]
        if features:
            argv += ["--features", features]
        self.command("build-" + profile, argv)
        source = target / "debug" / package
        digest = sha(source)
        destination = self.folder / profile
        shutil.copyfile(source, destination)
        if sha(destination) != digest or sha(source) != digest:
            raise RuntimeError("fixture changed while copying")
        destination.chmod(0o555)
        self.report["fixtures"][variable] = {"path": str(destination), "sha256": digest}
        self.save()
        return str(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--postgres-bin", type=pathlib.Path)
    parser.add_argument("--postgres-share", type=pathlib.Path)
    parser.add_argument("--postgres-lib", type=pathlib.Path)
    parser.add_argument("--require-postgres", action="store_true", help="fail unless PostgreSQL qualification is included")
    args = parser.parse_args()
    if (args.postgres_share or args.postgres_lib or args.require_postgres) and not args.postgres_bin:
        parser.error("--postgres-bin is required for PostgreSQL options")
    parent = ROOT / "target/stage3-checks"
    parent.mkdir(parents=True, exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("ORACLE_", "PG", "DISCORD_", "GEMINI_", "GOOGLE_")) or any(part in key.upper() for part in ["TOKEN", "PASSWORD", "KEY", "SECRET", "CREDENTIAL", "DATABASE_URL", "CONNECTION_STRING"]):
            env.pop(key, None)
    env.pop("CARGO_BUILD_TARGET", None)
    env.pop("LD_PRELOAD", None)
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    env["CARGO_NET_OFFLINE"] = "true"
    if args.postgres_lib:
        env["LD_LIBRARY_PATH"] = str(args.postgres_lib.resolve()) + (":" + env["LD_LIBRARY_PATH"] if env.get("LD_LIBRARY_PATH") else "")
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
            "ORACLE_CONFIGURATION_PROBE": q.fixture("configuration", "oracle-fixture-configuration-probe", "", "ORACLE_CONFIGURATION_PROBE"),
            "ORACLE_EVENT_PROBE": q.fixture("events", "oracle-fixture-configuration-probe", "events", "ORACLE_EVENT_PROBE"),
            "ORACLE_COMMAND_COLLISION_PROBE": q.fixture("collision", "oracle-fixture-configuration-probe", "collision", "ORACLE_COMMAND_COLLISION_PROBE"),
            "ORACLE_ACTIVITY_LOG": q.fixture("activity-log", "oracle-example-activity-log", "", "ORACLE_ACTIVITY_LOG"),
        }
        q.command("workspace-tests", ["cargo", "test", "--locked", "--workspace"], minimum=1)
        for suite, minimum in [("configuration", len(CONFIGURATION)), ("events", len(EVENTS)), ("activity_log", 3)]:
            q.command("sqlite-" + suite, ["cargo", "test", "--locked", "-p", "oracle-modules", "--test", suite, "--", "--ignored", "--nocapture", "--test-threads=1"], fixtures, minimum=minimum)
        q.command("command-runtime", ["cargo", "test", "--locked", "-p", "oracle", HOST_TEST, "--", "--ignored", "--exact", "--nocapture"], fixtures, minimum=1)
        if pg:
            for name in ["initdb", "pg_ctl", "postgres", "createdb"]:
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
            q.command("postgres-start", [pg / "pg_ctl", "-D", cluster, "-l", folder / "postgres.log", "-o", f"-k {socket} -h '' -p 55443", "-w", "start"], timeout=60)
            import pwd
            user = pwd.getpwuid(os.getuid()).pw_name
            for suite, tests, variable in [("configuration", CONFIGURATION, "ORACLE_TEST_CONFIGURATION_POSTGRES_URL"), ("events", EVENTS, "ORACLE_TEST_EVENTS_POSTGRES_URL")]:
                for index, test in enumerate(tests):
                    database = f"{suite}_{index}"
                    q.command("create-" + database, [pg / "createdb", "-h", socket, "-p", "55443", database], timeout=60)
                    extra = {**fixtures, variable: f"postgresql://{user}@localhost/{database}?host={socket}&port=55443"}
                    q.command("pg-" + test, ["cargo", "test", "--locked", "-p", "oracle-modules", "--test", suite, test, "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"], extra, minimum=1)
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
                    raise RuntimeError("PostgreSQL still has a live ownership file after stop")
                q.report["postgres_stopped"] = True
            except (OSError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt):
                error = "PostgreSQLCleanupFailed"
                q.report["postgres_stopped"] = False
        after = snapshot()
        q.report["source_changed"] = after != q.report["source_sha256"]
        q.report["source_sha256_after"] = after
        q.report["error"] = error
        q.report["passed"] = error is None and not q.report["source_changed"] and all(c["status"] == "passed" for c in q.report["checks"])
        q.report["finished_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        q.save()
        print("Report:", folder / "report.json", flush=True)
    return 0 if q.report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
