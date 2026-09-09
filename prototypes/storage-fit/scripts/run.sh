#!/usr/bin/env bash
set -euo pipefail
p4_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$p4_root/.local/downloads" "$p4_root/artifacts"
p4_version=18.6-0ubuntu0.26.04.1
p4_pg="$p4_root/.local/pg"
if [[ ! -x "$p4_pg/usr/lib/postgresql/18/bin/postgres" ]]; then
  (
    cd "$p4_root/.local/downloads"
    apt-get download "postgresql-18=$p4_version" "postgresql-client-18=$p4_version" "libpq5=$p4_version"
    for p4_deb in ./*.deb; do dpkg-deb -x "$p4_deb" "$p4_pg"; done
  )
fi
export LD_LIBRARY_PATH="$p4_pg/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
p4_bin="$p4_pg/usr/lib/postgresql/18/bin"
p4_run=$(mktemp -d "$p4_root/.local/run-XXXXXX")
mkdir -m 700 "$p4_run/socket"
p4_cleanup() { "$p4_bin/pg_ctl" -D "$p4_run/data" -m fast -w stop >>"$p4_run/postgres.log" 2>&1 || true; }
trap p4_cleanup EXIT
"$p4_bin/initdb" -D "$p4_run/data" -L "$p4_pg/usr/share/postgresql/18" --locale=C --encoding=UTF8 --auth=trust >"$p4_run/initdb.log"
"$p4_bin/pg_ctl" -D "$p4_run/data" -l "$p4_run/postgres.log" -o "-k $p4_run/socket -h '' -p 55434" -w start
export P4_POSTGRES_URL="postgresql://$(id -un)@localhost/postgres?host=$p4_run/socket&port=55434"
cargo run --locked --release --manifest-path "$p4_root/Cargo.toml" -- "$p4_root/artifacts/p4-report.json" "$p4_run/contract.sqlite"
(
  cd "$p4_root"
  sha256sum .local/downloads/*.deb >artifacts/postgres-packages.sha256
  rustc -Vv >artifacts/toolchain.txt
  sha256sum target/release/oracle-storage-fit Cargo.lock src/main.rs src/schema.sql scripts/run.sh >artifacts/build.sha256
)
printf 'Disposable databases stopped on exit; retained under %s\n' "$p4_run"
