"""Bounded canonical MediaWiki acquisition for operator-enabled refresh.

Deployment must separately qualify source access before enabling the scheduler.
No arbitrary URL, credentials, browser impersonation, or challenge workaround.
"""
import argparse
import hashlib
import http.client
import json
import math
import queue
import socket
import threading
import time
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation, ROUND_CEILING, localcontext
from email.utils import parsedate_to_datetime
from pathlib import Path
from urllib.parse import quote, urlencode, urlsplit

from wiki_source import Corpus, ImportError, MAX_CORPUS, MAX_PAGE, ORIGIN, millis, sha, read as source_read

API = ORIGIN + '/api.php'
NAMESPACES = {'0': 'articles', '10': 'templates', '14': 'categories', '828': 'modules', '2900': 'maps'}
USER_AGENT = 'Oracle-DandysWorld-SourceAdapter/1.0 (bounded revision mirror)'
MAX_PAGES = 10000
JOB_SECONDS = 900
REQUEST_SECONDS = 15
MAX_RESPONSE = 4 * MAX_PAGE
_DNS_CAPACITY = threading.BoundedSemaphore(3)


class SourceError(ImportError):
    pass


class SourceDenied(SourceError):
    """Permanent access denial: callers must stop scheduled source requests."""
    pass


class SourceRetry(SourceError):
    def __init__(self, message, retry_not_before_ms):
        super().__init__(message)
        self.retry_not_before_ms = retry_not_before_ms


class SourceRetryOverflow(SourceDenied):
    reason = 'retry_deadline_out_of_range'


def retry_after(header, now):
    now_ms = millis(now)
    try:
        seconds = Decimal(header)
    except InvalidOperation:
        try:
            date = parsedate_to_datetime(header)
            if date.tzinfo is None:
                raise ValueError('Missing timezone')
            seconds = max(Decimal(0), Decimal(str(date.timestamp())) - Decimal(now_ms) / 1000)
        except (ValueError, TypeError, OverflowError) as error:
            raise SourceError('Invalid Retry-After') from error
    if not seconds.is_finite() or seconds < 0:
        raise SourceRetryOverflow('Unrepresentable Retry-After')
    if seconds > Decimal(2**64 - 1) / 1000:
        raise SourceRetryOverflow('Unrepresentable retry deadline')
    if 0 < seconds < Decimal('0.001'):
        delta_ms = 1
    else:
        with localcontext() as context:
            context.prec = max(32, len(seconds.as_tuple().digits) + 4)
            delta_ms = int((seconds * 1000).to_integral_value(rounding=ROUND_CEILING))
    floor = now_ms + delta_ms
    if floor > 2**64 - 1:
        raise SourceRetryOverflow('Unrepresentable retry deadline')
    return math.nextafter(float(seconds), math.inf), floor


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise SourceError('Duplicate JSON key')
        result[key] = value
    return result


def json_value(raw):
    try:
        return json.loads(raw, object_pairs_hook=unique_object,
                          parse_constant=lambda _: (_ for _ in ()).throw(SourceError('Nonfinite JSON')))
    except (ValueError, UnicodeError) as error:
        raise SourceError('Malformed API JSON') from error


def mapping(value):
    if not isinstance(value, dict):
        raise SourceError('Malformed API object')
    return value


def positive(value):
    return type(value) is int and 0 < value <= 9007199254740991


def utc():
    return datetime.now(timezone.utc).isoformat()


def resolve_addresses(host, deadline):
    """Bound DNS independently of OS resolver timeouts; never send HTTP here.

    A timed-out daemon may finish DNS later, but cannot connect or send a request.
    Client retries cap a failed acquisition at three unresolved worker threads;
    the owning acquisition process terminates them when it exits.
    """
    results = queue.Queue(maxsize=1)

    def resolve():
        try:
            results.put((True, socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)))
        except OSError as error:
            results.put((False, error))
        finally:
            _DNS_CAPACITY.release()

    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('DNS deadline')
    if not _DNS_CAPACITY.acquire(blocking=False):
        raise SourceError('DNS resolver capacity exhausted')
    try:
        threading.Thread(target=resolve, name='oracle-source-dns', daemon=True).start()
    except Exception:
        _DNS_CAPACITY.release()
        raise
    try:
        success, addresses = results.get(timeout=remaining)
    except queue.Empty as error:
        raise TimeoutError('DNS deadline') from error
    if not success:
        raise addresses
    return addresses


