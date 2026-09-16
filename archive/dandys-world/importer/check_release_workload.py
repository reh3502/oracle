#!/usr/bin/env python3
"""Qualify existing binaries on an isolated full catalog; no network or live store.

Example: python3 importer/check_release_workload.py --module target/release/dw-module
  --cli target/release/dw-query --catalog /path/candidate.json --output /path/new-run
  --legacy-cli /path/pre-stage3/dw-query
Outputs machine-readable timing, memory, hashes, recovery evidence and raw latencies.
The optional legacy binary is needed to prove old-reader rejection experimentally.
Add --corpus /path/saved-wiki-corpus --python /path/venv/bin/python to measure
five clients throughout real offline copy/normalization under production worker
limits. The copy seam has no network access and cannot publish without host review.
"""
import argparse
import asyncio
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(command, expect=0):
    result = subprocess.run(list(map(str, command)), capture_output=True, text=True, timeout=60)
    assert result.returncode == expect, (command, result.returncode, result.stderr)
    return result


def cli(binary, command, store, **options):
    args = [binary, command, '--store', store]
    for key, value in options.items():
        args.extend(['--' + key.replace('_', '-'), value])
    return json.loads(run(args).stdout)


def memory(pid):
    try:
        rows = Path(f'/proc/{pid}/status').read_text().splitlines()
        return {row.split(':')[0]: int(row.split()[1]) for row in rows
                if row.startswith(('VmRSS:', 'VmHWM:', 'VmSize:'))}
    except FileNotFoundError:
        return {}


class Peer:
    def __init__(self, process):
        self.process = process
        self.pending = {}
        self.next_id = 0
        self.max_pending = 0
        self.reader = asyncio.create_task(self.read())

    async def read(self):
        try:
            while True:
                size = struct.unpack('>I', await self.process.stdout.readexactly(4))[0]
                assert 0 < size <= 1024 * 1024
                frame = json.loads(await self.process.stdout.readexactly(size))
                assert frame['kind'] == 'response', 'unexpected module callback'
                future = self.pending.pop(frame['id'])
                result = frame['result']
                if 'Ok' in result:
                    future.set_result(result['Ok'])
                else:
                    future.set_exception(AssertionError(result))
        except (Exception, asyncio.CancelledError) as error:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(error)
            self.pending.clear()

    async def call(self, method, params):
        self.next_id += 1
        request_id = self.next_id
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        self.max_pending = max(self.max_pending, len(self.pending))
        body = json.dumps(dict(kind='request', protocol=1, id=request_id,
                               method=method, params=params, timeout_ms=10000)).encode()
        self.process.stdin.write(struct.pack('>I', len(body)) + body)
        await self.process.stdin.drain()
        return await asyncio.wait_for(future, 12)

    async def invoke(self, operation, input_value):
        return await self.call('operation.invoke', dict(
            invocation=f'qualification-{self.next_id}', session='release-workload',
            generation=1, guild='123', epoch=1, operation=operation, input=input_value))

    async def close(self):
        await self.call('shutdown', {})
        self.process.stdin.close()
        await asyncio.wait_for(self.process.wait(), 10)
        await self.reader
        assert self.process.returncode == 0


async def launch(binary, store, stderr):
    process = await asyncio.create_subprocess_exec(
        str(binary), stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=stderr)
    peer = Peer(process)
    try:
        hello = await peer.call('hello', dict(protocol_major=1, protocol_minor=1,
                                             session='release-workload', generation=1))
        assert hello['protocol_minor'] == 1
        await peer.call('initialize', dict(session='release-workload', generation=1,
                                          mode='normal', runtime=dict(data_directory=str(store))))
        await peer.call('activate', dict(guild='123', epoch=1))
        return peer
    except BaseException:
        process.kill()
        await process.wait()
        raise


SAFE_INTEGER = 9_007_199_254_740_991
NAME = re.compile(r'[a-z0-9_-]{1,32}\Z')


