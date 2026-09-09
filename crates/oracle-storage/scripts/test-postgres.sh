#!/usr/bin/env bash
set -euo pipefail
storage_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
repo_root=$(cd "$storage_root/../.." && pwd)
pg_bin=${ORACLE_TEST_PG_BIN:-$repo_root/prototypes/storage-fit/.local/pg/usr/lib/postgresql/18/bin}
if [[ ! -x "$pg_bin/initdb" ]]; then
  echo 'Set ORACLE_TEST_PG_BIN to PostgreSQL 18 bin directory (P4 local extraction is supported).' >&2
  exit 1
fi
export ORACLE_TEST_PG_BIN="$pg_bin"
export LD_LIBRARY_PATH="$pg_bin/../../../../lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
mkdir -p "$storage_root/.local"
run_dir=$(mktemp -d "$storage_root/.local/pg-XXXXXX")
mkdir -m 700 "$run_dir/socket"
cleanup() { "$pg_bin/pg_ctl" -D "$run_dir/data" -m fast -w stop >>"$run_dir/server.log" 2>&1 || true; }
trap cleanup EXIT
"$pg_bin/initdb" -D "$run_dir/data" --locale=C --encoding=UTF8 --auth=trust >"$run_dir/initdb.log"
"$pg_bin/pg_ctl" -D "$run_dir/data" -l "$run_dir/server.log" -o "-k $run_dir/socket -h '' -p 55435" -w start
"$pg_bin/createdb" -h "$run_dir/socket" -p 55435 oracle_storage_source
"$pg_bin/createdb" -h "$run_dir/socket" -p 55435 oracle_storage_restore
export ORACLE_TEST_POSTGRES_URL="postgresql://$(id -un)@localhost/oracle_storage_source?host=$run_dir/socket&port=55435"
export ORACLE_TEST_POSTGRES_RESTORE_URL="postgresql://$(id -un)@localhost/oracle_storage_restore?host=$run_dir/socket&port=55435"
cd "$repo_root"
cargo test --locked -p oracle-storage postgres_contract -- --nocapture
