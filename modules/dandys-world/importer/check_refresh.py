"""Qualify operator refresh/recovery commands against a real catalog in disposable state."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--catalog', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--workdir', required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    catalog = args.catalog.resolve(strict=True)
    work = args.workdir.absolute()
    work.mkdir(mode=0o700)  # Never overwrite or test against existing state.
    store = work / 'store'
    checks = []

    def run(command, *options, target=store, success=True):
        proc = subprocess.run([str(binary), command, '--store', str(target), *map(str, options)],
                              capture_output=True, text=True, timeout=60)
        checks.append({'command': command, 'exit_code': proc.returncode, 'expected_success': success})
        if (proc.returncode == 0) != success:
            raise AssertionError(f'{command} unexpected exit: {proc.returncode}: {proc.stderr[:1000]}')
        return json.loads(proc.stdout) if success else None

    original = run('publish', '--catalog', catalog)['snapshot_id']
    assert original == hashlib.sha256(catalog.read_bytes()).hexdigest()
    assert run('stage', '--catalog', catalog)['state'] == 'unchanged'
    candidate = json.loads(catalog.read_bytes())
    candidate['adapter_version'] += '-refresh-qualification'
    candidate_path = work / 'candidate.json'
    candidate_path.write_text(json.dumps(candidate, ensure_ascii=False, sort_keys=True, separators=(',', ':')))
    expected = hashlib.sha256(candidate_path.read_bytes()).hexdigest()
    proposed = run('stage', '--catalog', candidate_path)
    assert proposed['state'] == 'review_required'
    assert proposed['review']['candidate_digest'] == expected
    review = run('review')
    assert review['active_digest'] == original
    run('approve', '--active', '0' * 64, '--candidate', expected, success=False)
    assert run('query', '--input', '{"op":"status"}')['snapshot_id'] == original
    assert run('approve', '--active', original, '--candidate', expected)['published'] == expected
    assert run('review') is None
    backup = work / 'backup'
    run('backup', '--output', backup)
    run('backup', '--output', backup, success=False)
    assert run('rollback')['rolled_back'] == original
    # Corruption affects only a disposable catalog. Recovery may use the recorded
    # previous snapshot but must never promote arbitrary uncommitted files.
    (store / f'{original}.json').write_text('corrupt')
    run('query', '--input', '{"op":"status"}', success=False)
    recovered = run('recover')
    assert recovered == {'snapshot_id': expected, 'recovered': True}
    restored = work / 'restored'
    assert run('restore', '--backup', backup, target=restored)['restored'] == expected
    assert run('query', '--input', '{"op":"status"}', target=restored)['snapshot_id'] == expected
    assert run('rollback', target=restored)['rolled_back'] == original
    # Backup/restore preserve source freshness; no restore time is fabricated.
    restored_data = json.loads((restored / f'{original}.json').read_bytes())
    assert restored_data['sources'] == json.loads(catalog.read_bytes())['sources']
    report = {'passed': True, 'checks': checks, 'original': original, 'candidate': expected,
              'source_timestamps_preserved': True, 'workdir': str(work)}
    (work / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': len(checks), 'report': str(work / 'report.json')}))


if __name__ == '__main__':
    main()