def display_text(value, maximum, empty=False):
    assert isinstance(value, str)
    units = len(value.encode('utf-16-le')) // 2
    assert units <= maximum and (empty or value.strip())
    assert not any((ord(c) < 32 and c != '\n') or 0x7f <= ord(c) <= 0x9f
                   or ord(c) in (0x61c, 0x200e, 0x200f, 0xfeff)
                   or 0x200b <= ord(c) <= 0x200d
                   or 0x202a <= ord(c) <= 0x202e
                   or 0x2066 <= ord(c) <= 0x2069 for c in value)
    return units


def validate_action(action, prompt=False):
    expected = {'label', 'route', 'options'} if prompt else {'label', 'description', 'route', 'options'}
    assert isinstance(action, dict)
    if prompt:
        assert set(action) in (expected, expected | {'prompt'})
    else:
        assert set(action) == expected
    display_text(action['label'], 80)
    assert isinstance(action['route'], str) and NAME.fullmatch(action['route'])
    options = action['options']
    assert isinstance(options, dict) and len(options) <= 25
    assert len(json.dumps(options, ensure_ascii=False, separators=(',', ':')).encode()) <= 8192
    for name, value in options.items():
        assert NAME.fullmatch(name)
        if isinstance(value, str):
            display_text(value, 6000, empty=True)
        elif type(value) is int:
            assert -SAFE_INTEGER <= value <= SAFE_INTEGER
        else:
            assert type(value) is bool
    if not prompt:
        display_text(action['description'], 100, empty=True)
    modal = action.get('prompt')
    if modal is not None:
        assert isinstance(modal, dict) and set(modal) == {'label', 'option', 'placeholder', 'max_length'}
        display_text(modal['label'], 45)
        display_text(modal['placeholder'], 100, empty=True)
        assert isinstance(modal['option'], str) and NAME.fullmatch(modal['option'])
        assert modal['option'] not in options
        assert type(modal['max_length']) is int and 1 <= modal['max_length'] <= 200


def validate_reply(result):
    """Validate both shipped reply formats without relaxing citation/action bounds.

    These are wire-shape checks. The Rust card_corpus integration test additionally
    exercises host sanitization and measures expanded Discord UTF-16 lengths.
    """
    reply = result['reply']
    assert isinstance(reply, dict)
    legacy = set(reply) == {'text', 'citations'}
    assert legacy or set(reply) in ({'text', 'card', 'citations', 'buttons', 'choices'}, {'text', 'card', 'citations', 'buttons', 'choices', 'image'})
    if 'image' in reply:
        from media_source import image_url
        image = reply['image']
        assert isinstance(image, dict) and set(image) == {'url', 'revision'}
        assert image_url(image['url'])
        assert type(image['revision']) is int and 0 < image['revision'] <= SAFE_INTEGER
    display_text(reply['text'], 1800 if legacy else 2000)
    assert isinstance(reply['citations'], list) and len(reply['citations']) <= 5
    for citation in reply['citations']:
        assert isinstance(citation, dict) and set(citation) == {'label', 'revision'}
        display_text(citation['label'], 120)
        assert type(citation['revision']) is int and 0 < citation['revision'] <= SAFE_INTEGER
    if legacy:
        return
    card = reply['card']
    assert isinstance(card, dict) and set(card) == {'title', 'description', 'fields', 'footer'}
    total = display_text(card['title'], 256)
    total += display_text(card['description'], 4096, empty=True)
    total += display_text(card['footer'], 512, empty=True)
    assert isinstance(card['fields'], list) and len(card['fields']) + bool(reply['citations']) <= 20
    for field in card['fields']:
        assert isinstance(field, dict) and set(field) == {'name', 'value', 'inline'}
        total += display_text(field['name'], 256)
        total += display_text(field['value'], 1024)
        assert type(field['inline']) is bool
    assert total <= 6000
    for name, maximum, prompt in [('buttons', 5, True), ('choices', 25, False)]:
        assert isinstance(reply[name], list) and len(reply[name]) <= maximum
        for action in reply[name]:
            validate_action(action, prompt=prompt)


