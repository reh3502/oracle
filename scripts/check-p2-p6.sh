#!/usr/bin/env bash
set -euo pipefail
stage0_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$stage0_root"
cargo build --locked --release -p oracle-fixture-alpha
for stage0_spec in 'drain-boundary oracle-drain-boundary p2' 'scenario-a oracle-scenario-a p6'; do
    read -r stage0_dir stage0_bin stage0_id <<< "$stage0_spec"
    stage0_path="prototypes/$stage0_dir"
    mkdir -p "$stage0_path/artifacts"
    cargo fmt --manifest-path "$stage0_path/Cargo.toml" -- --check
    cargo clippy --locked --manifest-path "$stage0_path/Cargo.toml" --all-targets -- -D warnings
    cargo build --locked --release --manifest-path "$stage0_path/Cargo.toml"
    "$stage0_path/target/release/$stage0_bin" "$stage0_path/artifacts/$stage0_id-report.json" "$stage0_root/target/release/oracle-fixture-alpha"
    python3 - "$stage0_path" "$stage0_bin" "$stage0_id" <<'PY'
import datetime, hashlib, json, pathlib, platform, subprocess, sys
path, binary, prototype = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
files = list((path/'src').rglob('*.rs'))+[path/'Cargo.toml',path/'Cargo.lock',pathlib.Path('scripts/check-p2-p6.sh')]
if prototype == 'p2':
 files += list(pathlib.Path('prototypes/process-runtime/src').rglob('*.rs')) + list(pathlib.Path('prototypes/fixtures').rglob('*.rs')) + [pathlib.Path('Cargo.toml'),pathlib.Path('prototypes/process-runtime/Cargo.toml')]
report={
 'recorded_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
 'rustc':subprocess.check_output(['rustc','-Vv'],text=True).strip(),
 'cargo':subprocess.check_output(['cargo','-Vv'],text=True).strip(),
 'platform':list(platform.uname()),
 'source_sha256':{str(p):sha(p) for p in files},
 'binary_sha256':sha(path/'target/release'/binary),
 'alpha_fixture_sha256':sha(pathlib.Path('target/release/oracle-fixture-alpha')),
 'checks':{'format':'passed','clippy_deny_warnings':'passed','release_build':'passed','acceptance_exit_code':0},
 'acceptance':json.loads((path/'artifacts'/f'{prototype}-report.json').read_text())
}
(path/'artifacts/verification.json').write_text(json.dumps(report,indent=2)+'\n')
PY
done
