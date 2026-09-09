#!/usr/bin/env python3
"""Create the editable exact-base fork; refuse to replace unexpected existing edits."""
import pathlib
import subprocess
ROOT = pathlib.Path(__file__).resolve().parent.parent
FORK = ROOT / 'target' / 'serenity-fork'
REV = '98ec74223b0ff77fc4e8085d25569ea59e09a36f'
PATCH = ROOT / 'patches' / 'serenity-preserve-unknown-dispatch.patch'
def git(*args):
    return subprocess.check_output(['git', '-C', str(FORK), *args])
if not FORK.exists():
    FORK.mkdir(parents=True)
    git('init', '-q')
    git('remote', 'add', 'upstream', 'https://github.com/serenity-rs/serenity.git')
    git('fetch', '--depth=1', 'upstream', REV)
    git('checkout', '-b', 'oracle-next', REV)
    git('apply', '--check', str(PATCH))
    git('apply', str(PATCH))
if git('rev-parse', 'HEAD').decode().strip() != REV:
    raise SystemExit('Editable fork has an unexpected base; refusing to overwrite it')
if git('diff', '--abbrev=8').replace(b'\r\n', b'\n') != PATCH.read_bytes().replace(b'\r\n', b'\n'):
    raise SystemExit('Editable fork differs from recorded patch; preserve and review local changes')
print('Editable Serenity fork verified:', REV)