async def workload(peer, cases, rounds, worker_pid=None, worker_finished=None):
    latencies = []
    overlap_latencies = []
    worker_peak = {}
    peak = {}
    stop = asyncio.Event()

    async def sample():
        while not stop.is_set():
            for key, value in memory(peer.process.pid).items():
                peak[key] = max(peak.get(key, 0), value)
            if worker_pid:
                for key, value in memory(worker_pid).items():
                    worker_peak[key] = max(worker_peak.get(key, 0), value)
            await asyncio.sleep(.01)

    async def client(index):
        count = 0
        while count < rounds or (worker_pid and not worker_finished.exists()):
            assert len(latencies) < 1_000_000, 'qualification sample budget exhausted'
            assert time.perf_counter() - started < 905, 'qualification worker deadline exceeded'
            case = cases[(index + count * 5) % len(cases)]
            overlapping = worker_pid and Path(f'/proc/{worker_pid}').exists() and not worker_finished.exists()
            start = time.perf_counter_ns()
            result = await peer.invoke('lookup', case)
            elapsed = (time.perf_counter_ns() - start) / 1_000_000
            validate_reply(result)
            latencies.append(elapsed)
            if overlapping:
                overlap_latencies.append(elapsed)
            count += 1
    sampler = asyncio.create_task(sample())
    started = time.perf_counter()
    try:
        await asyncio.gather(*(client(index) for index in range(5)))
    finally:
        stop.set()
        await sampler
    ordered = sorted(latencies)
    p95 = ordered[math.ceil(len(ordered) * .95) - 1]
    assert p95 < 1000, f'p95 target failed: {p95} ms'
    assert peer.max_pending == 5, peer.max_pending
    overlap = None
    if worker_pid:
        assert overlap_latencies, 'no lookup overlapped the real offline worker'
        ordered_overlap = sorted(overlap_latencies)
        overlap = dict(requests=len(overlap_latencies),
                       p95_ms=ordered_overlap[math.ceil(len(ordered_overlap) * .95) - 1],
                       max_ms=max(ordered_overlap), worker_memory_kib=worker_peak)
        assert overlap['p95_ms'] < 1000
    return dict(refresh_overlap=overlap, requests=len(latencies), concurrent_clients=5, max_outstanding=peer.max_pending,
                p50_ms=ordered[math.ceil(len(ordered) * .5) - 1], p95_ms=p95,
                max_ms=max(ordered), wall_seconds=time.perf_counter() - started,
                process_memory_kib=peak, latency_ms=latencies)


