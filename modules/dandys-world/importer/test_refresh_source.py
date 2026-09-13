"""All acquisition tests are offline; no requests reach Fandom."""
import hashlib
import http.client
import json
import socket
import tempfile
import time
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from refresh_source import API, Client, NAMESPACES, SourceDenied, SourceError, acquire, network, resolve_addresses, main, retry_after
from wiki_source import Corpus, MAX_PAGE, ORIGIN


class Clock:
    def __init__(self):
        self.value = 0
        self.delays = []
    def monotonic(self):
        return self.value
    def sleep(self, delay):
        self.delays.append(delay)
        self.value += delay
    def wall(self):
        return '2026-09-13T00:00:00+00:00'


class Wiki:
    def __init__(self):
        self.pages = [{'pageid': i + 1, 'ns': int(ns), 'title': ('Page' if ns == '0' else label + ':Page'),
                       'raw': 'fixture ' + label, 'revision': i + 10} for i, (ns, label) in enumerate(NAMESPACES.items())]
        self.calls = []
        self.change = None
        self.http_status = 200
    def transport(self, url, timeout):
        assert url.startswith(API + '?') and 0 < timeout <= 15
        q = {k: v[0] for k, v in parse_qs(urlsplit(url).query).items()}
        self.calls.append(q)
        if self.change:
            replacement = self.change(self, q)
            if replacement is not None:
                return replacement
        if self.http_status != 200:
            return self.http_status, {}, b''
        if q.get('meta') == 'siteinfo':
            value = {'query': {'general': {'server': ORIGIN}, 'rightsinfo': {'text': 'CC-BY-SA', 'url': 'https://www.fandom.com/licensing'},
                               'namespaces': {ns: {'id': int(ns)} for ns in NAMESPACES}}}
        elif q.get('list') == 'allpages':
            value = {'batchcomplete': True, 'query': {'allpages': [{k: p[k] for k in ('pageid', 'ns', 'title')} for p in self.pages if p['ns'] == int(q['apnamespace'])]}}
        else:
            pages = []
            for pid in map(int, q['pageids'].split('|')):
                p = next((p for p in self.pages if p['pageid'] == pid), None)
                if p is None:
                    pages.append({'pageid': pid, 'missing': True}); continue
                slot = {'contentmodel': 'wikitext'}
                if '|content' in q['rvprop']:
                    # contentmodel also begins with content; distinguish full field.
                    if 'content' in q['rvprop'].split('|'):
                        slot['content'] = p['raw']
                pages.append({**{k: p[k] for k in ('pageid', 'ns', 'title')}, 'revisions': [{'revid': p['revision'], 'parentid': 1,
                              'timestamp': '2026-09-12T00:00:00Z', 'sha1': hashlib.sha1(p['raw'].encode()).hexdigest(),
                              'size': len(p['raw'].encode()), 'slots': {'main': slot}}]})
            value = {'batchcomplete': True, 'query': {'pages': pages}}
        return 200, {'Content-Type': 'application/json'}, json.dumps(value).encode()
    def client(self, clock=None):
        clock = clock or Clock()
        return Client(self.transport, clock.monotonic, clock.sleep, clock.wall)
    def body_calls(self):
        return sum('content' in q.get('rvprop', '').split('|') for q in self.calls)


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.TemporaryDirectory()
        self.addCleanup(self.root.cleanup)
        self.base = Path(self.root.name)
    def test_complete_corpus_and_exact_revision_cache_reuse(self):
        wiki = Wiki()
        first = acquire(self.base / 'first', client=wiki.client())
        self.assertEqual(first['pages'], 5)
        self.assertEqual(wiki.body_calls(), 5)
        self.assertEqual(set(Corpus.open(self.base / 'first').namespace_counts), set(NAMESPACES.values()))
        before = (self.base / 'first' / 'manifest.json').read_bytes()
        wiki.calls.clear()
        second = acquire(self.base / 'second', previous=self.base / 'first', client=wiki.client())
        self.assertEqual(second['acquisition']['reused_pages'], 5)
        self.assertEqual(wiki.body_calls(), 0)
        self.assertEqual((self.base / 'first' / 'manifest.json').read_bytes(), before)
    def test_rename_deletion_and_new_revision_between_jobs_are_supported(self):
        wiki = Wiki()
        acquire(self.base / 'first', client=wiki.client())
        wiki.pages[0]['title'] = 'Renamed'
        wiki.pages[1]['raw'] = 'changed'; wiki.pages[1]['revision'] += 100
        wiki.pages.pop()
        wiki.calls.clear()
        result = acquire(self.base / 'next', previous=self.base / 'first', client=wiki.client())
        self.assertEqual(result['pages'], 4)
        self.assertEqual(wiki.body_calls(), 2)
        self.assertIn('Renamed', Corpus.open(self.base / 'next').by_title)
    def test_previous_corruption_is_rejected_without_network_or_output(self):
        wiki = Wiki()
        acquire(self.base / 'first', client=wiki.client())
        (self.base / 'first' / 'articles/1.txt').write_text('corruption')
        wiki.calls.clear()
        with self.assertRaises(ValueError):
            acquire(self.base / 'next', previous=self.base / 'first', client=wiki.client())
        self.assertFalse(wiki.calls)
        self.assertFalse((self.base / 'next').exists())
    def test_existing_output_never_overwritten(self):
        wiki = Wiki()
        acquire(self.base / 'first', client=wiki.client())
        with self.assertRaises(FileExistsError):
            acquire(self.base / 'first', client=wiki.client())
        with self.assertRaises(SourceError):
            acquire(self.base / 'first/nested', previous=self.base / 'first', client=wiki.client())
        self.assertFalse((self.base / 'first/nested').exists())
    def test_revision_change_during_final_check_leaves_no_complete_manifest(self):
        wiki = Wiki()
        counter = 0
        def change(w, q):
            nonlocal counter
            if q.get('prop') == 'revisions' and 'content' not in q['rvprop'].split('|'):
                counter += 1
                if counter == 2:
                    w.pages[0]['revision'] += 1
        wiki.change = change
        with self.assertRaisesRegex(SourceError, 'final validation'):
            acquire(self.base / 'failed', client=wiki.client())
        self.assertFalse((self.base / 'failed/manifest.json').exists())
    def test_rename_or_delete_mid_crawl_fails_identity_check(self):
        for deletion in (False, True):
            wiki = Wiki()
            def change(w, q):
                if q.get('prop') == 'revisions':
                    if deletion: w.pages.pop(0)
                    else: w.pages[0]['title'] = 'Moved'
                    w.change = None
            wiki.change = change
            with self.assertRaises(SourceError):
                acquire(self.base / str(deletion), client=wiki.client())
            self.assertFalse((self.base / str(deletion) / 'manifest.json').exists())
    def test_discovery_change_between_enumerations_aborts(self):
        wiki = Wiki()
        seen = 0
        def change(w, q):
            nonlocal seen
            if q.get('list') == 'allpages' and q['apnamespace'] == '0':
                seen += 1
                if seen == 2:
                    w.pages.append({'pageid': 200, 'ns': 0, 'title': 'New', 'raw': 'new', 'revision': 999})
        wiki.change = change
        with self.assertRaisesRegex(SourceError, 'discovery changed'):
            acquire(self.base / 'failed', client=wiki.client())
    def test_malformed_pagination_is_rejected(self):
        for continuation in ({'apcontinue': 'x'}, {'continue': '-||', 'apcontinue': ''}, {'continue': '-||', 'apcontinue': 'x', 'url': 'evil'}):
            wiki = Wiki()
            wiki.change = lambda w, q: (200, {}, json.dumps({'query': {'allpages': []}, 'continue': continuation}).encode()) if q.get('list') else None
            with self.assertRaises(SourceError):
                acquire(self.base / str(len(list(self.base.iterdir()))), client=wiki.client())
    def test_http_auth_challenge_redirect_and_304_never_retry(self):
        for status in (401, 403, 301, 302, 304):
            wiki = Wiki(); wiki.http_status = status
            with self.assertRaises(SourceError):
                acquire(self.base / str(status), client=wiki.client())
            self.assertEqual(len(wiki.calls), 1)
        clock = Clock()
        client = Client(lambda *args: (200, {'Content-Type': 'text/html'}, b'<html>challenge</html>'), clock.monotonic, clock.sleep, clock.wall)
        with self.assertRaisesRegex(SourceError, 'challenge'):
            client.get(meta='siteinfo')
    def test_rate_limit_retry_after_and_bounded_transient_retries(self):
        clock = Clock(); responses = [(429, {'Retry-After': '3'}, b''), (503, {}, b''), (200, {}, b'{}')]
        client = Client(lambda *args: responses.pop(0), clock.monotonic, clock.sleep, clock.wall)
        self.assertEqual(client.get()[0], {})
        self.assertGreaterEqual(clock.delays[0], 3)
        self.assertEqual(clock.delays[1], 2)
        client = Client(lambda *args: (429, {}, b''), clock.monotonic, clock.sleep, clock.wall)
        with self.assertRaisesRegex(SourceError, 'retry limit'): client.get()
        self.assertEqual(client.requests, 3)
    def test_retry_budget_and_request_deadline(self):
        clock = Clock()
        client = Client(lambda *args: (429, {'Retry-After': '901'}, b''), clock.monotonic, clock.sleep, clock.wall)
        with self.assertRaisesRegex(SourceError, 'deadline'): client.get()
        self.assertFalse(clock.delays)
        def slow(*args): clock.value += 15; return 200, {}, b'{}'
        with self.assertRaisesRegex(SourceError, 'deadline'):
            Client(slow, clock.monotonic, clock.sleep, clock.wall).get()
    def test_response_and_content_limits(self):
        clock = Clock()
        with patch('refresh_source.MAX_RESPONSE', 3):
            with self.assertRaisesRegex(SourceError, 'Oversized'):
                Client(lambda *args: (200, {}, b'1234'), clock.monotonic, clock.sleep, clock.wall).get()
        wiki = Wiki(); wiki.pages[0]['raw'] = 'x' * (MAX_PAGE + 1)
        with self.assertRaises(SourceError): acquire(self.base / 'huge', client=wiki.client())
        with patch('refresh_source.MAX_PAGES', 4):
            with self.assertRaisesRegex(SourceError, 'Page count'):
                acquire(self.base / 'many', client=Wiki().client())
        with patch('refresh_source.MAX_CORPUS', 1):
            with self.assertRaisesRegex(SourceError, 'Corpus byte'):
                acquire(self.base / 'total', client=Wiki().client())
    def test_duplicate_json_and_api_errors_fail_closed(self):
        for body in (b'{"query":{},"query":{}}', b'{"error":{}}', b'{"warnings":{}}', b'NaN', b'<html>'):
            clock = Clock()
            with self.assertRaises(SourceError):
                Client(lambda *args: (200, {}, body), clock.monotonic, clock.sleep, clock.wall).get()
    def test_production_transport_rejects_noncanonical_endpoint_before_io(self):
        for url in ('http://example.org/api.php', 'https://evil.example/api.php', ORIGIN + '/other', API + '#x'):
            with self.assertRaises(SourceError): network(url, 1)
    def test_final_validation_time_changes_only_successful_new_corpus(self):
        wiki = Wiki()
        acquire(self.base / 'first', client=wiki.client())
        before = (self.base / 'first/catalog.json').read_bytes()
        clock = Clock()
        clock.wall = lambda: '2026-09-13T01:00:00+00:00'
        acquire(self.base / 'second', previous=self.base / 'first', client=wiki.client(clock))
        second = json.loads((self.base / 'second/catalog.json').read_text())
        self.assertTrue(all(row['retrieved_at'] == clock.wall() for row in second))
        self.assertEqual((self.base / 'first/catalog.json').read_bytes(), before)
        wiki.http_status = 304
        with self.assertRaises(SourceError):
            acquire(self.base / 'failure', previous=self.base / 'second', client=wiki.client(clock))
        self.assertFalse((self.base / 'failure/manifest.json').exists())
        self.assertEqual((self.base / 'first/catalog.json').read_bytes(), before)

    def test_same_revision_cache_body_must_match_current_metadata_hash(self):
        wiki = Wiki()
        acquire(self.base / 'first', client=wiki.client())
        wiki.pages[0]['raw'] = 'different bytes despite same revision number'
        wiki.calls.clear()
        result = acquire(self.base / 'second', previous=self.base / 'first', client=wiki.client())
        self.assertEqual(result['acquisition']['downloaded_pages'], 1)
        self.assertEqual(wiki.body_calls(), 1)

    def test_body_corruption_rejects_the_entire_candidate(self):
        wiki = Wiki()
        original = wiki.transport
        def corrupt(url, timeout):
            status, headers, raw = original(url, timeout)
            if 'content' in wiki.calls[-1].get('rvprop', '').split('|'):
                value = json.loads(raw)
                value['query']['pages'][0]['revisions'][0]['slots']['main']['content'] = 'corrupt'
                raw = json.dumps(value).encode()
            return status, headers, raw
        clock = Clock()
        with self.assertRaisesRegex(SourceError, 'body size/hash'):
            acquire(self.base / 'failure', client=Client(corrupt, clock.monotonic, clock.sleep, clock.wall))
        self.assertFalse((self.base / 'failure/manifest.json').exists())

    def test_physical_output_budget_counts_json_duplicates_and_metadata(self):
        # Five tiny text bodies fit 1KiB; their serialized records do not.
        with patch('refresh_source.MAX_CORPUS', 1024):
            with self.assertRaisesRegex(SourceError, 'Physical output'):
                acquire(self.base / 'failure', client=Wiki().client())
        self.assertFalse((self.base / 'failure/manifest.json').exists())
        self.assertLessEqual(sum(p.stat().st_size for p in (self.base / 'failure').rglob('*') if p.is_file()), 1024)

    def test_pagination_repeat_and_missing_completion_rejected(self):
        for repeat in (False, True):
            wiki = Wiki()
            def invalid(w, q):
                if q.get('list'):
                    value = {'query': {'allpages': [{'pageid': 80 + len(w.calls), 'ns': int(q['apnamespace']), 'title': 'Unique' + str(len(w.calls))}]}}
                    if repeat: value['continue'] = {'continue': '-||', 'apcontinue': 'same'}
                    return 200, {}, json.dumps(value).encode()
            wiki.change = invalid
            with self.assertRaises(SourceError):
                acquire(self.base / str(repeat), client=wiki.client())

    def test_site_identity_and_license_drift_fail_before_page_fetch(self):
        for field, changed in [('general', {'server': 'https://other.example'}), ('rightsinfo', {'text': 'All rights reserved'}), ('namespaces', {})]:
            wiki = Wiki()
            original = wiki.transport
            def drift(url, timeout):
                status, headers, raw = original(url, timeout)
                value = json.loads(raw)
                value['query'][field] = changed
                return status, headers, json.dumps(value).encode()
            clock = Clock()
            with self.assertRaises(SourceError):
                acquire(self.base / field, client=Client(drift, clock.monotonic, clock.sleep, clock.wall))
            self.assertEqual(len(wiki.calls), 1)

    def test_real_socket_trickle_cannot_extend_total_request_deadline(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200); self.send_header('Content-Length', '100'); self.end_headers()
                for _ in range(100):
                    try:
                        self.wfile.write(b' '); self.wfile.flush()
                    except (OSError, BrokenPipeError):
                        break
                    time.sleep(0.02)
            def log_message(self, *args): pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try:
            # Exercise the production response reader, only substituting its
            # socket destination with an ordinary controlled local HTTP server.
            with patch('refresh_source.resolve_addresses', return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, '', server.server_address)]), patch('refresh_source.http.client.HTTPSConnection', side_effect=lambda host, timeout: http.client.HTTPConnection(*server.server_address, timeout=timeout)):
                started = time.monotonic()
                with self.assertRaises((TimeoutError, OSError)):
                    network(API + '?format=json', 0.12)
                self.assertLess(time.monotonic() - started, 0.5)
        finally:
            server.shutdown(); server.server_close(); thread.join()

    def test_real_socket_header_trickle_cannot_extend_request_deadline(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                for byte in b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}':
                    try:
                        self.wfile.write(bytes([byte])); self.wfile.flush()
                    except OSError:
                        break
                    time.sleep(0.02)
            def log_message(self, *args): pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try:
            with patch('refresh_source.resolve_addresses', return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, '', server.server_address)]), patch('refresh_source.http.client.HTTPSConnection', side_effect=lambda host, timeout: http.client.HTTPConnection(*server.server_address, timeout=timeout)):
                started = time.monotonic()
                with self.assertRaises((TimeoutError, OSError, http.client.HTTPException)):
                    network(API + '?format=json', 0.12)
                self.assertLess(time.monotonic() - started, 0.5)
        finally:
            server.shutdown(); server.server_close(); thread.join()

    def test_stalled_dns_resolver_obeys_absolute_deadline_without_late_http(self):
        release = threading.Event()
        entered = threading.Event()
        def stalled(*args, **kwargs):
            entered.set()
            release.wait(1)
            return [(socket.AF_INET, socket.SOCK_STREAM, 6, '', ('127.0.0.1', 9))]
        try:
            with patch('refresh_source.socket.getaddrinfo', side_effect=stalled), patch('refresh_source.http.client.HTTPSConnection') as connection:
                started = time.monotonic()
                with self.assertRaisesRegex(TimeoutError, 'DNS deadline'):
                    network(API + '?format=json', 0.05)
                self.assertTrue(entered.is_set())
                self.assertLess(time.monotonic() - started, 0.3)
                connection.assert_not_called()
                release.set()
                time.sleep(0.02)
                connection.assert_not_called()
        finally:
            release.set()

    def test_stalled_dns_threads_are_globally_bounded_to_three(self):
        release = threading.Event()
        def stalled(*args, **kwargs):
            release.wait(1)
            return []
        try:
            with patch('refresh_source.socket.getaddrinfo', side_effect=stalled):
                for _ in range(3):
                    with self.assertRaises(TimeoutError):
                        resolve_addresses('fixture.invalid', time.monotonic() + 0.01)
                with self.assertRaisesRegex(SourceError, 'capacity'):
                    resolve_addresses('fixture.invalid', time.monotonic() + 0.01)
        finally:
            release.set()
            time.sleep(0.03)

    def test_retry_after_never_shortens_local_backoff_and_all_5xx_retry(self):
        clock = Clock()
        responses = [(429, {'Retry-After': '0'}, b''), (599, {'Retry-After': '0.25'}, b''), (200, {}, b'{}')]
        client = Client(lambda *args: responses.pop(0), clock.monotonic, clock.sleep, clock.wall)
        client.get()
        self.assertEqual(clock.delays, [1, 2])

    def test_retry_floor_preserves_fractional_and_http_date_precision(self):
        from wiki_source import millis
        wall = Clock().wall()
        delay, floor = retry_after('0.0001', wall)
        self.assertGreaterEqual(delay, 0.0001)
        self.assertEqual(floor, millis(wall) + 1)
        self.assertEqual(retry_after('1e-99999999', wall)[1], millis(wall) + 1)
        self.assertEqual(retry_after('1000000000000000.0000000000000001', wall)[1], millis(wall) + 1000000000000000001)
        delay, floor = retry_after('Sun, 13 Sep 2026 01:00:00 GMT', wall)
        self.assertGreaterEqual(delay, 3600)
        self.assertEqual(floor, millis(wall) + 3600000)

    def test_permanent_denials_have_distinct_exception_and_no_retry(self):
        for status, headers, body in [(401, {}, b''), (403, {}, b''), (200, {'Content-Type': 'text/html'}, b'challenge'), (200, {}, b'<html>challenge</html>'), (200, {}, b'{"error":{"code":"readapidenied"}}')]:
            clock = Clock()
            client = Client(lambda *args: (status, headers, body), clock.monotonic, clock.sleep, clock.wall)
            with self.assertRaises(SourceDenied):
                client.get()
            self.assertEqual(client.requests, 1)
            self.assertFalse(clock.delays)

    def test_cli_access_denial_is_distinct_and_redacted(self):
        import contextlib
        import io
        for error, code in [(SourceDenied('remote secret body'), 3), (SourceError('remote secret body'), 2)]:
            stderr = io.StringIO()
            with patch('sys.argv', ['refresh_source.py', '--output', str(self.base / 'unused')]), patch('refresh_source.acquire', side_effect=error), contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as result:
                    main()
            self.assertEqual(result.exception.code, code)
            self.assertNotIn('remote secret', stderr.getvalue())

    def test_real_loopback_http_fixture_exercises_corpus_pipeline(self):
        wiki = Wiki()
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                status, headers, body = wiki.transport(ORIGIN + self.path, 15)
                self.send_response(status)
                for key, value in headers.items(): self.send_header(key, value)
                self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
            def log_message(self, *args): pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try:
            def transport(url, timeout):
                self.assertTrue(url.startswith(API + '?'))
                conn = http.client.HTTPConnection(*server.server_address, timeout=timeout)
                try:
                    parsed = urlsplit(url); conn.request('GET', parsed.path + '?' + parsed.query)
                    response = conn.getresponse(); return response.status, dict(response.getheaders()), response.read()
                finally: conn.close()
            clock = Clock()
            acquire(self.base / 'http', client=Client(transport, clock.monotonic, clock.sleep, clock.wall))
            self.assertEqual(len(Corpus.open(self.base / 'http').rows), 5)
        finally:
            server.shutdown(); server.server_close(); thread.join()


if __name__ == '__main__':
    unittest.main()
