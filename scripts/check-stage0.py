#!/usr/bin/env python3
"""Reproduce all local Stage 0 gates; never issue live Discord/Gemini requests."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'prototypes/artifacts'
OUT.mkdir(exist_ok=True)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--offline-only', action='store_true', help='exit successfully when local checks pass even if live evidence is absent')
args = parser.parse_args()
commands = [
    ('P1', ['./scripts/check-p1.sh']),
    ('P2_P6', ['./scripts/check-p2-p6.sh']),
    ('P3', ['python3', 'prototypes/serenity-baseline/check.py']),
    ('P4', ['./prototypes/storage-fit/scripts/run.sh']),
    ('P5_tests', ['cargo', 'test', '--locked', '--manifest-path', 'prototypes/gemini-contract/Cargo.toml']),
    ('P5_lint', ['cargo', 'clippy', '--locked', '--manifest-path', 'prototypes/gemini-contract/Cargo.toml', '--all-targets', '--', '-D', 'warnings']),
    ('P5_fmt', ['cargo', 'fmt', '--manifest-path', 'prototypes/gemini-contract/Cargo.toml', '--check']),
    ('P7', ['python3', 'prototypes/scenario-b/verify.py']),
]
results = []
for name, command in commands:
    print(f'Checking {name}...', flush=True)
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (OUT / f'{name}.log').write_text(result.stdout)
    results.append({'gate': name, 'command': command, 'exit_code': result.returncode, 'log': f'{name}.log'})
    if result.returncode:
        print(result.stdout)
        break
local_passed = len(results) == len(commands) and all(r['exit_code'] == 0 for r in results)

def read(relative):
    p = ROOT / relative
    return json.loads(p.read_text()) if p.exists() else {}

p3 = read('prototypes/serenity-baseline/artifacts/offline.json')
p5 = read('prototypes/gemini-contract/live-report.json')
# Never infer live success merely from a successful compilation or local fixture.
p3_live = p3.get('live_status', '').startswith('passed:')
p5_live = p5.get('passed') is True
# Live provider evidence must carry current adapter provenance to count.
p5_sources = p5.get('source_sha256', {})
p5_live = p5_live and bool(p5_sources) and all(
    (ROOT / 'prototypes/gemini-contract' / name).is_file()
    and hashlib.sha256((ROOT / 'prototypes/gemini-contract' / name).read_bytes()).hexdigest() == digest
    for name, digest in p5_sources.items()
)
report = {
    'recorded_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'local_passed': local_passed,
    'stage0_passed': local_passed and p3_live and p5_live,
    'commands': results,
    'live_evidence': {'P3': p3_live, 'P5': p5_live},
    'P8': 'optional comparison; not required for trusted-process Stage 0',
    'note': 'Live requests are separately authorized and are never replayed by this script.',
}
(OUT / 'stage0.json').write_text(json.dumps(report, indent=2)+'\n')
print(json.dumps({key: report[key] for key in ['local_passed', 'stage0_passed', 'live_evidence']}, indent=2))
sys.exit(0 if local_passed and (args.offline_only or report['stage0_passed']) else 2)
