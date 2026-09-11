#!/usr/bin/env python3
"""Run the local developer gate; --full also qualifies stages 1–4 on disposable PostgreSQL."""
import argparse
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", action="store_true", help="include native module, migration and backup/restore drills")
    parser.add_argument("--postgres-bin", type=pathlib.Path)
    parser.add_argument("--postgres-share", type=pathlib.Path)
    parser.add_argument("--postgres-lib", type=pathlib.Path)
    args = parser.parse_args()
    if args.full and not args.postgres_bin:
        parser.error("--full requires --postgres-bin (PostgreSQL 18)")
    if not args.full and any((args.postgres_bin, args.postgres_share, args.postgres_lib)):
        parser.error("PostgreSQL options require --full")
    env = os.environ.copy()
    # Ambient fixture/database settings must not turn the ordinary gate into live work.
    for key in list(env):
        if key.startswith(("ORACLE_", "PG", "DISCORD_", "GEMINI_", "GOOGLE_")) or any(
            part in key.upper() for part in ("TOKEN", "PASSWORD", "KEY", "SECRET", "CREDENTIAL", "DATABASE_URL", "CONNECTION_STRING")
        ):
            env.pop(key, None)
    env.pop("LD_PRELOAD", None)
    env.pop("CARGO_BUILD_TARGET", None)
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    commands = [
        [sys.executable, "scripts/check-architecture.py"],
        [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests"],
        [sys.executable, "scripts/prepare-serenity.py"],
    ]
    if args.full:
        postgres = []
        for option in ("bin", "share", "lib"):
            path = getattr(args, "postgres_" + option)
            if path:
                postgres += ["--postgres-" + option, str(path.resolve())]
        # Stage 2 embeds Stage 1, including the real both-backend restore drill.
        commands += [
            [sys.executable, "scripts/check-stage2.py", *postgres],
            [sys.executable, "scripts/check-stage3.py", "--require-postgres", *postgres],
            [sys.executable, "scripts/check-stage4.py", "--require-postgres", *postgres],
        ]
    else:
        commands += [
            ["cargo", "fmt", "--all", "--", "--check"],
            ["cargo", "test", "--locked", "--workspace"],
            ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
        ]
    for command in commands:
        print("+ " + " ".join(command), flush=True)
        result = subprocess.run(command, cwd=ROOT, env=env)
        if result.returncode:
            return result.returncode if result.returncode > 0 else 1
    print("Developer checks passed" + (" (stages 1–4 offline contracts, SQLite and PostgreSQL)" if args.full else " (local gate; run --full for subprocess and PostgreSQL qualification)"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