def connect_addresses(addresses, deadline):
    """Connect only previously resolved sockaddrs; no implicit second DNS call."""
    for family, socktype, protocol, _, address in addresses:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Connection deadline')
        sock = socket.socket(family, socktype, protocol)
        try:
            sock.settimeout(remaining)
            sock.connect(address)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Connection deadline')
            sock.settimeout(remaining)
            return sock
        except OSError:
            sock.close()
    raise OSError('No resolved address connected')


def network(url, timeout):
    """One HTTPS request with no redirect following and bounded response bytes."""
    parsed = urlsplit(url)
    if parsed.scheme != 'https' or parsed.netloc != urlsplit(ORIGIN).netloc or parsed.path != '/api.php' or parsed.fragment:
        raise SourceError('Noncanonical API endpoint')
    deadline = time.monotonic() + timeout
    addresses = resolve_addresses(parsed.netloc, deadline)
    connection = http.client.HTTPSConnection(parsed.netloc, timeout=max(0.001, deadline - time.monotonic()))
    # HTTPConnection's socket factory hook avoids socket.create_connection's
    # second getaddrinfo. HTTPSConnection still owns normal certificate checking
    # and original-host SNI; no hostname or TLS validation is substituted.
    connection._create_connection = lambda *args, **kwargs: connect_addresses(addresses, deadline)
    timer = None
    try:
        connection.connect()
        sock = connection.sock
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Request deadline')
        sock.settimeout(remaining)

        def abort(target=sock):
            # An absolute deadline must also interrupt HTTP header readline,
            # whose individual socket reads otherwise allow indefinite trickle.
            try:
                target.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()

        timer = threading.Timer(remaining, abort)
        timer.daemon = True
        timer.start()
        connection.request('GET', parsed.path + '?' + parsed.query,
                           headers={'User-Agent': USER_AGENT, 'Accept': 'application/json', 'Accept-Encoding': 'identity'})
        sock = connection.sock
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Request deadline')
        if sock is not None:
            sock.settimeout(remaining)
        response = connection.getresponse()
        if response.status != 200 or ('Content-Type' in response.headers and 'application/json' not in response.headers['Content-Type'].lower()):
            return response.status, dict(response.getheaders()), b''
        chunks, size = [], 0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Request deadline')
            if sock is not None:
                sock.settimeout(remaining)
            chunk = response.read1(min(65536, MAX_RESPONSE + 1 - size))
            if time.monotonic() >= deadline:
                raise TimeoutError('Request deadline')
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_RESPONSE:
                raise SourceError('Oversized API response')
            chunks.append(chunk)
        return response.status, dict(response.getheaders()), b''.join(chunks)
    finally:
        if timer is not None:
            timer.cancel()
        connection.close()


class Client:
    def __init__(self, transport=None, monotonic=time.monotonic, sleep=time.sleep, wall=utc):
        # Injection is a library test seam; the CLI never accepts another origin.
        self.transport = transport or network
        self.clock, self.sleep, self.wall = monotonic, sleep, wall
        self.started = self.clock()
        self.requests = 0

    def remaining(self):
        left = JOB_SECONDS - (self.clock() - self.started)
        if left <= 0:
            raise SourceError('Acquisition job deadline')
        return left

    def wait(self, seconds):
        if not math.isfinite(seconds) or seconds < 0 or seconds >= self.remaining():
            raise SourceError('Retry exceeds acquisition deadline')
        self.sleep(seconds)
        self.remaining()

    def get(self, **parameters):
        url = API + '?' + urlencode({'action': 'query', 'format': 'json', 'formatversion': '2', **parameters})
        retry_floor = None

        def failure(message):
            return SourceRetry(message, retry_floor) if retry_floor is not None else SourceError(message)

        def wait(delay):
            try:
                self.wait(delay)
            except SourceError as error:
                raise failure('Retry exceeds acquisition deadline') from error

        for attempt in range(3):
            started = self.clock()
            try:
                timeout = min(REQUEST_SECONDS, self.remaining())
            except SourceError as error:
                raise failure('Acquisition job deadline') from error
            try:
                self.requests += 1
                status, headers, body = self.transport(url, timeout)
            except (OSError, http.client.HTTPException) as error:
                if attempt == 2:
                    raise failure('API transport failed') from error
                wait(2 ** attempt)
                continue
            transient = status == 429 or 500 <= status <= 599
            delay = 2 ** attempt
            if transient:
                header = next((v for k, v in headers.items() if k.lower() == 'retry-after'), None)
                if header is not None:
                    server_delay, floor = retry_after(header, self.wall())
                    retry_floor = max(retry_floor or 0, floor)
                    delay = max(delay, server_delay)
            if self.clock() - started >= timeout:
                raise failure('API request deadline')
            try:
                self.remaining()
            except SourceError as error:
                raise failure('Acquisition job deadline') from error
            if status in (401, 403):
                raise SourceDenied('API access denied')
            if len(body) > MAX_RESPONSE:
                raise SourceError('Oversized API response')
            if transient:
                if attempt == 2:
                    raise failure('API retry limit')
                wait(delay)
                continue
            # 304 has no body validation and is never considered successful.
            if status != 200:
                raise SourceError('API access refused or unexpected HTTP status')
            content_type = next((v for k, v in headers.items() if k.lower() == 'content-type'), '')
            if content_type and 'application/json' not in content_type.lower():
                raise SourceDenied('API challenge or non-JSON response')
            if body.lstrip().startswith(b'<'):
                raise SourceDenied('API challenge or non-JSON response')
            value = json_value(body)
            if isinstance(value, dict) and isinstance(value.get('error'), dict) and value['error'].get('code') in ('readapidenied', 'permissiondenied', 'assertuserfailed'):
                raise SourceDenied('API access denied')
            if not isinstance(value, dict) or 'error' in value or 'warnings' in value:
                raise SourceError('API error or unsupported schema')
            return value, self.wall()
        raise SourceError('Unreachable retry state')


