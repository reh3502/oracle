#!/usr/bin/env python3
"""Measure real Linux process lifecycle resources offline; reports stay in target/.

Defaults are a bounded qualification workload, not evidence of a day-long soak or
production capacity. Limits are explicit regression hypotheses, not an SLA.
"""
import argparse
import datetime
import importlib.util
import json
import os
import pathlib
import platform
import signal
import time
import statistics
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("stage4_soak", ROOT / "scripts/check-stage4.py")
stage4 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage4)


def evaluate(samples, cycles, warmup, rss_growth_kib, fd_growth, rss_slope_kib):
    if len(samples) != cycles or [s["cycle"] for s in samples] != list(range(cycles)):
        raise ValueError("incomplete or duplicate lifecycle samples")
    for s in samples:
        if s["mode"] != ["graceful", "forced", "crash"][s["cycle"] % 3]:
            raise ValueError("missing lifecycle mode")
        if s["stop"]["cleanup_error"] or s["stop"]["descendants_reaped"] < 1:
            raise ValueError("process cleanup failed")
        if s["mode"] == "forced" and not s["stop"]["forced"]:
            raise ValueError("forced termination was not exercised")
        if s["mode"] == "crash" and s["stop"]["exit_code"] != 23:
            raise ValueError("crash was not exercised")
    steady = samples[warmup:]
    if len(steady) < 6:
        raise ValueError("at least six measured cycles are required")
    rss = [s["host"]["rss_kib"] for s in steady]
    fds = [s["host"]["fds"] for s in steady]
    window = max(1, len(steady) // 4)
    growth = statistics.median(rss[-window:]) - statistics.median(rss[:window])
    slope = statistics.linear_regression(range(len(rss)), rss).slope
    result = {"host_rss_peak_kib": max(s["host"]["rss_kib"] for s in samples),
              "guest_rss_peak_kib": max(s["guest"]["rss_kib"] for s in samples),
              "guest_fd_peak": max(s["guest"]["fds"] for s in samples),
              "host_fd_peak": max(fds), "host_fd_growth": max(fds) - min(fds),
              "host_rss_window_growth_kib": growth, "host_rss_slope_kib_per_cycle": slope,
              "cleanup_max_ms": max(s["stop_ms"] for s in samples),
              "elapsed_seconds": samples[-1]["elapsed_seconds"],
              "descendants_reaped": sum(s["stop"]["descendants_reaped"] for s in samples)}
    result["passed"] = growth <= rss_growth_kib and result["host_fd_growth"] <= fd_growth and slope <= rss_slope_kib
    return result


def stop_workload(process):
    """Stop admission first, then kill this workload's separate guest groups."""
    if process.poll() is not None:
        return
    os.kill(process.pid, signal.SIGSTOP)
    owned = {process.pid}
    # Guests cannot create new children once stopped; repeat until closure.
    while True:
        added = set()
        for entry in pathlib.Path("/proc").iterdir():
            if not entry.name.isdigit() or int(entry.name) in owned:
                continue
            try:
                status = (entry / "status").read_text()
                parent = int(next(line for line in status.splitlines() if line.startswith("PPid:")).split()[1])
                if parent in owned:
                    pid = int(entry.name)
                    os.kill(pid, signal.SIGSTOP)
                    added.add(pid)
            except (FileNotFoundError, ProcessLookupError):
                continue
        owned.update(added)
        if not added:
            break
    # The supervisor remains alive to reap the guests and adopted descendants.
    for pid in owned - {process.pid}:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    os.kill(process.pid, signal.SIGCONT)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def run_workload(q, binary, args):
    argv = [str(binary), str(args.cycles), str(args.calls), str(args.payload_bytes)]
    log = q.folder / "soak.jsonl"
    record = {"name": "soak", "command": argv, "status": "running", "log": str(log)}
    q.report["checks"].append(record)
    q.save()
    started = time.monotonic()
    try:
        with log.open("wb") as output:
            process = subprocess.Popen(argv, cwd=ROOT, env=q.env, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = process.wait(timeout=max(120, args.cycles * (args.calls * 2 + 10)))
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                stop_workload(process)
                raise
        record["exit_code"] = code
        if code:
            raise RuntimeError("soak process failed")
        record["status"] = "passed"
    except (OSError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        record.update(status="failed", reason=type(error).__name__)
        raise
    finally:
        record["seconds"] = round(time.monotonic() - started, 3)
        q.save()
    return [json.loads(line) for line in log.read_text().splitlines()]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cycles", type=int, default=300)
    parser.add_argument("--warmup", type=int, default=30)
    parser.add_argument("--calls", type=int, default=100)
    parser.add_argument("--payload-bytes", type=int, default=4096)
    parser.add_argument("--rss-growth-kib", type=int, default=8192)
    parser.add_argument("--fd-growth", type=int, default=2)
    parser.add_argument("--rss-slope-kib", type=float, default=32.0)
    args = parser.parse_args()
    if args.warmup < 0 or args.cycles - args.warmup < 6 or args.calls < 1 or not 1 <= args.payload_bytes <= 262144:
        parser.error("require nonnegative warmup, six measured cycles, positive calls and payload 1..262144")
    if min(args.rss_growth_kib, args.fd_growth, args.rss_slope_kib) < 0:
        parser.error("resource limits must be nonnegative")
    parent = ROOT / "target/process-soak"
    parent.mkdir(parents=True, exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    q = stage4.Qualification(folder, stage4.isolated_environment(os.environ))
    q.report.update(scope=__doc__, workload=vars(args), platform=platform.platform(),
                    architecture=platform.machine(), cpu_count=os.cpu_count(),
                    cpu_model=next((line.split(":", 1)[1].strip() for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown"),
                    memory_total_kib=int(pathlib.Path("/proc/meminfo").read_text().splitlines()[0].split()[1]),
                    profile="release", concurrency=1, database="none", network="none")
    error = None
    try:
        q.command("rust-version", ["rustc", "--version", "--verbose"])
        q.command("cargo-version", ["cargo", "--version"])
        q.command("build", ["cargo", "build", "--locked", "--release", "-p", "oracle-process", "--example", "process_soak"])
        binary = ROOT / "target/release/examples/process_soak"
        q.report["binary_sha256"] = stage4.stage3.sha(binary)
        samples = run_workload(q, binary, args)
        q.report["measurements"] = evaluate(samples, args.cycles, args.warmup, args.rss_growth_kib, args.fd_growth, args.rss_slope_kib)
        if not q.report["measurements"]["passed"]:
            raise RuntimeError("resource regression threshold exceeded")
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.SubprocessError, KeyboardInterrupt) as caught:
        error = type(caught).__name__
    finally:
        after = stage4.stage3.snapshot()
        q.report.update(source_sha256_after=after, source_changed=after != q.report["source_sha256"], error=error,
                        finished_at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat())
        q.report["passed"] = error is None and not q.report["source_changed"]
        q.save()
        print("Report:", folder / "report.json", flush=True)
    return 0 if q.report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
