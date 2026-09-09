#!/usr/bin/env bash
# Reproduce the Linux-only Stage 0 P1 gate without Discord or provider credentials.
set -euo pipefail
p1_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$p1_root"
p1_cycles="${1:-60}"
p1_output="$p1_root/prototypes/process-runtime/artifacts"
p1_target="$p1_root/target"
mkdir -p "$p1_output"
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --target-dir "$p1_target" -- -D warnings
cargo test --locked --workspace --target-dir "$p1_target"
cargo build --locked --release --workspace --target-dir "$p1_target"
python3 - "$p1_output/environment.json" <<'PY'
import datetime, hashlib, json, os, pathlib, platform, subprocess, sys

def command(*args):
    return subprocess.check_output(args, text=True).strip()

paths = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z']).decode().split('\0')
sources = {p: hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
           for p in sorted(set(paths)) if p and (p.endswith(('.rs', '.toml')) or p in ('Cargo.lock', 'scripts/check-p1.sh'))}
report = {
    'recorded_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'rustc': command('rustc', '-Vv'),
    'cargo': command('cargo', '-Vv'),
    'uname': list(platform.uname()),
    'logical_cpus': os.cpu_count(),
    'cpu_model': next((s.split(':',1)[1].strip() for s in pathlib.Path('/proc/cpuinfo').read_text().splitlines() if s.startswith('model name')), 'unknown'),
    'mem_total': next(s for s in pathlib.Path('/proc/meminfo').read_text().splitlines() if s.startswith('MemTotal:')),
    'base_commit': command('git', 'rev-parse', 'HEAD'),
    'source_sha256': sources,
    'note': 'Source hashes identify the exact working tree; base_commit alone may not include this prototype.'
}
pathlib.Path(sys.argv[1]).write_text(json.dumps(report, indent=2)+'\n')
PY
"$p1_target/release/p1" \
    --alpha "$p1_target/release/oracle-fixture-alpha" \
    --beta "$p1_target/release/oracle-fixture-beta" \
    --cycles "$p1_cycles" \
    --output "$p1_output/p1-release.json"