async def qualify(args, report):
    output = args.output
    catalog_bytes = args.catalog.read_bytes()
    catalog = json.loads(catalog_bytes)
    report['corpus'] = dict(bytes=len(catalog_bytes), sha256=digest(args.catalog),
                            entities=len(catalog['entities']), sources=len(catalog['sources']),
                            oldest_validated_at_ms=min(s['validated_at_ms'] for s in catalog['sources']),
                            latest_validated_at_ms=max(s['validated_at_ms'] for s in catalog['sources']))
    assert len(catalog['entities']) == 308 and len(catalog['sources']) == 2119
    cases = [dict(name=e['id'], field=e['facts'][0]['key'])
             for e in catalog['entities'] if e['facts']]
    report['workload'] = dict(cases=cases, rounds_per_client=args.rounds,
                              measurement='wall time from framed SDK invocation through decoded reply; excludes Discord and host manager',
                              cache='catalog loaded in module memory; warm-up visits every selected entity; no assumed response cache')
    store = output / 'store'
    initial = cli(args.cli, 'publish', store, catalog=args.catalog)['snapshot_id']
    peer = None
    with (output / 'module-stderr.log').open('wb') as stderr:
        try:
            peer = await launch(args.module, store, stderr)
            for case in cases:
                validate_reply(await peer.invoke('lookup', case))
            report['baseline'] = await workload(peer, cases, args.rounds)
            await peer.close()
            peer = None
            worker = output / 'stall_worker.py'
            worker.write_text('from pathlib import Path\nimport os,time\n'
                              "payload=bytearray(32*1024*1024)\n"
                              "Path(__file__).with_suffix('.pid').write_text(str(os.getpid()))\n"
                              'time.sleep(300)\n')
            worker.chmod(0o600)
            settings = dict(enabled=True, source_access_qualified=True,
                            python=str(Path(sys.executable).resolve()), worker=str(worker), previous=None)
            (store / 'refresh-settings.json').write_text(json.dumps(settings))
            peer = await launch(args.module, store, stderr)
            for _ in range(500):
                if worker.with_suffix('.pid').exists():
                    break
                await asyncio.sleep(.01)
            assert worker.with_suffix('.pid').exists(), 'fixture worker did not start'
            worker_pid = int(worker.with_suffix('.pid').read_text())
            assert Path(f'/proc/{worker_pid}').exists()
            report['stalled_worker_memory_kib'] = memory(worker_pid)
            for case in cases:
                validate_reply(await peer.invoke('lookup', case))
            report['stalled_refresh'] = await workload(peer, cases, args.rounds)
            start = time.perf_counter()
            await peer.close()
            peer = None
            report['shutdown_cancel_seconds'] = time.perf_counter() - start
            assert not Path(f'/proc/{worker_pid}').exists(), 'refresh worker survived shutdown'
            state = json.loads((store / 'refresh-schedule.json').read_text())
            assert state['running'] and state['last_success_ms'] is None
            # Remove only this isolated test's settings; restart must recover the
            # pinned catalog without starting another intentionally stalled worker.
            (store / 'refresh-settings.json').unlink()
            peer = await launch(args.module, store, stderr)
            validate_reply(await peer.invoke('lookup', cases[0]))
            await peer.close()
            peer = None
            report['restart_after_cancellation'] = 'passed; active catalog unchanged'
        finally:
            if peer is not None and peer.process.returncode is None:
                peer.process.kill()
                await peer.process.wait()
    if args.corpus:
        await offline_refresh(args, report, store, cases, initial, catalog)
    assert cli(args.cli, 'recover', store)['snapshot_id'] == initial
    # Create two full catalogs without changing any per-source validation time.
    catalog['adapter_version'] += '-release-recovery-fixture'
    second_path = output / 'second-catalog.json'
    second_path.write_text(json.dumps(catalog, separators=(',', ':')))
    second = cli(args.cli, 'publish', store, catalog=second_path)['snapshot_id']
    assert second != initial
    backup = output / 'backup'
    cli(args.cli, 'backup', store, output=backup)
    restored = output / 'restored'
    assert cli(args.cli, 'restore', restored, backup=backup)['restored'] == second
    for snapshot_id in (initial, second):
        original = (store / f'{snapshot_id}.json').read_bytes()
        restored_bytes = (restored / f'{snapshot_id}.json').read_bytes()
        assert original == restored_bytes
        assert hashlib.sha256(restored_bytes).hexdigest() == snapshot_id
        assert json.loads(restored_bytes)['sources'] == json.loads(catalog_bytes)['sources']
    with (output / 'restored-module-stderr.log').open('wb') as stderr:
        peer = await launch(args.module, restored, stderr)
        try:
            validate_reply(await peer.invoke('lookup', cases[0]))
        finally:
            await peer.close()
    assert cli(args.cli, 'rollback', restored)['rolled_back'] == initial
    assert cli(args.cli, 'recover', restored)['snapshot_id'] == initial
    # The full old reader must reject the upgraded pointer without modifying it.
    upgraded_head = (restored / 'active').read_bytes()
    assert json.loads(upgraded_head)['store_format_version'] == 1
    downgrade = dict(new_pointer_format=1, legacy_reader='not supplied; inspection only')
    query_input = json.dumps(dict(op='status'))
    if args.legacy_cli:
        failed = run([args.legacy_cli, 'query', '--store', restored, '--input', query_input], expect=2)
        assert (restored / 'active').read_bytes() == upgraded_head
        downgrade.update(legacy_reader='rejected format 1 without mutation',
                         rejection_stderr=failed.stderr.strip(), legacy_binary_sha256=digest(args.legacy_cli))
    legacy_store = output / 'legacy-plain64'
    legacy_store.mkdir()
    shutil.copyfile(restored / f'{initial}.json', legacy_store / f'{initial}.json')
    (legacy_store / 'active').write_text(initial)
    assert cli(args.cli, 'recover', legacy_store)['snapshot_id'] == initial
    if args.legacy_cli:
        assert cli(args.legacy_cli, 'query', legacy_store, input=query_input)['snapshot_id'] == initial
    # Strict rejection of an incompatible corpus schema through the real CLI.
    invalid = output / 'future-schema.json'
    catalog['schema_version'] = 999
    invalid.write_text(json.dumps(catalog))
    run([args.cli, 'publish', '--store', restored, '--catalog', invalid], expect=2)
    assert (restored / 'active').read_bytes() == upgraded_head
    corrupt_store = output / 'corrupt-active'
    assert cli(args.cli, 'restore', corrupt_store, backup=backup)['restored'] == second
    (corrupt_store / f'{second}.json').write_bytes(b'corrupt active catalog')
    with (output / 'recovered-module-stderr.log').open('wb') as stderr:
        peer = await launch(args.module, corrupt_store, stderr)
        try:
            validate_reply(await peer.invoke('lookup', cases[0]))
        finally:
            await peer.close()
    assert cli(args.cli, 'recover', corrupt_store)['snapshot_id'] == initial
    assert (corrupt_store / f'{initial}.json').read_bytes() == catalog_bytes
    bad_backup = output / 'corrupt-backup'
    shutil.copytree(backup, bad_backup)
    (bad_backup / f'{second}.json').write_bytes(b'corrupt backup catalog')
    before_bad_restore = (restored / 'active').read_bytes()
    run([args.cli, 'restore', '--store', restored, '--backup', bad_backup], expect=2)
    assert (restored / 'active').read_bytes() == before_bad_restore
    report['recovery'] = dict(initial=initial, second=second, backup_restore='byte-for-byte active and previous',
                              corrupt_active_sdk_recovery=True, corrupt_backup_rejected_before_publication=True,
                              all_source_records_preserved=True, restored_sdk_restart=True,
                              rollback=True, incompatible_schema_rejected=True, downgrade=downgrade,
                              legacy_plain64_read_by_new=True)


