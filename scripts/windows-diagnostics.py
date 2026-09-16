"""Read-only local command diagnostics. Never reads .env or member/run records."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess


def collect(bundle, root):
    report = {'version': 1}
    config_path = root / 'oracle.json'
    config = json.loads(config_path.read_text(encoding='utf-8-sig'))
    report['configuration'] = {key: config.get(key) for key in
                               ('guilds', 'member_reads', 'member_mutations')}
    report['ai_disabled'] = config.get('ai') is None
    host = bundle / 'oracle-host.exe'
    report['host_sha256'] = hashlib.sha256(host.read_bytes()).hexdigest()
    environment = os.environ.copy()
    environment.pop('DISCORD_TOKEN', None)
    environment.pop('GEMINI_API_KEY', None)
    try:
        result = subprocess.run([str(host), '--config', str(config_path), 'module', 'health'],
                                capture_output=True, timeout=15, env=environment)
        report['health_exit_code'] = result.returncode
        if result.returncode == 0:
            health = json.loads(result.stdout)
            report['modules'] = {name: {key: value.get(key) for key in ('generation', 'host', 'guilds')}
                                 for name, value in health.items()}
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        report['health_error'] = type(error).__name__
    database = Path(config['database']['path'])
    if not database.is_absolute():
        database = root / database
    try:
        with sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True, timeout=5) as db:
            db.execute('PRAGMA query_only=ON')
            rows = db.execute("SELECT guild,kind,key,value FROM oracle_workflows WHERE kind='command_binding' OR (kind='command_group' AND key='publication') ORDER BY guild,kind,key LIMIT 1000")
            report['commands'] = []
            for guild, kind, key, raw in rows:
                value = json.loads(raw)
                item = {'guild': guild, 'kind': kind, 'key': key}
                if kind == 'command_binding':
                    item.update({k: value.get(k) for k in ('owner', 'id', 'route', 'pending', 'deleted')})
                    item['name'] = value.get('definition', {}).get('name')
                else:
                    item.update({k: value.get(k) for k in ('active', 'affected')})
                report['commands'].append(item)
    except (OSError, ValueError, sqlite3.Error) as error:
        report['database_error'] = type(error).__name__
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(os.environ.get('LOCALAPPDATA', '.')) / 'OracleSister')
    parser.add_argument('--bundle', type=Path, default=Path(__file__).resolve().parent)
    args = parser.parse_args()
    try:
        report = collect(args.bundle.resolve(), args.root.resolve())
    except (OSError, ValueError, KeyError) as error:
        report = {'error': type(error).__name__}
    destination = args.bundle / 'Oracle diagnostics.json'
    destination.write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print('Saved Oracle diagnostics.json. This report excludes the bot token and run records.')


if __name__ == '__main__':
    main()
