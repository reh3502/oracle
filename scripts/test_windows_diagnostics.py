"""Check diagnostic data selection without reading credentials or changing saved state."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('diagnostics', Path(__file__).with_name('windows-diagnostics.py'))
diag = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diag)


class DiagnosticTests(unittest.TestCase):
    def test_read_only_allowlist_excludes_credentials_and_run_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'oracle-host.exe').write_bytes(b'test executable')
            (root / '.env').write_text('DISCORD_TOKEN=do-not-read')
            (root / 'oracle.json').write_text(json.dumps({
                'database': {'path': 'state.sqlite'}, 'ai': None, 'guilds': [],
                'discord': {'token': 'do-not-export'}, 'member_mutations': []}))
            dbpath = root / 'state.sqlite'
            with sqlite3.connect(dbpath) as db:
                db.execute('CREATE TABLE oracle_workflows (guild TEXT,kind TEXT,key TEXT,value TEXT)')
                for kind, key, value in [
                    ('command_binding', '1:hostrun', {'id': '42', 'definition': {'name': 'hostrun', 'extra': 'private-body'}, 'route': {'session': 's', 'generation': 1, 'epoch': 2}, 'pending': None}),
                    ('command_group', 'publication', {'active': True, 'affected': ['community.dandys-world'], 'extra': 'private-body'}),
                    ('shared_card', 'run', {'private': 'private-run'})]:
                    db.execute('INSERT INTO oracle_workflows VALUES (?,?,?,?)', ('123', kind, key, json.dumps(value)))
            before = dbpath.read_bytes()
            with patch.object(diag.subprocess, 'run', side_effect=OSError('private-error')):
                result = diag.collect(root, root)
            encoded = json.dumps(result)
            for secret in ('do-not-read', 'do-not-export', 'private-body', 'private-run', 'private-error'):
                self.assertNotIn(secret, encoded)
            self.assertEqual(result['commands'][0]['name'], 'hostrun')
            self.assertEqual(result['commands'][0]['route']['generation'], 1)
            self.assertEqual(len(result['commands']), 2)
            self.assertEqual(dbpath.read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