def enumerate_pages(client):
    indices, seen, titles = {}, set(), set()
    for namespace, label in NAMESPACES.items():
        listing, continuation, tokens = [], {}, set()
        while True:
            value, _ = client.get(list='allpages', apnamespace=namespace, aplimit='500', **continuation)
            rows = mapping(value.get('query')).get('allpages')
            if not isinstance(rows, list) or len(rows) > 500:
                raise SourceError('Invalid page listing')
            for row in rows:
                if not isinstance(row, dict) or set(row) != {'pageid', 'ns', 'title'} or not positive(row['pageid']) or type(row['ns']) is not int or row['ns'] != int(namespace) or not isinstance(row['title'], str) or not row['title'] or len(row['title']) > 1000 or any(c in row['title'] for c in '\x00\r\n'):
                    raise SourceError('Malformed page identity')
                if row['pageid'] in seen or row['title'] in titles:
                    raise SourceError('Repeated page identity')
                seen.add(row['pageid']); titles.add(row['title']); listing.append(row)
                if len(seen) > MAX_PAGES:
                    raise SourceError('Page count limit')
            continuation = value.get('continue')
            if continuation is None:
                if 'batchcomplete' not in value:
                    raise SourceError('Missing pagination completion')
                break
            if not rows or not isinstance(continuation, dict) or set(continuation) != {'continue', 'apcontinue'} or any(not isinstance(v, str) or not v or len(v) > 1000 for v in continuation.values()):
                raise SourceError('Malformed pagination')
            token = tuple(sorted(continuation.items()))
            if token in tokens:
                raise SourceError('Pagination loop')
            tokens.add(token)
        indices[label] = listing
    if not seen:
        raise SourceError('Empty corpus')
    return indices


