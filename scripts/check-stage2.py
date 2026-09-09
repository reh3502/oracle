#!/usr/bin/env python3
"""Qualify Stage 2 with real fixtures, disposable PostgreSQL 18, and complete check evidence.

No existing database or credential file is read. Failed and blocked checks remain
in the report; this runner never edits source to make a failing check pass.
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import pwd
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
REPORT = ROOT / "evidence/stage2-local.json"
EXCLUDED = {"target", ".local", ".git", "__pycache__", "node_modules", ".venv"}
SUFFIXES = {".rs", ".toml", ".json", ".sql", ".py", ".sh", ".patch", ".md"}


def sha(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def source_snapshot():
    selected = {path for path in ROOT.glob("Cargo.*") if path.is_file()}
    for name in ["rust-toolchain", "rust-toolchain.toml", "rustfmt.toml", ".rustfmt.toml"]:
        if (ROOT / name).is_file():
            selected.add(ROOT / name)
    for name in ["crates", "examples", "scripts", "patches", ".cargo"]:
        for folder, directories, files in os.walk(ROOT / name):
            directories[:] = sorted(d for d in directories if d not in EXCLUDED)
            for filename in sorted(files):
                path = pathlib.Path(folder) / filename
                if path.suffix in SUFFIXES and not path.is_symlink():
                    selected.add(path)
    return {str(path.relative_to(ROOT)): sha(path) for path in sorted(selected)}


def test_counts(log):
    totals = dict(passed=0, failed=0, ignored=0, measured=0, filtered_out=0)
    summaries = 0
    for match in re.finditer(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out", log):
        summaries += 1
        for key, value in zip(totals, match.groups()):
            totals[key] += int(value)
    return {"summaries": summaries, **totals}


class Checks:
    def __init__(self, folder, environment, report):
        self.folder, self.environment, self.report = folder, environment, report

    def command(self, name, argv, extra_env=None, timeout=1200, minimum_tests=0):
        record = {"name": name, "command": [str(a) for a in argv], "status": "running"}
        self.report["checks"].append(record)
        print(f"[{name}] running", flush=True)
        logfile = self.folder / f"{len(self.report['checks']):02d}-{name}.log"
        record["log"] = str(logfile)
        environment = self.environment.copy()
        if extra_env:
            environment.update(extra_env)
            record["environment_keys"] = sorted(extra_env)  # Never record environment values.
        self.save()
        started = time.monotonic()
        try:
            with logfile.open("wb") as output:
                result = subprocess.run([str(a) for a in argv], cwd=ROOT, env=environment,
                                        stdout=output, stderr=subprocess.STDOUT, timeout=timeout)
            record["exit_code"] = result.returncode
            record["status"] = "passed" if result.returncode == 0 else "failed"
            if result.returncode:
                record["reason"] = "command returned a nonzero exit code"
        except (OSError, subprocess.SubprocessError) as error:
            record.update(status="failed", exit_code=None, reason=type(error).__name__)
        record["duration_seconds"] = round(time.monotonic() - started, 3)
        log = logfile.read_text(errors="replace") if logfile.exists() else ""
        record["test_counts"] = test_counts(log)
        if record["status"] == "passed" and record["test_counts"]["passed"] < minimum_tests:
            record.update(status="failed", reason=f"expected at least {minimum_tests} executed passing tests")
        self.save()
        print(f"[{name}] {record['status']}", flush=True)
        return record["status"] == "passed"

    def internal(self, name, action):
        record = {"name": name, "status": "running", "command": None}
        self.report["checks"].append(record)
        started = time.monotonic()
        try:
            record["result"] = action()
            record.update(status="passed", exit_code=0)
        except (OSError, ValueError, RuntimeError) as error:
            record.update(status="failed", exit_code=None, reason=str(error))
        record["duration_seconds"] = round(time.monotonic() - started, 3)
        self.save()
        return record["status"] == "passed"

    def blocked(self, name, reason):
        self.report["checks"].append({"name": name, "status": "blocked", "reason": reason,
                                      "exit_code": None, "duration_seconds": 0})
        self.save()

    def save(self):
        REPORT.parent.mkdir(exist_ok=True)
        temporary = REPORT.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(self.report, indent=2) + "\n")
        temporary.replace(REPORT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--postgres-bin", type=pathlib.Path)
    parser.add_argument("--postgres-share", type=pathlib.Path)
    parser.add_argument("--postgres-lib", type=pathlib.Path)
    args = parser.parse_args()
    (ROOT / "target").mkdir(exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="stage2-", dir=ROOT / "target"))
    environment = os.environ.copy()
    for key in list(environment):
        if key.startswith("ORACLE_TEST_") or key in {"ORACLE_COUNTER_V1", "ORACLE_COUNTER_V2", "GEMINI_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY", "DISCORD_TOKEN", "DISCORD_BOT_TOKEN"}:
            environment.pop(key, None)
    environment["CARGO_TARGET_DIR"] = str(ROOT / "target")
    environment.pop("CARGO_BUILD_TARGET", None)
    if args.postgres_lib:
        environment["LD_LIBRARY_PATH"] = str(args.postgres_lib.resolve()) + (":" + environment["LD_LIBRARY_PATH"] if environment.get("LD_LIBRARY_PATH") else "")
    pg_bin = args.postgres_bin.resolve() if args.postgres_bin else pathlib.Path(shutil.which("pg_ctl") or "/usr/lib/postgresql/18/bin/pg_ctl").parent
    before = source_snapshot()
    report = {"passed": False, "status": "running", "started_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "report": str(REPORT), "artifacts": str(folder), "checks": [], "source_sha256": before,
              "postgres_bin": str(pg_bin), "scope": "disposable local databases and native fixture executables; no live Discord/provider calls"}
    checks = Checks(folder, environment, report)
    cluster = folder / "pg"
    pg_started = False
    tests_ready = False
    fixtures = {}
    try:
        checks.command("rust-version", ["rustc", "--version"])
        report["rust_version"] = (folder / "01-rust-version.log").read_text().strip()
        pg_tools = all((pg_bin / name).is_file() for name in ["initdb", "pg_ctl", "postgres", "createdb", "pg_dump", "pg_restore"])
        if pg_tools:
            pg_tools = checks.command("postgres-version", [pg_bin / "postgres", "--version"])
            version = pathlib.Path(report["checks"][-1]["log"]).read_text().strip()
            report["postgres_version"] = version
            if not re.search(r"\b18(?:\.|\b)", version):
                report["checks"][-1].update(status="failed", reason="PostgreSQL 18 is required")
                pg_tools = False
        else:
            checks.blocked("postgres-version", "required PostgreSQL 18 executables are missing")
        stage1 = [sys.executable, "scripts/check-stage1.py"]
        for flag in ["postgres_bin", "postgres_share", "postgres_lib"]:
            value = getattr(args, flag)
            if value is not None:
                stage1 += ["--" + flag.replace("_", "-"), str(value.resolve())]
        if pg_tools:
            checks.command("stage1-regression", stage1, timeout=2400, minimum_tests=1)
        else:
            checks.blocked("stage1-regression", "PostgreSQL 18 tool preflight failed")
        checks.command("prepare-serenity", [sys.executable, "scripts/prepare-serenity.py"])
        default_ready = checks.command("build-default-fixtures", ["cargo", "build", "--locked", "-p", "oracle-example-counter", "-p", "oracle-example-dependent", "-p", "oracle-example-authority-probe"])
        host_ready = checks.command("build-host", ["cargo", "build", "--locked", "-p", "oracle"])
        copied = False
        if default_ready:
            def copy_v1():
                source = ROOT / "target/debug/oracle-example-counter"
                destination = ROOT / "target/stage2-fixtures/counter-v1"
                destination.parent.mkdir(exist_ok=True)
                binary_hash = sha(source)
                temporary = destination.with_suffix(".tmp")
                shutil.copyfile(source, temporary)
                if sha(temporary) != binary_hash:
                    raise RuntimeError("default counter changed while copying")
                temporary.chmod(0o555)
                temporary.replace(destination)
                fixtures["ORACLE_COUNTER_V1"] = str(destination)
                return {"path": str(destination), "sha256": binary_hash}
            copied = checks.internal("stage-counter-v1", copy_v1)
        else:
            checks.blocked("stage-counter-v1", "default fixture build failed")
        v2_ready = checks.command("build-v2-fixture", ["cargo", "build", "--locked", "-p", "oracle-example-counter", "--features", "v2", "--target-dir", "target/upgrade-v2"])
        if v2_ready:
            binary = ROOT / "target/upgrade-v2/debug/oracle-example-counter"
            fixtures["ORACLE_COUNTER_V2"] = str(binary)
            report["counter_v2"] = {"path": str(binary), "sha256": sha(binary)}
        authority_copied = False
        if default_ready:
            def copy_authority():
                source = ROOT / "target/debug/oracle-example-authority-probe"
                destination = ROOT / "target/stage2-fixtures/authority-v1"
                destination.parent.mkdir(exist_ok=True)
                binary_hash = sha(source)
                temporary = destination.with_suffix(".tmp")
                shutil.copyfile(source, temporary)
                if sha(temporary) != binary_hash:
                    raise RuntimeError("default authority probe changed while copying")
                temporary.chmod(0o555)
                temporary.replace(destination)
                fixtures["ORACLE_AUTHORITY_V1"] = str(destination)
                return {"path": str(destination), "sha256": binary_hash}
            authority_copied = checks.internal("stage-authority-v1", copy_authority)
        else:
            checks.blocked("stage-authority-v1", "default fixture build failed")
        authority_v2 = checks.command("build-authority-v2", ["cargo", "build", "--locked", "-p", "oracle-example-authority-probe", "--features", "v2", "--target-dir", "target/upgrade-v2"])
        if authority_v2:
            binary = ROOT / "target/upgrade-v2/debug/oracle-example-authority-probe"
            fixtures["ORACLE_AUTHORITY_V2"] = str(binary)
            report["authority_v2"] = {"path": str(binary), "sha256": sha(binary)}
        tests_ready = default_ready and copied and v2_ready and authority_copied and authority_v2
        test_command = ["cargo", "test", "--locked", "-p", "oracle-modules"]
        ignored = ["--", "--ignored", "--nocapture", "--test-threads=1"]
        for target, minimum in [("lifecycle", 1), ("effects", 4), ("upgrade", 1), ("restore", 1), ("authority", 1), ("publication", 3)]:
            name = "sqlite-" + target
            if tests_ready:
                checks.command(name, [*test_command, "--test", target, *ignored], fixtures, minimum_tests=minimum)
            else:
                checks.blocked(name, "fresh v1/v2 fixture preparation failed")
        if host_ready and default_ready:
            checks.command("developer-reload-upgrade", [sys.executable, "scripts/check-module-dev.py"], timeout=1200)
        else:
            checks.blocked("developer-reload-upgrade", "fresh host/default fixture build failed")
        if pg_tools:
            sock = folder / "socket"
            sock.mkdir(mode=0o700)
            init = [pg_bin / "initdb", "-D", cluster, "--locale=C", "--encoding=UTF8", "--auth=trust"]
            if args.postgres_share:
                init += ["-L", args.postgres_share.resolve()]
            initialized = checks.command("postgres-init", init, timeout=120)
            if initialized:
                options = f"-k {shlex.quote(str(sock))} -h '' -p 55440"
                pg_started = checks.command("postgres-start", [pg_bin / "pg_ctl", "-D", cluster, "-l", folder / "postgres.log", "-o", options, "-w", "start"], timeout=120)
            else:
                checks.blocked("postgres-start", "cluster initialization failed")
            for target, selector, database in [("lifecycle", "ORACLE_TEST_MODULE_POSTGRES_URL", "stage2_lifecycle"), ("upgrade", "ORACLE_TEST_UPGRADE_POSTGRES_URL", "stage2_upgrade")]:
                created = False
                if pg_started:
                    created = checks.command("postgres-create-" + target, [pg_bin / "createdb", "-h", sock, "-p", "55440", database], timeout=60)
                else:
                    checks.blocked("postgres-create-" + target, "private PostgreSQL cluster did not start")
                if created and tests_ready:
                    user = urllib.parse.quote(pwd.getpwuid(os.getuid()).pw_name, safe="")
                    socket_parameter = urllib.parse.quote(str(sock), safe="")
                    url = f"postgresql://{user}@localhost/{database}?host={socket_parameter}&port=55440"
                    checks.command("postgres-" + target, [*test_command, "--test", target, *ignored], {**fixtures, selector: url}, minimum_tests=1)
                else:
                    checks.blocked("postgres-" + target, "fresh fixture or isolated database prerequisite failed")
        else:
            for name in ["postgres-init", "postgres-start", "postgres-create-lifecycle", "postgres-lifecycle", "postgres-create-upgrade", "postgres-upgrade"]:
                checks.blocked(name, "PostgreSQL 18 tool preflight failed")
        checks.command("rust-1.95-check", ["cargo", "+1.95.0", "check", "--locked", "--workspace", "--all-targets", "--all-features"])
        checks.command("workspace-clippy", ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"])
        checks.command("workspace-format", ["cargo", "fmt", "--all", "--", "--check"])
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        report["failure_reason"] = f"runner setup/execution failed: {type(error).__name__}"
    finally:
        # pg_ctl may time out after starting postgres; its private PID file also
        # triggers cleanup even when the start command did not return success.
        if pg_started or (cluster / "postmaster.pid").exists():
            stopped = checks.command("postgres-stop", [pg_bin / "pg_ctl", "-D", cluster, "-m", "fast", "-w", "stop"], timeout=60)
            if not stopped:
                checks.command("postgres-stop-immediate", [pg_bin / "pg_ctl", "-D", cluster, "-m", "immediate", "-w", "stop"], timeout=60)
        after = source_snapshot()
        report["source_changed_during_run"] = before != after
        if before != after:
            report["source_sha256_after"] = after
            report["failure_reason"] = "source changed during qualification; a fresh frozen-source run is required"
        required = ["stage1-regression", "build-default-fixtures", "stage-counter-v1", "build-v2-fixture", "stage-authority-v1", "build-authority-v2", "build-host", "sqlite-lifecycle", "sqlite-effects", "sqlite-upgrade", "sqlite-restore", "sqlite-authority", "sqlite-publication", "developer-reload-upgrade", "postgres-lifecycle", "postgres-upgrade", "rust-1.95-check", "workspace-clippy", "workspace-format"]
        names = {check["name"] for check in report["checks"]}
        for name in required:
            if name not in names:
                checks.blocked(name, "runner could not reach this required check")
        report["passed"] = not report.get("failure_reason") and not report["source_changed_during_run"] and all(check["status"] == "passed" for check in report["checks"])
        report["status"] = "passed" if report["passed"] else "failed"
        if not report["passed"] and "failure_reason" not in report:
            report["failure_reason"] = "one or more required checks failed or were blocked; see complete check records"
        report["finished_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        checks.save()
    print(json.dumps({"passed": report["passed"], "report": str(REPORT), "artifacts": str(folder)}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
