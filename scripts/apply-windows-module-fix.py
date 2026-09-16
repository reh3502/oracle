"""Apply a verified DW module upgrade through the running host; preserve saved runs."""
import json
import hashlib
import re
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess


def run(bundle, config, *arguments):
    environment = os.environ.copy()
    environment.pop('DISCORD_TOKEN', None)
    environment.pop('GEMINI_API_KEY', None)
    try:
        result = subprocess.run([str(bundle / 'oracle-host.exe'), '--config', str(config), *arguments],
                                capture_output=True, timeout=120, env=environment)
    except (OSError, subprocess.TimeoutExpired):
        raise RuntimeError('Could not contact the bot. Keep Start Oracle.exe running and retry.') from None
    if result.returncode:
        codes = {'InvalidInput', 'ModuleUnavailable', 'Compatibility', 'DependencyUnavailable',
                 'DataVersionMismatch', 'SchemaInvalid', 'QuotaExceeded', 'TrustedCodeRequired',
                 'ArtifactChanged', 'ForbiddenScope', 'ForbiddenPermission', 'Conflict', 'NotFound',
                 'StorageUnavailable', 'MigrationMismatch', 'Backup', 'Integrity', 'AlreadyRunning',
                 'Cancelled', 'UnknownOutcome', 'RecoveryRequired', 'Io'}
        words = re.findall(r'[A-Za-z]+', result.stderr.decode('utf-8', errors='replace'))
        code = next((word for word in words if word in codes), 'unavailable')
        step = ' '.join(arguments[:2]) if arguments[0] == 'module' else arguments[0]
        raise RuntimeError('Bot control failed at ' + step + ': ' + code + '. Send this exact message.')
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise RuntimeError('Bot returned an invalid control response.') from None


def main():
    bundle = Path(__file__).resolve().parent
    root = Path(os.environ['LOCALAPPDATA']) / 'OracleSister'
    config = root / 'oracle.json'
    module = 'community.dandys-world'
    package = bundle / 'dw-fix-package'
    settings = json.loads(config.read_text(encoding='utf-8-sig'))
    if settings.get('ai') is not None:
        raise RuntimeError('Expected the AI-disabled sister deployment.')
    print('Checking update files...', flush=True)
    for name in ('package.json', 'dw-module.exe'):
        path = package / name
        try:
            data = path.read_bytes()
        except FileNotFoundError:
            raise RuntimeError('Missing dw-fix-package/' + name + '. Extract the entire fix ZIP into this bot folder, including dw-fix-package. If already extracted, check antivirus quarantine.') from None
        except PermissionError:
            raise RuntimeError('Windows blocked access to dw-fix-package/' + name + '. Send this message and the antivirus alert.') from None
    metadata = json.loads((package / 'package.json').read_text(encoding='utf-8-sig'))
    expected = metadata['files']['dw-module.exe']
    if hashlib.sha256((package / 'dw-module.exe').read_bytes()).hexdigest() != expected:
        raise RuntimeError('The update executable does not match package.json. Extract a fresh complete copy of the fix ZIP.')
    print('Checking the running bot...', flush=True)
    run(bundle, config, 'module', 'health')
    print('Installing the Windows run-ID fix...', flush=True)
    installed = run(bundle, config, 'module', 'install', '--source', str(package), '--trust-native')
    digest = installed['digest']
    print('Upgrading Dandy\'s World and preserving saved runs...', flush=True)
    database = Path(settings['database']['path'])
    if not database.is_absolute():
        database = root / database
    with sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True) as db:
        selected = db.execute('SELECT digest,loaded FROM oracle_module_desired WHERE module=?', (module,)).fetchone()
    if selected != (digest, 1):
        run(bundle, config, 'module', 'upgrade', '--module', module, '--digest', digest)
    print('Synchronizing Discord commands...', flush=True)
    run(bundle, config, 'publish-commands')
    health = run(bundle, config, 'module', 'health').get(module, {})
    guild = settings['guilds'][0]['guild']
    if not health.get('guilds', {}).get(guild, {}).get('active'):
        raise RuntimeError('The upgraded module is not active. Send this message.')
    # Keep both first-start copies consistent with the successfully installed artifact.
    for destination in (root / 'module-package', bundle / 'payload' / 'module-package'):
        destination.mkdir(parents=True, exist_ok=True)
        for name in ('package.json', 'dw-module.exe'):
            shutil.copyfile(package / name, destination / name)
    (root / 'module-ready').write_text(digest, encoding='utf-8')
    print('FIX APPLIED. Try /hostrun casual in #planned-runs. Keep the bot window open.', flush=True)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, sqlite3.Error) as error:
        # OS exceptions can contain local paths, but never credential file contents.
        print('Fix stopped: ' + (str(error) if isinstance(error, RuntimeError) else type(error).__name__))
        raise SystemExit(1)
