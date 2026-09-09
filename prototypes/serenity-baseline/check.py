#!/usr/bin/env python3
"""Offline P3 check. Never reads credentials or invokes the live-write harness."""
import datetime
import hashlib
import json
import pathlib
import platform
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent
OUT = ROOT / 'artifacts'
OUT.mkdir(exist_ok=True)
REV = '98ec74223b0ff77fc4e8085d25569ea59e09a36f'
subprocess.run(['python3', str(ROOT / 'prepare_fork.py')], check=True)
commands = [
    ['cargo', '+1.95.0', 'test', '--locked', '--manifest-path', 'upstream/Cargo.toml', '--target-dir', 'target', '-j', '2'],
    ['cargo', '+1.95.0', 'fmt', '--all', '--', '--check'],
    ['cargo', '+1.95.0', 'check', '--locked', '--all-targets', '-j', '2'],
    ['cargo', '+1.95.0', 'clippy', '--locked', '--all-targets', '-j', '2', '--', '-D', 'warnings'],
    ['cargo', '+1.95.0', 'test', '--locked', '-j', '2'],
]
results = []
for index, command in enumerate(commands):
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (OUT / f'check-{index}.log').write_text(result.stdout)
    print(result.stdout, end='')
    results.append({'command': command, 'exit_code': result.returncode, 'log': f'check-{index}.log'})
    if result.returncode:
        break
metadata = json.loads(subprocess.check_output(['cargo', '+1.95.0', 'metadata', '--locked', '--format-version', '1'], cwd=ROOT, text=True))
serenity = [package for package in metadata['packages'] if package['name'] == 'serenity']
upstream_metadata = json.loads(subprocess.check_output(['cargo', '+1.95.0', 'metadata', '--locked', '--manifest-path', 'upstream/Cargo.toml', '--format-version', '1'], cwd=ROOT, text=True))
upstream = [p for p in upstream_metadata['packages'] if p['name'] == 'serenity']
source_ok = len(upstream) == 1 and upstream[0]['source'].endswith('#' + REV) and len(serenity) == 1 and serenity[0]['source'] is None and pathlib.Path(serenity[0]['manifest_path']).resolve() == (ROOT / 'target/serenity-fork/Cargo.toml').resolve()
patch_digest = hashlib.sha256((ROOT / 'patches/0001-preserve-unknown-dispatch.patch').read_bytes()).hexdigest()
live_path = OUT / 'live-authorized.json'
live_status = 'unverified: no current separately authorized live evidence'
if live_path.exists():
    live = json.loads(live_path.read_text())
    current_sources = all((ROOT / path).is_file() and hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == digest for path, digest in live.get('source_sha256', {}).items())
    if live.get('status') == 'passed' and live.get('cleanup_errors') == [] and live.get('source_sha256') and current_sources and live.get('fork_patch_sha256') == patch_digest:
        live_status = 'passed: separately authorized run with matching sources; not replayed by offline checks'
report = {
    'recorded_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'offline_status': 'passed' if source_ok and len(results) == len(commands) and all(r['exit_code'] == 0 for r in results) else 'failed',
    'live_status': live_status,
    'scope': 'Stage 0 P3 baseline compile and offline dispatch/HTTP/command fixtures; not full production Discord adapter qualification',
    'serenity': [{key: p[key] for key in ['name', 'version', 'source', 'rust_version', 'edition']} for p in serenity],
    'exact_source_verified': source_ok,
    'upstream_source': upstream[0]['source'],
    'fork_base': REV, 'fork_patch_sha256': patch_digest,
    'rustc': subprocess.check_output(['rustc', '+1.95.0', '-Vv'], cwd=ROOT, text=True).strip(),
    'platform': list(platform.uname()),
    'commands': results,
    'source_sha256': {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(ROOT.rglob('*')) if path.is_file() and 'target' not in path.relative_to(ROOT).parts and 'artifacts' not in path.relative_to(ROOT).parts},
}
(OUT / 'offline.json').write_text(json.dumps(report, indent=2) + '\n')
print('P3 offline:', report['offline_status'], '| live:', live_status)
raise SystemExit(0 if report['offline_status'] == 'passed' else 1)
