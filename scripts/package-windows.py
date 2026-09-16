#!/usr/bin/env python3
"""Stage a private Windows DW release from verified executables and a validated store.

Generated packages, credentials and database state must remain outside Git.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import struct
import subprocess
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent


def pe_x64(path):
    data = path.read_bytes()
    if len(data) < 64 or data[:2] != b'MZ':
        raise ValueError(f'Not a Windows executable: {path.name}')
    offset = struct.unpack_from('<I', data, 60)[0]
    if data[offset:offset+6] != b'PE\0\0d\x86':
        raise ValueError(f'Not an x64 Windows executable: {path.name}')


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n', encoding='utf-8')


def stage(args):
    for path in (args.host, args.module, args.launcher):
        pe_x64(path)
    for value in (args.guild, args.operator, args.channel):
        if not value.isdecimal() or not 1 <= int(value) < 2**64:
            raise ValueError('Discord IDs must be positive decimal snowflakes')
    output = args.output.resolve()
    if output.exists():
        raise ValueError('Output already exists; use a new release directory')
    # Never stage a token in a path Git would track.
    if args.env_file:
        result = subprocess.run(['git', '-C', str(ROOT), 'check-ignore', '--quiet', str(output)], check=False)
        if result.returncode != 0:
            raise ValueError('Private output must be inside an ignored directory')
        lines = args.env_file.read_text(encoding='utf-8-sig').splitlines()
        tokens = [line.split('=', 1)[1].strip().strip('\"\'') for line in lines if line.startswith('DISCORD_TOKEN=')]
        if len(tokens) != 1 or not tokens[0]:
            raise ValueError('.env needs exactly one nonempty DISCORD_TOKEN')
    active = (args.catalog / 'active').read_bytes()
    heads = json.loads(active) if active.startswith(b'{') else {'current': active.decode(), 'previous': None}
    snapshots = {}
    for digest in set(filter(None, (heads['current'], heads.get('previous')))):
        if len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
            raise ValueError('Invalid catalog pointer')
        data = (args.catalog / (digest + '.json')).read_bytes()
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError('Catalog checksum mismatch')
        snapshots[digest] = data
    output.mkdir(parents=True, mode=0o700)
    shutil.copy2(args.host, output / 'oracle-host.exe')
    shutil.copy2(args.launcher, output / 'Start Oracle.exe')
    for dll in args.dll:
        shutil.copy2(dll, output / dll.name)
    payload = output / 'payload'
    package = payload / 'module-package'
    package.mkdir(parents=True)
    shutil.copy2(args.module, package / 'dw-module.exe')
    manifest = json.loads((ROOT / 'modules/dandys-world/manifest.json').read_text())
    manifest['target'] = 'x86_64-pc-windows-gnu'
    files = {'dw-module.exe': hashlib.sha256(args.module.read_bytes()).hexdigest()}
    # Any runtime DLL needed by the module must travel in its hashed package too.
    for dll in args.dll:
        shutil.copy2(dll, package / dll.name)
        files[dll.name] = hashlib.sha256(dll.read_bytes()).hexdigest()
    revision = subprocess.check_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], text=True).strip()
    write_json(package / 'package.json', {'manifest': manifest, 'entrypoint': 'dw-module.exe', 'files': files,
        'source_revision': 'git:' + revision, 'toolchain': 'Rust x86_64-pc-windows-gnu', 'license': 'Private distribution'})
    if args.python_runtime:
        shutil.copytree(args.python_runtime, payload / 'python')
        worker = payload / 'refresh-worker'
        worker.mkdir()
        for name in ('refresh_worker.py', 'refresh_source.py', 'normalize.py', 'wikitext.py', 'wiki_source.py', 'media_source.py', 'source_reviews.json', 'ATTRIBUTION.md'):
            shutil.copy2(ROOT / 'modules/dandys-world/importer' / name, worker / name)
    catalog = payload / 'catalog'
    catalog.mkdir()
    (catalog / 'active').write_bytes(active)
    for digest, data in snapshots.items():
        (catalog / (digest + '.json')).write_bytes(data)
    if heads.get('previous'):
        (catalog / 'previous').write_text(heads['previous'])
    module = 'community.dandys-world'
    config = {'version': 1, 'state_dir': 'state', 'database': {'backend': 'sqlite', 'path': 'state/oracle.sqlite'},
        'guilds': [{'guild': args.guild, 'operators': [args.operator]}],
        'discord': {'token_env': 'DISCORD_TOKEN', 'intents': ['guilds']}, 'ai': None,
        'module_runtime': {module: {'data_directory': 'catalog',
            'citation_prefix': 'https://dandys-world-robloxhorror.fandom.com/index.php?oldid=',
            'image_prefix': 'https://static.wikia.nocookie.net/dandys-world-robloxhorror/images/'}},
        'member_reads': [{'guild': args.guild, 'module': module, 'policy': {'channels': [], 'roles': [], 'per_user_per_minute': 6, 'per_guild_per_minute': 60}}],
        'member_mutations': [{'guild': args.guild, 'module': module, 'policy': {'channels': [args.channel], 'permission_roles': {}, 'per_user_per_minute': 60, 'per_guild_per_minute': 200}}],
        'shared_card_destinations': [{'guild': args.guild, 'module': module, 'destination': 'runs', 'channel': args.channel}]}
    write_json(payload / 'oracle.json', config)
    if args.env_file:
        shutil.copyfile(args.env_file, output / '.env')
        (output / '.env').chmod(0o600)
    else:
        (output / '.env.example').write_text('DISCORD_TOKEN=\n')
    (output / 'READ ME.txt').write_text(
        'Oracle — Dandy\'s World\n\n'
        '1. Extract the entire ZIP to a folder. Do not run inside the ZIP.\n'
        '2. Double-click Start Oracle.exe. Press Start if it is stopped.\n'
        '3. Press Stop or close the window to stop the bot.\n\n'
        'AI is disabled. No Python, Rust, or .NET installation is required.\n'
        'Your saves live under %LOCALAPPDATA%\\OracleSister. Keep that folder when updating.\n'
        'The .env contains your private bot token. Keep this package private.\n'
        'The bot must already be invited to the configured Discord server.\n'
        'Windows may warn because this private release is unsigned.\n', encoding='utf-8')
    # Hash binaries and payload, deliberately excluding the credential file.
    write_json(output / 'checksums.json', {str(p.relative_to(output)).replace('\\', '/'): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted(output.rglob('*')) if p.is_file() and p.name != '.env'})
    if args.zip:
        archive = output.with_suffix('.zip')
        if archive.exists():
            raise ValueError('ZIP already exists')
        with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as z:
            for p in sorted(output.rglob('*')):
                if p.is_file():
                    z.write(p, pathlib.Path(output.name) / p.relative_to(output))
        archive.chmod(0o600)
    print('Staged private Windows release:', output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('host', 'module', 'launcher', 'catalog', 'output'):
        parser.add_argument('--' + name, type=pathlib.Path, required=True)
    for name in ('guild', 'operator', 'channel'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--python-runtime', type=pathlib.Path)
    parser.add_argument('--env-file', type=pathlib.Path)
    parser.add_argument('--dll', type=pathlib.Path, action='append', default=[])
    parser.add_argument('--zip', action='store_true')
    args = parser.parse_args()
    try:
        stage(args)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f'Packaging failed: {error}\n')


if __name__ == '__main__':
    main()