OFFLINE_WORKER = r'''
import argparse, hashlib, json, os, resource, stat, sys, time
from pathlib import Path
sys.dont_write_bytecode = True
sys.path.insert(0, IMPORTER)
from refresh_worker import apply_limits, wall_deadline, run
p=argparse.ArgumentParser()
p.add_argument('--output', type=Path, required=True)
p.add_argument('--budget-bytes', type=int, required=True)
a=p.parse_args()
apply_limits()
started=time.monotonic()
Path(PID_FILE).write_text(str(os.getpid()))
report={'success':False,'limits':{'address_space_bytes':resource.getrlimit(resource.RLIMIT_AS)[0],
                                 'cpu_seconds':resource.getrlimit(resource.RLIMIT_CPU)[0]},
        'expected_files':FILE_COUNT,'expected_source_bytes':SOURCE_BYTES}
def copy_saved(destination, previous=None, budget_bytes=0):
    files=[]
    total=0
    for path in Path(CORPUS).rglob('*'):
        meta=path.lstat()
        assert not stat.S_ISLNK(meta.st_mode)
        relative=path.relative_to(CORPUS)
        assert len(relative.parts)<=16
        if stat.S_ISDIR(meta.st_mode):
            continue
        assert stat.S_ISREG(meta.st_mode) and meta.st_size<=16*1024*1024
        files.append((path,relative,meta.st_size))
        total+=meta.st_size
        assert len(files)<=FILE_COUNT and total<=min(SOURCE_BYTES,budget_bytes)
    assert len(files)==FILE_COUNT and total==SOURCE_BYTES
    destination.mkdir(mode=0o700)
    copied=0
    for source,relative,size in files:
        target=destination/relative
        target.parent.mkdir(parents=True,exist_ok=True)
        assert copied+size<=budget_bytes
        # Input is bounded before it is read; reject mutation or symlink races.
        fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW)
        with os.fdopen(fd,'rb') as handle:
            data=handle.read(size+1)
        assert len(data)==size
        with target.open('xb') as handle:
            handle.write(data)
        assert hashlib.sha256(target.read_bytes()).digest()==hashlib.sha256(data).digest()
        copied+=size
    report['copied_files']=len(files)
    report['copied_bytes']=copied
try:
    with wall_deadline():
        outcome=run(a.output,budget_bytes=a.budget_bytes,acquirer=copy_saved)
        candidate=Path(outcome['candidate']).read_bytes()
        data=json.loads(candidate)
        validations=sorted((s['id'],s['validated_at_ms']) for s in data['sources'])
        validation_hash=hashlib.sha256(json.dumps(validations,separators=(',',':')).encode()).hexdigest()
        assert validation_hash==VALIDATION_SHA, 'source validation timestamps changed'
        report.update(outcome)
        report.update(success=True, candidate_sha256=hashlib.sha256(candidate).hexdigest(),
                      validation_sha256=validation_hash, entities=len(data['entities']),sources=len(data['sources']))
finally:
    usage=resource.getrusage(resource.RUSAGE_SELF)
    report.update(elapsed_seconds=time.monotonic()-started,cpu_user_seconds=usage.ru_utime,
                  cpu_system_seconds=usage.ru_stime,peak_rss_kib=usage.ru_maxrss)
    Path(RESULT_FILE).write_text(json.dumps(report))
'''


