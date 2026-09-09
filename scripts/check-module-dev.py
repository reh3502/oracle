#!/usr/bin/env python3
"""Verify real reloads and explicit v1-to-v2 upgrade on a disposable SQLite host."""
import argparse
import datetime
import json
import os
import pathlib
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
GUILD = "100"
MODULE = "fixture.counter"


def objects(path):
    text = path.read_text() if path.exists() else ""
    decoder = json.JSONDecoder()
    values = []
    while text.strip():
        text = text.lstrip()
        try:
            value, end = decoder.raw_decode(text)
        except ValueError:
            break  # A concurrently written final JSON object is not complete yet.
        values.append(value)
        text = text[end:]
    return values


def run(argv, env, timeout=120):
    result = subprocess.run([str(a) for a in argv], cwd=ROOT, env=env,
                            capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        location = ""
        try:
            failure = json.loads(result.stdout)
            candidate = pathlib.Path(failure["report"]).resolve()
            if candidate.is_relative_to(ROOT / ".local/module-dev") and candidate.is_file():
                location = f"; report: {candidate}"
        except (ValueError, KeyError, TypeError):
            pass
        raise RuntimeError(f"local check command failed with exit code {result.returncode}{location}")
    try:
        return json.loads(result.stdout)
    except ValueError as error:
        raise RuntimeError("local check command returned invalid JSON") from error


def owned_session_pids(session):
    # The host starts a private session; native modules create process groups
    # within it. This cleanup never selects another host or the user's session.
    result = []
    for entry in pathlib.Path("/proc").iterdir():
        if entry.name.isdecimal():
            pid = int(entry.name)
            try:
                if os.getsid(pid) == session:
                    result.append(pid)
            except ProcessLookupError:
                pass
    return result


def stop_host(host, report, log):
    if host.poll() is None:
        host.send_signal(signal.SIGTERM)
    try:
        host.wait(timeout=25)
    except subprocess.TimeoutExpired:
        report["forced_cleanup"] = True
        for pid in owned_session_pids(host.pid):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        host.wait(timeout=5)
    remaining = owned_session_pids(host.pid)
    if remaining:
        report["forced_cleanup"] = True
        for pid in remaining:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    report["host_exit_code"] = host.returncode
    stopped = next((event for event in objects(log) if event.get("event") == "stopped"), None)
    report["stopped"] = stopped
    if host.returncode != 0 or report.get("forced_cleanup") or stopped is None:
        raise RuntimeError("host did not complete graceful joined shutdown")
    if stopped["tasks"]["stats"]["counts"]["running"] != 0:
        raise RuntimeError("host shutdown reported remaining tracked tasks")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", type=pathlib.Path, default=ROOT / "target/debug/oracle")
    args = parser.parse_args()
    executable = args.oracle.resolve()
    environment = os.environ.copy()
    for key in ["GEMINI_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY", "DISCORD_TOKEN",
                "DISCORD_BOT_TOKEN", "ORACLE_TEST_POSTGRES_URL"]:
        environment.pop(key, None)
    folder = ROOT / ".local/module-dev/checks" / uuid.uuid4().hex
    folder.mkdir(parents=True)
    report_path = folder / "report.json"
    report = {"passed": False, "report": str(report_path),
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    exit_code = 0
    try:
        with tempfile.TemporaryDirectory(prefix="oracle-module-dev-check-") as temporary:
            config = pathlib.Path(temporary) / "oracle.json"
            command = [executable, "--config", config]
            initial = run([*command, "init"], environment)
            if initial["modules_loaded"] != 0 or initial["ai_available"]:
                raise RuntimeError("new deployment did not start empty without AI")
            settings = json.loads(config.read_text())
            if settings["discord"] is not None or settings["database"]["backend"] != "sqlite":
                raise RuntimeError("check requires a local SQLite deployment with Discord disabled")
            settings["guilds"] = [{"guild": GUILD, "operators": ["10"]}]
            config.write_text(json.dumps(settings, indent=2) + "\n")
            log = folder / "host.jsonl"
            with log.open("wb") as stdout, (folder / "host.stderr").open("wb") as stderr:
                host = subprocess.Popen([str(a) for a in [*command, "serve"]], cwd=ROOT,
                                        env=environment, stdout=stdout, stderr=stderr,
                                        start_new_session=True)
                report["host_pid"] = host.pid
                try:
                    deadline = time.monotonic() + 20
                    while True:
                        if host.poll() is not None:
                            raise RuntimeError("host exited before readiness")
                        ready = next((event for event in objects(log)
                                      if event.get("event") == "ready"), None)
                        if ready is not None:
                            if ready["discord_connected"]:
                                raise RuntimeError("check host unexpectedly connected to Discord")
                            break
                        if time.monotonic() >= deadline:
                            raise RuntimeError("host readiness timed out")
                        time.sleep(0.025)
                    first = run([sys.executable, ROOT / "scripts/module-dev.py", "--reload",
                                 "--profile", "counter", "--config", config, "--guild", GUILD,
                                 "--trust-native", "--oracle", executable], environment, 600)
                    report["first_runner_report"] = first["report"]
                    if not first["passed"] or first["activation_restored_by_load"]:
                        raise RuntimeError("first reload did not perform the initial activation")
                    invocation = [*command, "module", "invoke", "--module", MODULE,
                                  "--guild", GUILD]
                    increment = run([*invocation, "--operation", "increment", "--input",
                                     '{"amount":7}'], environment)
                    if increment["value"] != 7:
                        raise RuntimeError("counter increment did not return the expected value")
                    second = run([sys.executable, ROOT / "scripts/module-dev.py", "--reload",
                                  "--profile", "counter", "--config", config, "--guild", GUILD,
                                  "--trust-native", "--oracle", executable], environment, 600)
                    report["second_runner_report"] = second["report"]
                    if not second["passed"] or not second["activation_restored_by_load"]:
                        raise RuntimeError("second reload did not observe restored activation")
                    if any(item["command"] == "activate" for item in second["commands"]):
                        raise RuntimeError("second reload repeated an already restored activation")
                    if first["generation"] == second["generation"] or first["epoch"] == second["epoch"]:
                        raise RuntimeError("reload did not replace generation and guild epoch")
                    read = run([*invocation, "--operation", "get"], environment)
                    if read["value"] != 7 or read["generation"] != second["generation"] or read["epoch"] != second["epoch"]:
                        raise RuntimeError("counter state or new invocation identity was not preserved")
                    # Only this freshly generated SQLite file is inspected. No
                    # provider credentials or existing deployment are involved.
                    database = pathlib.Path(settings["database"]["path"])
                    if not database.is_absolute():
                        database = config.parent / database
                    database = database.resolve()
                    if not database.is_relative_to(pathlib.Path(temporary).resolve()):
                        raise RuntimeError("check database escaped its disposable directory")
                    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
                        before = connection.execute(
                            "SELECT value,data_version FROM oracle_module_documents WHERE module=? AND guild=? AND collection='counters' AND key='main'",
                            (MODULE, GUILD)).fetchone()
                    if before is None or json.loads(before[0]) != {"count": 7} or before[1] != 1:
                        raise RuntimeError("counter v1 did not persist the expected count document")
                    upgraded = run([sys.executable, ROOT / "scripts/module-dev.py", "--reload",
                                    "--profile", "counter", "--v2", "--upgrade", "--config", config,
                                    "--guild", GUILD, "--trust-native", "--oracle", executable],
                                   environment, 600)
                    report["upgrade_runner_report"] = upgraded["report"]
                    if not upgraded["passed"] or not upgraded["upgrade_completed"] or not upgraded["activation_restored_by_upgrade"]:
                        raise RuntimeError("explicit upgrade did not restore the desired activation")
                    commands = [item["command"] for item in upgraded["commands"]]
                    if commands.count("upgrade") != 1 or any(name in commands for name in ["unload", "load", "activate"]):
                        raise RuntimeError("upgrade runner bypassed the explicit migration lifecycle")
                    if upgraded["generation"] == second["generation"] or upgraded["epoch"] == second["epoch"]:
                        raise RuntimeError("upgrade did not advance module generation and guild epoch")
                    upgraded_read = run([*invocation, "--operation", "get"], environment)
                    if upgraded_read["value"] != 7 or upgraded_read["generation"] != upgraded["generation"] or upgraded_read["epoch"] != upgraded["epoch"]:
                        raise RuntimeError("upgraded counter did not preserve its value and new identity")
                    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
                        namespace = connection.execute(
                            "SELECT data_version,target_version,artifact_digest,cursor FROM oracle_module_namespaces WHERE module=? AND guild=?",
                            (MODULE, GUILD)).fetchone()
                        document = connection.execute(
                            "SELECT value,data_version FROM oracle_module_documents WHERE module=? AND guild=? AND collection='counters' AND key='main'",
                            (MODULE, GUILD)).fetchone()
                    if namespace != (2, None, None, None) or document is None or json.loads(document[0]) != {"total": 7} or document[1] != 2:
                        raise RuntimeError("upgrade did not finish at data version 2 with the transformed total document")
                    downgrade = subprocess.run(
                        [sys.executable, str(ROOT / "scripts/module-dev.py"), "--reload", "--profile", "counter",
                         "--config", str(config), "--guild", GUILD, "--trust-native", "--oracle", str(executable)],
                        cwd=ROOT, env=environment, capture_output=True, text=True, timeout=600)
                    rejected = json.loads(downgrade.stdout)
                    report["rejected_downgrade_report"] = rejected["report"]
                    if downgrade.returncode == 0 or rejected["passed"] or any(
                            item["command"] != "list" for item in rejected.get("commands", [])):
                        raise RuntimeError("ordinary v1 reload did not reject downgrade before mutation")
                    after_rejection = run([*invocation, "--operation", "get"], environment)
                    if after_rejection != upgraded_read:
                        raise RuntimeError("rejected downgrade changed the active upgraded counter")
                    report.update(upgrade_generation=upgraded["generation"], upgrade_epoch=upgraded["epoch"],
                                  data_version=namespace[0], document=json.loads(document[0]),
                                  implicit_downgrade_rejected=True)
                    if host.poll() is not None:
                        raise RuntimeError("host process exited during module replacement")
                    status = run([*command, "status"], environment)
                    if status["deployment"] != initial["deployment"]:
                        raise RuntimeError("reload changed the host deployment")
                    report.update(first_generation=first["generation"], second_generation=second["generation"],
                                  first_epoch=first["epoch"], second_epoch=second["epoch"],
                                  counter_value=read["value"], host_pid_unchanged=True)
                finally:
                    stop_host(host, report, log)
            report["passed"] = True
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, subprocess.SubprocessError, sqlite3.Error) as error:
        report["failure_type"] = type(error).__name__
        report["failure"] = str(error) if isinstance(error, RuntimeError) else "isolated check failed"
        exit_code = 1
    finally:
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
