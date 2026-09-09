#!/usr/bin/env python3
"""Build and immutably stage SDK fixtures; optionally reload an already-running local host."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORK = ROOT / ".local/module-dev"


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def source_snapshot(profile):
    files = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
    for name in ["crates/oracle-contracts", "crates/oracle-rpc", "crates/oracle-task-scope",
                 "crates/oracle-module-sdk", f"examples/modules/{profile}"]:
        folder = ROOT / name
        files.extend(p for p in folder.rglob("*") if p.is_file()
                     and (p.suffix in {".rs", ".json"} or p.name == "Cargo.toml"))
    hashes = {str(path.relative_to(ROOT)): digest(path) for path in sorted(set(files))}
    return hashes, hashlib.sha256(json_bytes(hashes)).hexdigest()


def checked(argv, **kwargs):
    return subprocess.run([str(a) for a in argv], cwd=ROOT, check=True,
                          timeout=600, **kwargs)


def tool_output(*argv):
    return checked(argv, capture_output=True, text=True).stdout.strip()


def stage(args, report, run_dir):
    profile = args.profile
    crate = f"oracle-example-{profile}"
    selected = "v2" if args.v2 else "v1"
    fixture = ROOT / "examples/modules" / profile
    manifest_path = fixture / ("manifest-v2.json" if args.v2 else "manifest.json")
    manifest = json.loads(manifest_path.read_text())
    rustc = tool_output("rustc", "-vV")
    host = next((line.split(": ", 1)[1] for line in rustc.splitlines()
                 if line.startswith("host: ")), None)
    if manifest["target"] != host:
        raise ValueError("fixture manifest target does not match the native Rust toolchain")
    hashes, source_hash = source_snapshot(profile)
    git = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                         capture_output=True, text=True, timeout=10)
    revision = git.stdout.strip() if git.returncode == 0 else "unborn"
    if revision != "unborn" and not re.fullmatch(r"[0-9a-f]{40,64}", revision):
        raise ValueError("unexpected git revision format")
    target = WORK / "build" / f"{profile}-{selected}"
    command = ["cargo", "build", "--locked", "-p", crate, "--target-dir", str(target)]
    if args.v2:
        command += ["--features", "v2"]
    report["build_log"] = str(run_dir / "build.log")
    with (run_dir / "build.log").open("wb") as log:
        checked(command, stdout=log, stderr=subprocess.STDOUT)
    if source_snapshot(profile) != (hashes, source_hash):
        raise ValueError("fixture or dependency source changed during build; rerun to stage a consistent snapshot")
    binary = target / "debug" / crate
    if not binary.is_file() or binary.is_symlink():
        raise ValueError("Cargo did not produce the expected native executable")
    binary_hash = digest(binary)
    provenance = {"git_revision": revision, "source_sha256": hashes,
                  "source_tree_sha256": source_hash, "rustc": rustc,
                  "cargo": tool_output("cargo", "--version"), "build_command": command,
                  "profile": profile, "feature_v2": args.v2, "license": args.license}
    provenance_bytes = json_bytes(provenance)
    package = {"manifest": manifest, "entrypoint": "module",
               "files": {"module": binary_hash,
                         "source-provenance.json": hashlib.sha256(provenance_bytes).hexdigest()},
               "source_revision": f"git:{revision};source-tree-sha256:{source_hash}",
               "toolchain": rustc.splitlines()[0] + "; " + provenance["cargo"],
               "license": args.license}
    package_bytes = json_bytes(package)
    stage_hash = hashlib.sha256(package_bytes).hexdigest()
    stages = WORK / "packages"
    stages.mkdir(parents=True, exist_ok=True)
    destination = stages / stage_hash
    if destination.exists():
        if (destination / "package.json").read_bytes() != package_bytes:
            raise ValueError("existing immutable stage has different package metadata")
        for name, expected in package["files"].items():
            if digest(destination / name) != expected:
                raise ValueError("existing immutable stage failed its file hash check")
    else:
        temporary = pathlib.Path(tempfile.mkdtemp(prefix=".stage-", dir=stages))
        try:
            shutil.copyfile(binary, temporary / "module")
            if digest(temporary / "module") != binary_hash:
                raise ValueError("binary changed during staging")
            (temporary / "source-provenance.json").write_bytes(provenance_bytes)
            (temporary / "package.json").write_bytes(package_bytes)
            for file in temporary.iterdir():
                with file.open("rb") as stream:
                    os.fsync(stream.fileno())
                file.chmod(0o555 if file.name == "module" else 0o444)
            temporary.chmod(0o555)
            temporary.rename(destination)
        finally:
            if temporary.exists():
                temporary.chmod(0o700)
                shutil.rmtree(temporary)
    report.update(stage=str(destination), staging_sha256=stage_hash,
                  binary_sha256=binary_hash, module=manifest["id"],
                  data_version=manifest["data_version"], source_tree_sha256=source_hash)
    return destination, manifest


def require_live_host(config_path):
    config = json.loads(config_path.read_text())
    state = pathlib.Path(config["state_dir"])
    if not state.is_absolute():
        state = config_path.parent / state
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(3)
        stream.connect(str(state / "control.sock"))
        # Complete a read-only request so probing readiness does not leave an
        # incomplete request or manufacture a failed local-control task.
        stream.sendall(b'{"command":"status","guild":null}\n')
        response = bytearray()
        while not response.endswith(b"\n"):
            block = stream.recv(4096)
            if not block or len(response) + len(block) > 131072:
                raise ValueError("running host did not complete its status response")
            response.extend(block)
        value = json.loads(response)
        if value.get("error") is not None or not isinstance(value.get("result"), dict):
            raise ValueError("running host status probe failed")
    return state.resolve()


def oracle(args, report, *command):
    step = command[0]
    result = subprocess.run([str(args.oracle), "--config", str(args.config), "module", *command],
                            cwd=ROOT, capture_output=True, text=True, timeout=120)
    command_report = {"command": step, "exit_code": result.returncode}
    report.setdefault("commands", []).append(command_report)
    if result.returncode:
        try:
            error_code = json.loads(result.stdout).get("error")
            allowed = {"invalid_input", "module_unavailable", "compatibility", "dependency_unavailable",
                       "data_version_mismatch", "schema_invalid", "quota_exceeded", "trusted_code_required",
                       "artifact_changed", "forbidden_scope", "forbidden_permission", "conflict", "not_found",
                       "storage_unavailable", "migration_mismatch", "backup", "integrity", "already_running",
                       "cancelled", "unknown_outcome", "recovery_required", "io"}
            if isinstance(error_code, str) and error_code in allowed:
                command_report["error_code"] = error_code
        except (ValueError, AttributeError):
            pass
        raise RuntimeError(f"oracle module {step} failed with exit code {result.returncode}")
    try:
        return json.loads(result.stdout)
    except ValueError as error:
        raise ValueError(f"oracle module {step} returned invalid JSON") from error


def reload_module(args, report, source, manifest):
    report["config"] = str(args.config)
    report["state_directory"] = str(require_live_host(args.config))
    installed = oracle(args, report, "list")
    if not isinstance(installed, list):
        raise ValueError("unsupported module list response; expected installed-package array")
    # A plain unload/load is not a data-version upgrade or a rollback. Even an
    # installed newer artifact is conservatively treated as requiring an explicit
    # migration-aware command because health does not expose namespace versions.
    for item in installed:
        other = item["package"]["manifest"]
        if other["id"] != manifest["id"]:
            continue
        if other["data_version"] > manifest["data_version"]:
            raise ValueError("a newer data version is installed; implicit downgrade is forbidden")
        if other["data_version"] != manifest["data_version"] and not args.upgrade:
            raise ValueError("a different data version is installed; explicit --upgrade is required")
    health = oracle(args, report, "health")
    if not isinstance(health, dict):
        raise ValueError("unsupported module health response; expected module-ID map")
    existing = health.get(manifest["id"], {})
    if existing and not isinstance(existing.get("guilds"), dict):
        raise ValueError("existing module health is unavailable; refusing a blind replacement")
    if any(guild != args.guild and state.get("active")
           for guild, state in existing.get("guilds", {}).items()):
        raise ValueError("module is active in another guild; use the explicit multi-guild lifecycle workflow")
    if args.upgrade and not existing:
        raise ValueError("explicit upgrade requires the current module to be loaded")
    result = oracle(args, report, "install", "--source", str(source), "--trust-native")
    installed_digest = result.get("digest") if isinstance(result, dict) else None
    if not isinstance(installed_digest, str) or not re.fullmatch(r"[0-9a-f]{64}", installed_digest):
        raise ValueError("install response did not contain an InstalledModule digest")
    report["installed_digest"] = installed_digest
    if args.upgrade:
        report["upgrade_attempted"] = True
        oracle(args, report, "upgrade", "--module", manifest["id"],
               "--digest", installed_digest, "--grace-ms", "5000")
        report["upgrade_completed"] = True
    else:
        if manifest["id"] in health:
            oracle(args, report, "unload", "--module", manifest["id"], "--grace-ms", "5000")
            report["previous_generation_unloaded"] = True
        oracle(args, report, "load", "--digest", installed_digest)
    loaded_health = oracle(args, report, "health")
    if not isinstance(loaded_health, dict):
        raise ValueError("unsupported post-load health response; expected module-ID map")
    loaded = loaded_health.get(manifest["id"], {})
    if not isinstance(loaded, dict) or not isinstance(loaded.get("guilds"), dict):
        raise ValueError("loaded module health is unavailable")
    target = loaded["guilds"].get(args.guild, {})
    restored = target.get("active") is True
    report["activation_restored_by_load"] = restored and not args.upgrade
    if args.upgrade:
        report["activation_restored_by_upgrade"] = restored
    if not restored:
        activation = ["activate", "--module", manifest["id"], "--guild", args.guild]
        for capability in manifest["capabilities"]:
            activation += ["--grant", capability]
        if args.profile == "dependent":
            activation += ["--bindings", json.dumps({"counter/v1": "fixture.counter"})]
        oracle(args, report, *activation)
    report["health"] = oracle(args, report, "health")
    final = report["health"].get(manifest["id"], {})
    target = final.get("guilds", {}).get(args.guild, {})
    if target.get("active") is not True or type(target.get("epoch")) is not int:
        raise ValueError("target guild was not observed active with an epoch after reload")
    if type(final.get("generation")) is not int:
        raise ValueError("loaded module did not report its generation")
    report["generation"] = final["generation"]
    report["epoch"] = target["epoch"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--stage-only", action="store_true", help="build and stage without contacting a host (default)")
    mode.add_argument("--reload", action="store_true", help="install, replace the loaded generation, and activate on a running host")
    parser.add_argument("--profile", choices=["counter", "dependent", "activity-log"], default="counter")
    parser.add_argument("--v2", action="store_true", help="select the counter's migration-capable v2 artifact")
    parser.add_argument("--upgrade", action="store_true", help="explicitly advance counter data via the host upgrade command; requires --reload --v2")
    parser.add_argument("--config", type=pathlib.Path)
    parser.add_argument("--guild", help="configured guild snowflake for activation")
    parser.add_argument("--oracle", type=pathlib.Path, default=ROOT / "target/debug/oracle")
    parser.add_argument("--trust-native", action="store_true", help="explicitly authorize execution of this trusted native fixture")
    parser.add_argument("--license", default="UNLICENSED (repository declares no license)", help="package license provenance; no license is inferred")
    args = parser.parse_args()
    if args.v2 and args.profile != "counter":
        parser.error("--v2 applies only to --profile counter")
    if args.upgrade and not (args.reload and args.v2 and args.profile == "counter"):
        parser.error("--upgrade requires --reload --profile counter --v2")
    if args.reload:
        if not args.trust_native or args.config is None or args.guild is None:
            parser.error("--reload requires --trust-native, --config, and --guild")
        if not re.fullmatch(r"[1-9][0-9]*", args.guild) or int(args.guild) >= 2**64:
            parser.error("--guild must be a valid unsigned Discord snowflake")
        if args.v2 and not args.upgrade:
            parser.error("--v2 --reload requires explicit --upgrade; unload/load cannot advance data versions")
    if not args.license.strip() or len(args.license) > 1024:
        parser.error("--license must contain 1–1024 characters")
    args.oracle = args.oracle.resolve()
    if args.config is not None:
        args.config = args.config.resolve()
    WORK.mkdir(parents=True, exist_ok=True)
    run_dir = WORK / "runs" / uuid.uuid4().hex
    run_dir.mkdir(parents=True)
    report_path = run_dir / "report.json"
    report = {"passed": False, "mode": "reload" if args.reload else "stage-only",
              "profile": args.profile, "feature_v2": args.v2, "upgrade_requested": args.upgrade,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "report": str(report_path)}
    exit_code = 0
    try:
        with (WORK / "runner.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            source, manifest = stage(args, report, run_dir)
            if args.reload:
                reload_module(args, report, source, manifest)
            report["passed"] = True
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, subprocess.SubprocessError) as error:
        # Never dump environment variables, raw host stderr or arbitrary module errors.
        report["failure_type"] = type(error).__name__
        report["failure"] = str(error) if isinstance(error, (ValueError, RuntimeError)) else "local build or host command failed"
        exit_code = 1
    finally:
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report_path.write_bytes(json_bytes(report))
    print(json.dumps(report, indent=2))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