async def offline_refresh(args, report, store, cases, initial, catalog):
    import stat
    files = []
    for path in args.corpus.rglob('*'):
        metadata = path.lstat()
        assert not stat.S_ISLNK(metadata.st_mode)
        assert stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)
        if stat.S_ISREG(metadata.st_mode):
            files.append(path)
        assert len(files) <= 10000
    total = sum(path.stat().st_size for path in files)
    assert total <= 128 * 1024 * 1024
    validations = sorted((s['id'], s['validated_at_ms']) for s in catalog['sources'])
    validation_hash = hashlib.sha256(json.dumps(validations, separators=(',', ':')).encode()).hexdigest()
    worker = args.output / 'offline_worker.py'
    pid_file = args.output / 'offline-worker.pid'
    result_file = args.output / 'offline-worker-result.json'
    constants = dict(IMPORTER=str(Path(__file__).resolve().parent), CORPUS=str(args.corpus),
                     PID_FILE=str(pid_file), RESULT_FILE=str(result_file), FILE_COUNT=len(files),
                     SOURCE_BYTES=total, VALIDATION_SHA=validation_hash)
    worker.write_text('\n'.join(f'{key}={value!r}' for key, value in constants.items()) + '\n' + OFFLINE_WORKER)
    worker.chmod(0o600)
    (store / 'refresh-settings.json').write_text(json.dumps(dict(
        enabled=True, source_access_qualified=True, python=str(args.python), worker=str(worker), previous=None)))
    with (args.output / 'offline-module-stderr.log').open('wb') as stderr:
        peer = await launch(args.module, store, stderr)
        try:
            for _ in range(1000):
                if pid_file.exists():
                    break
                await asyncio.sleep(.01)
            assert pid_file.exists(), 'real offline worker did not start'
            pid = int(pid_file.read_text())
            report['offline_refresh'] = await workload(peer, cases, args.rounds, pid, result_file)
            # All worker and publication/review work must finish within its
            # production deadline. Queries remain available while it completes.
            deadline = time.monotonic() + 905
            while time.monotonic() < deadline:
                state = json.loads((store / 'refresh-schedule.json').read_text())
                if not state['running']:
                    break
                validate_reply(await peer.invoke('lookup', cases[0]))
                await asyncio.sleep(.01)
            else:
                raise AssertionError('offline refresh failed to complete within production deadline')
            assert result_file.exists()
            result = json.loads(result_file.read_text())
            report['offline_worker'] = result
            report['offline_schedule_result'] = state['last_result']
            assert result['success'], result
            assert result['validation_sha256'] == validation_hash
            assert result['entities'] == 308 and result['sources'] == 2119
            assert result['limits']['address_space_bytes'] == 512 * 1024 * 1024
            assert result['limits']['cpu_seconds'] == 900
            assert result['peak_rss_kib'] < 512 * 1024
            assert state['last_result'] in ('unchanged', 'review_required', 'rejected')
            assert not Path(f'/proc/{pid}').exists(), 'completed worker was not reaped'
        finally:
            await peer.close()
            (store / 'refresh-settings.json').unlink()
    assert cli(args.cli, 'recover', store)['snapshot_id'] == initial
    report['offline_active_unchanged'] = True
    report['limitations'] = [item for item in report['limitations'] if not item.startswith('Refresh fixture')]
    report['limitations'].append('Offline replay measures real copy/normalization/review; network acquisition is not exercised')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('module', 'cli', 'catalog', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--legacy-cli', type=Path)
    parser.add_argument('--corpus', type=Path, help='verified saved corpus for real offline normalization')
    parser.add_argument('--python', type=Path, help='interpreter with packaged normalizer dependencies')
    parser.add_argument('--rounds', type=int, default=20000)
    args = parser.parse_args()
    assert bool(args.corpus) == bool(args.python), '--corpus and --python must be provided together'
    assert 20 <= args.rounds <= 100000
    if args.python:
        args.python = args.python.absolute()  # Preserve virtual-environment executable spelling.
    for name in ('module', 'cli', 'catalog', 'output', 'legacy_cli', 'corpus'):
        value = getattr(args, name)
        if value is not None:
            setattr(args, name, value.resolve())
    args.output.mkdir(parents=True, exist_ok=False)
    report = dict(started_unix_ms=int(time.time() * 1000), success=False,
                  machine=dict(platform=platform.platform(), cpu_count=os.cpu_count(),
                               cpu_model=next((line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
                                               if line.startswith('model name')), 'unknown'),
                               meminfo=Path('/proc/meminfo').read_text(),
                               loadavg=Path('/proc/loadavg').read_text().strip()),
                  limitations=['Excludes Discord transport and host manager overhead',
                                'Refresh fixture allocates 32 MiB then sleeps; not a real network crawl or CPU-heavy normalization',
                                'No live deployment or source-access qualification is performed'],
                  artifacts=dict(module_sha256=digest(args.module), cli_sha256=digest(args.cli),
                                 module_path=str(args.module), cli_path=str(args.cli),
                                 manifest_version=json.loads((Path(__file__).parent.parent / 'manifest.json').read_text())['version'],
                                 sdk_commit=run(['git', 'rev-parse', 'HEAD']).stdout.strip()))
    try:
        asyncio.run(qualify(args, report))
        report['success'] = True
    except BaseException as error:
        report['failure'] = repr(error)
        raise
    finally:
        report['finished_unix_ms'] = int(time.time() * 1000)
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: report[key] for key in ('success', 'corpus', 'recovery')}, indent=2))
    for phase in ('baseline', 'stalled_refresh', 'offline_refresh'):
        if phase not in report:
            continue
        print(phase, {key: value for key, value in report[phase].items() if key != 'latency_ms'})


if __name__ == '__main__':
    main()
