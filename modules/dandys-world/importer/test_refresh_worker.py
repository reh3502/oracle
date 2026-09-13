"""Offline refresh-worker composition, resource, and disk-boundary tests."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import refresh_worker as worker
from refresh_source import SourceDenied, SourceError, SourceRetry, acquire
from test_refresh_source import Wiki, Clock
from wiki_source import millis


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def stub_acquire(self, source, previous=None, budget_bytes=None):
        self.assertGreater(budget_bytes, 0)
        self.assertLessEqual(budget_bytes, worker.MAX_FILE)
        source.mkdir()
        (source / 'data').write_bytes(b'x')

    def test_actual_tiny_corpus_is_normalized_with_original_provenance(self):
        pages = [
            ('Art Gallery', 'A named playable floor. [[Category:Floors]]'),
            ('Example Toon', '{{Toons|heart1=Heart|ability_1=Example effect}}'),
            ('Twisted Example Toon', '{{Twisted|mechanic=Example counterpart}}'),
            ('Example Trinket', '{{Trinket|effect=Example effect}}'),
            ('Example Person', 'Example person. [[Category:Humans]]'),
            ('Example Event', '{{Infobox_Event|start_date=Unknown}}'),
            ('Health', 'Example mechanic.'),
            ('Machines', '==Mechanics==\n===Default Machine===\nExample machine.'),
            ('Floors', '==List of Floors==\n{|\n! colspan="2" | Name !! Image !! Variants !! Requirements\n|-\n| colspan="2" | [[Example Floor]]\n| Image || One || Example restriction\n|}'),
            ('Items', '==List of Items==\n{|\n! Item !! Rarity !! Effect !! Category !! Shop !! Normal !! Plush !! Frugal !! Both\n|-\n| {{II|Example Item}} || Rare || Example effect || Example category || No || 10 || 5 || 9 || 4\n|}'),
            ('Example Item', '#REDIRECT [[Items#Example Item]]'),
        ]
        wiki = Wiki()
        wiki.pages = [{'pageid': index + 1, 'ns': 0, 'title': title, 'raw': raw, 'revision': index + 100} for index, (title, raw) in enumerate(pages)]
        def offline(source, previous=None, budget_bytes=None):
            return acquire(source, previous, wiki.client(), budget_bytes)
        result = worker.run(self.root / 'complete', budget_bytes=1024 * 1024, acquirer=offline)
        catalog = json.loads(Path(result['candidate']).read_bytes())
        self.assertEqual(set(catalog['coverage']['entities_by_kind']), {'toon', 'twisted', 'trinket', 'npc', 'event', 'mechanic', 'floor', 'item', 'machine'})
        corpus = json.loads((Path(result['corpus']) / 'catalog.json').read_bytes())
        self.assertEqual(catalog['sources'][0]['revision_id'], corpus[0]['revision_id'])
        self.assertEqual(catalog['crawl_completed_at'], '2026-09-13T00:00:00+00:00')
        self.assertEqual(result['output_bytes'], worker.regular_bytes(self.root / 'complete'))
        self.assertFalse((self.root / 'complete/active').exists())

    def test_failed_acquisition_and_denial_never_create_candidate(self):
        for index, failure in enumerate([SourceError('offline failure'), SourceDenied('denied')]):
            path = self.root / str(index)
            def failed(*args, **kwargs):
                raise failure
            with self.assertRaises(type(failure)):
                worker.run(path, acquirer=failed)
            self.assertFalse((path / 'candidate.json').exists())
            self.assertFalse((path / '.candidate.tmp').exists())

    def test_failed_normalization_keeps_source_but_no_candidate(self):
        def failed(source):
            raise SourceError('normalization failed')
        with self.assertRaises(SourceError):
            worker.run(self.root / 'failed', acquirer=self.stub_acquire, normalizer=failed)
        self.assertEqual((self.root / 'failed/source/data').read_bytes(), b'x')
        self.assertFalse((self.root / 'failed/candidate.json').exists())

    def test_budget_is_checked_before_each_candidate_write(self):
        value = {'text': '😀' * 20}
        expected = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
        result = worker.run(self.root / 'exact', budget_bytes=worker.RESULT_RESERVE + 1 + len(expected), acquirer=self.stub_acquire, normalizer=lambda _: value)
        self.assertEqual(Path(result['candidate']).read_bytes(), expected)
        self.assertEqual(result['output_bytes'], 1 + len(expected))
        with self.assertRaisesRegex(SourceError, 'remaining disk budget'):
            worker.run(self.root / 'short', budget_bytes=worker.RESULT_RESERVE + len(expected), acquirer=self.stub_acquire, normalizer=lambda _: value)
        self.assertFalse((self.root / 'short/candidate.json').exists())
        self.assertFalse((self.root / 'short/.candidate.tmp').exists())
        self.assertLessEqual(worker.regular_bytes(self.root / 'short'), len(expected))

    def test_candidate_individual_file_cap_and_combined_output_cap(self):
        with patch('refresh_worker.MAX_FILE', 10):
            with self.assertRaises(SourceError):
                worker.run(self.root / 'large', budget_bytes=1000, acquirer=self.stub_acquire, normalizer=lambda _: 'x' * 20)
        with patch('refresh_worker.MAX_OUTPUT', 15):
            with self.assertRaises(SourceError):
                worker.run(self.root / 'combined', budget_bytes=1000, acquirer=self.stub_acquire, normalizer=lambda _: 'x' * 20)

    def test_existing_and_previous_paths_are_never_modified(self):
        existing = self.root / 'previous'
        existing.mkdir()
        sentinel = existing / 'sentinel'; sentinel.write_text('unchanged')
        for target in (existing, existing / 'nested'):
            with self.assertRaises(SourceError):
                worker.run(target, previous=existing, acquirer=self.stub_acquire)
        with self.assertRaises(FileExistsError):
            worker.run(existing, acquirer=self.stub_acquire)
        self.assertEqual(sentinel.read_text(), 'unchanged')
        self.assertFalse((existing / 'nested').exists())
        with self.assertRaises(SourceError):
            worker.run(Path('relative'), acquirer=self.stub_acquire)
        with self.assertRaises(SourceError):
            worker.run(self.root / 'bad-budget', budget_bytes=0, acquirer=self.stub_acquire)

    def test_source_output_symlink_is_rejected_before_normalization(self):
        target = self.root / 'existing'; target.write_text('unchanged')
        def links(source, **kwargs):
            source.mkdir(); (source / 'escape').symlink_to(target)
        with self.assertRaises(SourceError):
            worker.run(self.root / 'failed', acquirer=links, normalizer=lambda _: {})
        self.assertEqual(target.read_text(), 'unchanged')

    def test_cli_codes_are_fixed_and_redacted(self):
        for failure, code in [(SourceDenied('secret remote body'), 3), (SourceError('secret remote body'), 2)]:
            stderr = io.StringIO()
            with patch('sys.argv', ['refresh_worker.py', '--output', str(self.root / 'output'), '--budget-bytes', '1000000']), patch.object(worker, 'apply_limits'), patch.object(worker, 'wall_deadline', return_value=contextlib.nullcontext()), patch.object(worker, 'run', side_effect=failure), contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as exited:
                    worker.main()
            self.assertEqual(exited.exception.code, code)
            self.assertNotIn('secret remote', stderr.getvalue())

    def child(self, code):
        # Child resource/deadline tests cannot alter the unittest runner's limits.
        return subprocess.run([sys.executable, '-c', code], cwd=Path(worker.__file__).parent,
                              capture_output=True, timeout=5, check=False)

    def test_retry_after_beyond_job_deadline_is_persisted_within_reserved_budget(self):
        wiki = Wiki()
        wiki.change = lambda *args: (429, {'Retry-After': '3600'}, b'')
        def offline(source, previous=None, budget_bytes=None):
            return acquire(source, previous, wiki.client(), budget_bytes)
        output = self.root / 'retry'
        with self.assertRaises(SourceRetry):
            worker.run(output, budget_bytes=worker.RESULT_RESERVE + 1, acquirer=offline)
        result = json.loads((output / 'result.json').read_bytes())
        self.assertEqual(result, {'status': 'retry', 'retry_not_before_ms': millis(Clock().wall()) + 3600000})
        self.assertLessEqual(worker.regular_bytes(output), worker.RESULT_RESERVE + 1)
        self.assertFalse((output / 'candidate.json').exists())
        self.assertFalse((output / 'source/manifest.json').exists())

    def test_lost_retry_metadata_stops_instead_of_shortening_retry_delay(self):
        def transient(*args, **kwargs):
            raise SourceRetry('retry later', millis(Clock().wall()) + 3600000)
        original = Path.open
        def fail_result(path, *args, **kwargs):
            if path.name == 'result.json':
                raise OSError('disk full')
            return original(path, *args, **kwargs)
        with patch.object(Path, 'open', fail_result), self.assertRaises(SourceDenied):
            worker.run(self.root / 'diskfull', budget_bytes=10000, acquirer=transient)
        self.assertFalse((self.root / 'diskfull/candidate.json').exists())

    def test_unrepresentable_retry_after_persists_explicit_stop(self):
        wiki = Wiki()
        wiki.change = lambda *args: (429, {'Retry-After': '1e100'}, b'')
        def offline(source, previous=None, budget_bytes=None):
            return acquire(source, previous, wiki.client(), budget_bytes)
        output = self.root / 'stopped'
        with self.assertRaises(SourceDenied):
            worker.run(output, budget_bytes=10000, acquirer=offline)
        self.assertEqual(json.loads((output / 'result.json').read_bytes()),
                         {'status': 'stopped', 'reason': 'retry_deadline_out_of_range'})

    def test_real_child_installs_memory_cpu_and_file_limits(self):
        result = self.child('import refresh_worker as w, resource, json; w.apply_limits(); print(json.dumps([resource.getrlimit(k) for k in [resource.RLIMIT_AS,resource.RLIMIT_CPU,resource.RLIMIT_FSIZE]]))')
        self.assertEqual(result.returncode, 0, result.stderr)
        limits = json.loads(result.stdout)
        for pair, maximum in zip(limits, [worker.MAX_MEMORY, worker.MAX_SECONDS, worker.MAX_FILE]):
            self.assertLessEqual(pair[0], maximum)
            self.assertLessEqual(pair[1], maximum)

    def test_real_child_wall_deadline_terminates_work_before_candidate(self):
        marker = self.root / 'candidate.json'
        code = 'import refresh_worker as w, time; from pathlib import Path\nwith w.wall_deadline(0.05):\n time.sleep(1)\n Path(' + repr(str(marker)) + ').write_text("late")\n'
        result = self.child(code)
        self.assertEqual(result.returncode, 124)
        self.assertFalse(marker.exists())

    def test_real_cli_rejects_relative_output_before_any_acquisition(self):
        result = subprocess.run([sys.executable, str(Path(worker.__file__).resolve()), '--output', 'relative', '--budget-bytes', '100000'],
                                cwd=self.root, capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.root / 'relative').exists())
        self.assertLess(len(result.stderr), 300)


if __name__ == '__main__':
    unittest.main()
