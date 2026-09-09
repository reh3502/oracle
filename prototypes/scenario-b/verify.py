#!/usr/bin/env python3
"""Run bounded offline checks and regenerate P7 artifact/evidence metadata."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from datetime import datetime, timezone

root = Path(__file__).resolve().parent
commands = [
    ['cargo', 'test', '--locked'],
    ['cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings'],
    ['cargo', 'fmt', '--check'],
]
results = []
for command in commands:
    result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end='')
    results.append({'command': command, 'exit_code': result.returncode, 'output': result.stdout})
    if result.returncode:
        break
paths = [root / 'Cargo.toml', root / 'Cargo.lock', root / 'src/lib.rs', root / 'src/bin/activity-log.rs',
         root / 'tests/scenario.rs', root / 'target/debug/activity-log',
         root / '../process-runtime/src/lib.rs', root / '../process-runtime/src/rpc.rs', root / '../process-runtime/src/runtime.rs']
passed = len(results) == len(commands) and all(result['exit_code'] == 0 for result in results)
report = {
    'gate': 'P7', 'scope': 'real trusted subprocess plus deterministic local delivery/subscription fixtures',
    'passed': passed, 'recorded_at': datetime.now(timezone.utc).isoformat(),
    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'cargo': subprocess.check_output(['cargo', '--version'], text=True).strip(),
    'tests_passed': re.findall(r'^test (\w+) \.\.\. ok$', results[0]['output'], re.M),
    'commands': results,
    'artifacts_sha256': {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths if path.exists()},
    'live_discord_requests': 0,
    'limitations': ['No real Discord event subscription or delivery verified.', 'Preset queue/retention settings are metadata; full logging data plane is outside P7.', 'Delivery deduplication fixture persists only within a harness host; crash-before-ack test crashes before sending.'],
}
(root / 'local-report.json').write_text(json.dumps(report, indent=2) + '\n')
sys.exit(0 if passed else 1)
