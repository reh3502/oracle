#!/usr/bin/env python3
"""Verify a staged Windows release without displaying credentials or connecting Discord."""
import argparse
import hashlib
import json
import pathlib
import struct


def verify(root, private=False):
    root = root.resolve()
    paths = [p for p in root.rglob('*') if p.is_file()]
    for path in root.rglob('*'):
        if path.is_symlink():
            raise ValueError('Release contains a symlink')
    sums = json.loads((root / 'checksums.json').read_text())
    actual = {p.relative_to(root).as_posix() for p in paths if p.name not in ('.env', 'checksums.json')}
    if set(sums) != actual:
        raise ValueError('Checksum inventory differs from release contents')
    for name, expected in sums.items():
        relative = pathlib.PurePosixPath(name)
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('Unsafe checksum path')
        if hashlib.sha256((root / relative).read_bytes()).hexdigest() != expected:
            raise ValueError('Checksum mismatch: ' + name)
    for path in paths:
        if path.suffix.lower() in ('.sqlite', '.db', '.sqlite3') or path.name in ('host.lock', 'module-ready', 'commands-ready'):
            raise ValueError('Release contains deployment state')
    for name in ('Start Oracle.exe', 'oracle-host.exe', 'payload/module-package/dw-module.exe'):
        data = (root / name).read_bytes()
        offset = struct.unpack_from('<I', data, 60)[0]
        if data[:2] != b'MZ' or data[offset:offset + 6] != b'PE\0\0d\x86':
            raise ValueError('Expected a native Windows x64 executable: ' + name)
    config = json.loads((root / 'payload/oracle.json').read_text())
    if config.get('ai') is not None or config['discord']['token_env'] != 'DISCORD_TOKEN':
        raise ValueError('Recipient AI/credential configuration mismatch')
    if len(config['guilds']) != 1 or not config['guilds'][0]['operators']:
        raise ValueError('Recipient operator configuration missing')
    package = json.loads((root / 'payload/module-package/package.json').read_text())
    if package['manifest']['target'] != 'x86_64-pc-windows-gnu':
        raise ValueError('Module platform mismatch')
    for name, expected in package['files'].items():
        if hashlib.sha256((root / 'payload/module-package' / name).read_bytes()).hexdigest() != expected:
            raise ValueError('Module package digest mismatch')
    env = root / '.env'
    if private:
        tokens = [line.split('=', 1)[1].strip().strip('\"\'') for line in env.read_text(encoding='utf-8-sig').splitlines() if line.startswith('DISCORD_TOKEN=')]
        if len(tokens) != 1 or not tokens[0]:
            raise ValueError('Private release lacks its configured bot token')
    elif env.exists():
        raise ValueError('Unexpected credential in public/test package')
    print('Verified: Windows x64 executables, checksums, AI disabled, operator policy, module package, no database, credential presence only.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=pathlib.Path)
    parser.add_argument('--private', action='store_true')
    args = parser.parse_args()
    verify(args.directory, args.private)
