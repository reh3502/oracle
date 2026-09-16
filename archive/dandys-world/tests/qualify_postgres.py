"""Run real run-store races/recovery in a disposable local PostgreSQL cluster.

Pass --pg-bin /path/to/postgresql/bin. Existing database servers are never used.
Evidence and cluster files live under the caller's ignored --output directory.
"""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import urllib.parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pg-bin", type=pathlib.Path, required=True)
    parser.add_argument("--pg-share", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--module-binary", type=pathlib.Path)
    parser.add_argument("--catalog", type=pathlib.Path)
    args = parser.parse_args()
    if bool(args.module_binary) != bool(args.catalog):
        parser.error("--module-binary and --catalog must be supplied together")
    repo = pathlib.Path(__file__).resolve().parents[3]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    folder = pathlib.Path(tempfile.mkdtemp(prefix="dw-pg-"))
    socket = folder / "socket"
    socket.mkdir()
    data = folder / "data"
    binary = args.pg_bin.resolve()
    env = os.environ.copy()
    env["ORACLE_TEST_PG_BIN"] = str(binary)
    checks = []

    def run(name, command, extra=None):
        with (output / (name + ".log")).open("w") as log:
            result = subprocess.run([str(x) for x in command], cwd=repo,
                                    env=env | (extra or {}), stdout=log,
                                    stderr=subprocess.STDOUT, timeout=600)
        checks.append({"name": name, "exit_code": result.returncode})
        print(name, result.returncode, flush=True)
        result.check_returncode()

    def url(database):
        return (f"postgresql://{urllib.parse.quote(env['USER'], safe='')}@localhost/{database}"
                f"?host={urllib.parse.quote(str(socket), safe='')}&port=55466")

    stopped = False
    try:
        command = [binary / "initdb", "-D", data, "-A", "trust", "--no-locale"]
        if args.pg_share:
            command.extend(["-L", args.pg_share.resolve()])
        run("pg-init", command)
        run("pg-start", [binary / "pg_ctl", "-D", data, "-l", folder / "server.log",
                         "-o", f"-k {socket} -p 55466 -c listen_addresses=''", "-w", "start"])
        for database in ("runs", "restored", "races", "interruption", "native", "wiki"):
            run("pg-create-" + database,
                [binary / "createdb", "-h", socket, "-p", "55466", database])
        cargo = ["cargo", "test", "--locked", "--manifest-path",
                 "archive/dandys-world/Cargo.toml"]
        run("pg-store", cargo + ["--test", "run_storage", "--", "--ignored"],
            {"DW_TEST_POSTGRES_URL": url("runs"),
             "DW_TEST_POSTGRES_RESTORE_URL": url("restored")})
        run("pg-races", cargo + ["--test", "run_storage_races", "--", "--ignored"],
            {"DW_TEST_POSTGRES_URL": url("races")})
        run("pg-interruption", cargo + ["--test", "run_interruption",
                                       "postgres_process_killed_at_write_boundaries_replays_one_signup",
                                       "--", "--ignored", "--exact"],
            {"DW_TEST_POSTGRES_URL": url("interruption")})
        if args.module_binary:
            native_env = {"DW_MODULE_BINARY": str(args.module_binary.resolve()),
                          "DW_RUN_CATALOG": str(args.catalog.resolve()),
                          "ORACLE_TEST_DW_RUNS_POSTGRES_URL": url("native"),
                          "ORACLE_TEST_MODULE_POSTGRES_URL": url("wiki")}
            host = ["cargo", "test", "--locked", "-p", "oracle-modules"]
            run("pg-native", host + ["--test", "dw_runs", "native_runs_postgres",
                                     "--", "--ignored", "--exact"], native_env)
            run("pg-wiki", host + ["--test", "member_runtime", "member_runtime_postgres",
                                   "--", "--ignored", "--exact"], native_env)
    finally:
        if (data / "postmaster.pid").exists():
            run("pg-stop", [binary / "pg_ctl", "-D", data, "-m", "fast", "-w", "stop"])
        stopped = not (data / "postmaster.pid").exists()
        (output / "postgres-report.json").write_text(
            json.dumps({"checks": checks, "stopped": stopped}, indent=2) + "\n")
        if stopped:
            shutil.rmtree(folder)


if __name__ == "__main__":
    main()