def metadata(client, expected, content=False):
    requested = {row['pageid']: row for row in expected}
    value, validated = client.get(prop='revisions', pageids='|'.join(map(str, requested)),
                                 rvprop='ids|timestamp|sha1|size|contentmodel' + ('|content' if content else ''), rvslots='main')
    pages = mapping(value.get('query')).get('pages')
    if 'continue' in value or not isinstance(pages, list) or len(pages) != len(requested):
        raise SourceError('Incomplete revision response')
    result = {}
    for page in pages:
        if not isinstance(page, dict) or not positive(page.get('pageid')) or type(page.get('ns')) is not int or page.get('pageid') not in requested or any(k in page for k in ('missing', 'invalid', 'redirect', 'interwiki')):
            raise SourceError('Missing or unexpected page')
        identity = requested[page['pageid']]
        if any(page.get(k) != identity[k] for k in ('pageid', 'ns', 'title')) or page['pageid'] in result:
            raise SourceError('Page moved or identity changed during acquisition')
        revisions = page.get('revisions')
        if not isinstance(revisions, list) or len(revisions) != 1:
            raise SourceError('Missing current revision')
        revision = mapping(revisions[0])
        slot = mapping(mapping(revision.get('slots')).get('main'))
        digest = revision.get('sha1')
        size = revision.get('size')
        timestamp = revision.get('timestamp')
        model = slot.get('contentmodel', revision.get('contentmodel'))
        if not positive(revision.get('revid')) or type(revision.get('parentid')) is not int or revision['parentid'] < 0 or not isinstance(digest, str) or len(digest) != 40 or any(c not in '0123456789abcdef' for c in digest) or type(size) is not int or not 0 <= size <= MAX_PAGE or model not in ('wikitext', 'Scribunto', 'interactivemap', 'json', 'sanitized-css'):
            raise SourceError('Invalid revision metadata')
        try:
            if millis(timestamp) > millis(validated):
                raise SourceError('Future revision timestamp')
        except (TypeError, AttributeError, ValueError) as error:
            raise SourceError('Invalid revision timestamp') from error
        row = {'pageid': page['pageid'], 'ns': page['ns'], 'title': page['title'], 'revision': revision,
               'sha1': digest, 'size': size, 'timestamp': timestamp, 'model': model, 'validated': validated}
        if content:
            raw = slot.get('content')
            if not isinstance(raw, str) or len(raw.encode()) != size or hashlib.sha1(raw.encode()).hexdigest() != digest:
                raise SourceError('Revision body size/hash mismatch')
            row['raw'] = raw
        result[page['pageid']] = row
    return result


def signature(row):
    return row['pageid'], row['ns'], row['title'], row['revision']['revid'], row['revision']['parentid'], row['timestamp'], row['sha1'], row['size'], row['model']


class OutputBudget:
    def __init__(self, limit=MAX_CORPUS):
        if type(limit) is not int or not 0 < limit <= MAX_CORPUS:
            raise SourceError('Invalid source output budget')
        self.limit = limit
        self.bytes = 0

    def write(self, path, data):
        self.bytes += len(data)
        if self.bytes > self.limit:
            raise SourceError('Physical output byte limit')
        with path.open('xb') as handle:
            handle.write(data)


def write_json(path, value, budget):
    budget.write(path, (json.dumps(value, ensure_ascii=False) + '\n').encode())


