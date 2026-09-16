#!/usr/bin/env python3
"""Prepare the pinned, portable Windows interpreter used by DW wiki refresh."""
import argparse
import hashlib
import io
import json
import pathlib
import tarfile
import urllib.request
import zipfile

PYTHON_VERSION = '3.13.15'
PYTHON_URL = f'https://www.python.org/ftp/python/{PYTHON_VERSION}/python-{PYTHON_VERSION}-embed-amd64.zip'
PYTHON_SHA256 = 'd1f04d990aee1253d8569e8e5104e30fa9f5fa830899f14843448872d936a2cf'
PARSER_VERSION = '0.7.2'
PARSER_SHA256 = 'f4193072e9ea93b9e88f772f60a02125c0602d32890d0bbdcb275ed58c8b3763'
PARSER_WHEEL_URL = 'https://files.pythonhosted.org/packages/e2/eb/09a2201943390f2491df5a2fc1fe9abc06b0115116bef799e11725c65ced/mwparserfromhell-0.7.2-cp313-cp313-win_amd64.whl'
PARSER_WHEEL_SHA256 = '52f193b59c1b6109b210ad85536ce3c569861c3bb9da7b1875618d8ba54c396f'


def download(url, digest=None):
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read(32 * 1024 * 1024 + 1)
    if len(data) > 32 * 1024 * 1024:
        raise ValueError('Download exceeds package size limit')
    if digest and hashlib.sha256(data).hexdigest() != digest:
        raise ValueError('Download checksum mismatch')
    return data


def prepare(output):
    if output.exists():
        raise ValueError('Output exists; choose a new directory')
    python = download(PYTHON_URL, PYTHON_SHA256)
    metadata = json.loads(download(f'https://pypi.org/pypi/mwparserfromhell/{PARSER_VERSION}/json'))
    source = next(item for item in metadata['urls'] if item['packagetype'] == 'sdist' and item['digests']['sha256'] == PARSER_SHA256)
    parser = download(source['url'], PARSER_SHA256)
    output.mkdir(parents=True)
    with zipfile.ZipFile(io.BytesIO(python)) as archive:
        for name in archive.namelist():
            if pathlib.PurePosixPath(name).name != name:
                raise ValueError('Unexpected nested interpreter payload')
        archive.extractall(output)
    prefix = f'mwparserfromhell-{PARSER_VERSION}/src/mwparserfromhell/'
    with tarfile.open(fileobj=io.BytesIO(parser), mode='r:gz') as archive:
        for item in archive:
            if not item.isfile():
                continue
            if item.name.startswith(prefix) and item.name.endswith('.py'):
                relative = pathlib.PurePosixPath(item.name[len(prefix):])
                if '..' in relative.parts:
                    raise ValueError('Unsafe source path')
                path = output / 'mwparserfromhell' / relative
            elif item.name == f'mwparserfromhell-{PARSER_VERSION}/LICENSE':
                path = output / 'mwparserfromhell-LICENSE.txt'
            else:
                continue
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(archive.extractfile(item).read())
    # Use the matching upstream C tokenizer to keep full-corpus refresh bounded.
    wheel = download(PARSER_WHEEL_URL, PARSER_WHEEL_SHA256)
    with zipfile.ZipFile(io.BytesIO(wheel)) as archive:
        name = 'mwparserfromhell/parser/_tokenizer.cp313-win_amd64.pyd'
        (output / name).write_bytes(archive.read(name))
    # Isolated path configuration: no system Python, user site, PYTHONPATH or pip.
    (output / 'python313._pth').write_text('python313.zip\n.\n../refresh-worker\n', encoding='ascii')
    (output / 'provenance.json').write_text(json.dumps({'python': {'version': PYTHON_VERSION, 'url': PYTHON_URL, 'sha256': PYTHON_SHA256}, 'mwparserfromhell': {'version': PARSER_VERSION, 'url': source['url'], 'sha256': PARSER_SHA256, 'implementation': 'Windows C tokenizer', 'wheel_url': PARSER_WHEEL_URL, 'wheel_sha256': PARSER_WHEEL_SHA256}}, indent=2) + '\n')
    print('Prepared portable Windows Python:', output)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    prepare(args.output)
