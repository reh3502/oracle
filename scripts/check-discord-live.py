#!/usr/bin/env python3
"""Explicit bounded live Discord release canary; credentials are never written.

Creates and cleans only nonce-named fixture resources. No model calls. Existing
deployment files are not loaded. Supply DISCORD_TOKEN/GUILD_ID in the environment
or explicitly name a private --env-file containing those two keys.
"""
import argparse
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("stage5_live_gate", ROOT / "scripts/check-stage5.py")
stage5 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage5)
CHECKS = ("complete_inventory", "nonce_preflight", "create_plan", "apply_complete", "receipt_readback",
          "repeat_noop", "owned_cleanup", "postflight_unchanged", "storage_closed", "gateway_ready",
          "gateway_shutdown", "command_verified", "owned_command_cleanup", "commands_unchanged")


def canary_sources():
    return {str(p.relative_to(ROOT)): stage5.digest(p) for p in (
        ROOT / "crates/oracle-discord/examples/stage5_live.rs", ROOT / "scripts/check-discord-live.py")}


def validate_report(report):
    stage5.require(report.get("schema") == 1 and report.get("purpose") == "stage5_structure_live", "wrong live report")
    stage5.require(report.get("passed") is True and not report.get("failure"), "live canary failed")
    for name in CHECKS:
        stage5.require(report.get(name) is True, "missing live check: " + name)


def validate_evidence(path):
    record = json.loads(path.read_text())
    stage5.require(record.get("passed") is True and record.get("scope") == "bounded live Discord canary; no model calls", "live evidence incomplete")
    stage5.require(record["runtime_sources"] == stage5.runtime_sources(), "live runtime source changed")
    stage5.require(record["canary_sources"] == canary_sources(), "live canary source changed")
    stage5.require(record["source_changed"] is False, "source changed during live canary")
    checks = record["checks"]
    stage5.require([c["name"] for c in checks] == ["build-canary", "live-canary"] and
                   all(c["status"] == "passed" and c["exit_code"] == 0 for c in checks), "live commands did not pass")
    raw = pathlib.Path(record["canary_report"])
    stage5.require(stage5.digest(raw) == record["canary_sha256"], "live report digest changed")
    validate_report(json.loads(raw.read_text()))
    return {"status": "passed", "sha256": stage5.digest(path), "canary_sha256": record["canary_sha256"],
            "checked_at": record["started_at_utc"], "human_slash_invocation": "not_exercised",
            "scope": "live Gateway, command CRUD, shared structure plan/apply/readback/repeat and owned cleanup"}


def credentials(env, env_file):
    values = {key: env[key] for key in ("DISCORD_TOKEN", "GUILD_ID") if key in env}
    if env_file:
        # Parse literal values, never source/execute a credentials file in a shell.
        for line in env_file.read_text().splitlines():
            key, separator, value = line.strip().partition("=")
            if separator and key in ("DISCORD_TOKEN", "GUILD_ID"):
                values[key] = value.strip().strip("\"'")
    stage5.require(bool(values.get("DISCORD_TOKEN")) and values.get("GUILD_ID", "").isdigit(), "missing canary credentials")
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute", action="store_true", help="explicitly authorize fixture creation and cleanup")
    parser.add_argument("--env-file", type=pathlib.Path)
    args = parser.parse_args()
    if not args.execute:
        parser.error("live changes require --execute")
    try:
        secrets = credentials(os.environ, args.env_file)
    except (OSError, ValueError):
        print("Canary credentials unavailable", file=sys.stderr)
        return 2
    parent = ROOT / "target/stage5-live"
    parent.mkdir(parents=True, exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    q = stage5.stage4.Qualification(folder, stage5.stage4.isolated_environment(os.environ))
    q.report.update(scope="bounded live Discord canary; no model calls", runtime_sources=stage5.runtime_sources(),
                    canary_sources=canary_sources())
    raw = folder / "canary.json"
    try:
        q.command("build-canary", ["cargo", "build", "--locked", "-p", "oracle-discord", "--example", "stage5_live"])
        binary = ROOT / "target/debug/examples/stage5_live"
        q.report["binary_sha256"] = stage5.digest(binary)
        q.command("live-canary", [binary, "--execute", raw], secrets, timeout=600)
        validate_report(json.loads(raw.read_text()))
        q.report.update(canary_report=str(raw), canary_sha256=stage5.digest(raw))
        q.report["source_changed"] = q.report["source_sha256"] != stage5.stage4.stage3.snapshot()
        stage5.require(not q.report["source_changed"], "source changed during live canary")
        q.report["passed"] = True
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        q.report.update(passed=False, error=type(error).__name__)
        print("Canary failed; inspect the local report for cleanup status. Do not blindly retry.", file=sys.stderr)
    finally:
        q.save()
        print(folder / "report.json")
    return 0 if q.report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