def acquire(output, previous=None, client=None, budget_bytes=None, include_images=False):
    """Write an absent output directory, never mutate an active/previous corpus.

    Completion requires stable discovery and a second revision check of every
    page. This detects observed changes, not an atomic MediaWiki transaction.
    """
    client = client or Client()
    output = Path(output)
    cached = Corpus.open(previous) if previous is not None else None
    if cached and output.resolve().is_relative_to(Path(previous).resolve()):
        raise SourceError('Output cannot replace or modify previous corpus')
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    started = client.wall()
    try:
        siteinfo, _ = client.get(meta='siteinfo', siprop='general|namespaces|rightsinfo')
        query = mapping(siteinfo.get('query'))
        if mapping(query.get('general')).get('server') != ORIGIN or mapping(query.get('rightsinfo')).get('text') != 'CC-BY-SA' or any(mapping(mapping(query.get('namespaces')).get(ns)).get('id') != int(ns) for ns in NAMESPACES):
            raise SourceError('Site identity, namespace, or license needs review')
        indices = enumerate_pages(client)
        pages = [row for rows in indices.values() for row in rows]
        records = {}
        cached_by_id = {row['page_id']: row for row in cached.rows} if cached else {}
        cached_revisions = {}
        if cached:
            for item in json_value(source_read(Path(previous), 'catalog.json', 16 * MAX_PAGE)):
                record = json_value(source_read(Path(previous), item['record_path'], 4 * MAX_PAGE))
                cached_revisions[item['pageid']] = record['revisions'][0]
        total, reused = 0, 0
        for offset in range(0, len(pages), 50):
            batch = pages[offset:offset + 50]
            initial = metadata(client, batch)
            for identity in batch:
                row = initial[identity['pageid']]
                old = cached_by_id.get(identity['pageid'])
                if old and old['title'] == row['title'] and old['namespace'] == row['ns'] and old['source']['revision_id'] == row['revision']['revid'] and old['source']['revision_timestamp'] == row['timestamp'] and cached_revisions[identity['pageid']].get('parentid') == row['revision']['parentid'] and cached_revisions[identity['pageid']].get('slots', {}).get('main', {}).get('contentmodel') == row['model'] and len(old['raw'].encode()) == row['size'] and hashlib.sha1(old['raw'].encode()).hexdigest() == row['sha1']:
                    row['raw'] = old['raw']; reused += 1
                else:
                    fetched = metadata(client, [identity], content=True)[identity['pageid']]
                    if signature(fetched) != signature(row):
                        raise SourceError('Revision changed while downloading body')
                    row = fetched
                total += len(row['raw'].encode())
                if total > MAX_CORPUS:
                    raise SourceError('Corpus byte limit')
                records[identity['pageid']] = row
        if enumerate_pages(client) != indices:
            raise SourceError('Wiki page discovery changed during acquisition')
        for offset in range(0, len(pages), 50):
            for page_id, checked in metadata(client, pages[offset:offset + 50]).items():
                if signature(records[page_id]) != signature(checked):
                    raise SourceError('Wiki revision changed during final validation')
                records[page_id]['validated'] = checked['validated']
        client.remaining()
        catalog = []
        budget = OutputBudget(MAX_CORPUS if budget_bytes is None else budget_bytes)
        (output / 'pages').mkdir()
        for label, listing in indices.items():
            (output / label).mkdir()
            write_json(output / ('index-' + label + '.json'), listing, budget)
            for identity in listing:
                row = records[identity['pageid']]
                title, page_id, raw = row['title'], row['pageid'], row['raw']
                source_url = ORIGIN + '/wiki/' + quote(title.replace(' ', '_'), safe='')
                revision = row['revision'].copy()
                revision['slots'] = {'main': {'*': raw, 'contentmodel': row['model']}}
                record = {**identity, 'revisions': [revision], 'retrieved_at': row['validated'], 'source_url': source_url,
                          'revision_url': ORIGIN + '/index.php?oldid=' + str(revision['revid']), 'content_sha256': sha(raw)}
                path, record_path = f'{label}/{page_id}.txt', f'pages/{page_id}.json'
                budget.write(output / path, raw.encode())
                write_json(output / record_path, record, budget)
                catalog.append({'pageid': page_id, 'namespace': row['ns'], 'kind': label, 'title': title,
                                'revision_id': revision['revid'], 'revision_timestamp': row['timestamp'], 'retrieved_at': row['validated'],
                                'source_url': source_url, 'revision_url': record['revision_url'], 'content_sha256': sha(raw),
                                'characters': len(raw), 'content_model': row['model'], 'path': path, 'record_path': record_path})
        write_json(output / 'catalog.json', catalog, budget)
        write_json(output / 'siteinfo.json', siteinfo, budget)
        manifest = {'status': 'complete', 'started_at': started, 'completed_at': client.wall(), 'api': API,
                    'namespaces': NAMESPACES, 'counts': {label: len(rows) for label, rows in indices.items()},
                    'pages': len(pages), 'characters': sum(len(r['raw']) for r in records.values()), 'rightsinfo': query['rightsinfo'],
                    'scope': 'Current revisions in all five configured namespaces; no images or edit history.',
                    'snapshot_consistency': 'Discovery repeated and every revision rechecked; not an atomic wiki snapshot.',
                    'acquisition': {'requests': client.requests, 'reused_pages': reused, 'downloaded_pages': len(pages) - reused}}
        client.remaining()
        write_json(output / 'manifest.json', manifest, budget)
        corpus = Corpus.open(output)
        if include_images:
            from media_source import collect, checksum
            media = collect(corpus, client)
            write_json(output / 'media.json', media, budget)
            manifest['media_sha256'] = checksum(media)
            manifest['completed_at'] = client.wall()
            manifest['scope'] = 'Current revisions in all five configured namespaces and main-image metadata; no image bytes or edit history.'
            manifest['acquisition']['requests'] = client.requests
            (output / 'manifest.json').unlink()
            write_json(output / 'manifest.json', manifest, budget)
            Corpus.open(output)
        client.remaining()
        return manifest
    except Exception:
        # Never leave a failed acquisition marked complete, including validation
        # failure after writing candidate files. Existing corpora are never touched.
        manifest_path = output / 'manifest.json'
        if manifest_path.exists():
            manifest_path.unlink()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--previous', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(acquire(args.output, args.previous, include_images=True)))
    except SourceDenied:
        parser.exit(3, 'Source acquisition denied; scheduled acquisition must stop.\n')
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(2, 'Source acquisition failed: ' + type(error).__name__ + '\n')


if __name__ == '__main__':
    main()
