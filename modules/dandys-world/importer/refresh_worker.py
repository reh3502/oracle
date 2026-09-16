"""Isolated acquisition + normalization worker; never publishes a snapshot.

The host must supervise this process and hold its store writer lock while disk
is produced. Source acquisition remains disabled on the deployed bot until the
external acquisition route is qualified. No URL or executable override exists.
"""
import argparse
import contextlib
import json
import os
from pathlib import Path
import stat
import sys
import threading

# Packaged scripts must not create unaccounted bytecode outside the job tree.
sys.dont_write_bytecode = True

from refresh_source import SourceDenied, SourceError, SourceRetry, acquire

MAX_MEMORY = 512 * 1024 * 1024
MAX_FILE = 128 * 1024 * 1024
MAX_OUTPUT = 256 * 1024 * 1024
MAX_SECONDS = 900
RESULT_RESERVE = 512


_windows_job = None


def apply_limits():
    if os.name == 'nt':
        _apply_windows_limits()
        return
    import resource
    for kind, desired in ((resource.RLIMIT_AS, MAX_MEMORY),
                          (resource.RLIMIT_CPU, MAX_SECONDS),
                          (resource.RLIMIT_FSIZE, MAX_FILE)):
        soft, hard = resource.getrlimit(kind)
        hard = desired if hard == resource.RLIM_INFINITY else min(hard, desired)
        soft = hard if soft == resource.RLIM_INFINITY else min(soft, hard)
        resource.setrlimit(kind, (soft, hard))


def _apply_windows_limits():
    """The worker owns a nested job before parsing/network acquisition starts.

    It is intentionally limited to one process. Cancellation only needs to kill
    and join this Python child; the module's outer job covers abrupt parent death.
    File quotas remain enforced before writes in the acquisition and serializer.
    """
    import ctypes
    from ctypes import wintypes
    global _windows_job

    class Basic(ctypes.Structure):
        _fields_ = [('process_time', ctypes.c_longlong), ('job_time', ctypes.c_longlong),
                    ('flags', wintypes.DWORD), ('min_working_set', ctypes.c_size_t),
                    ('max_working_set', ctypes.c_size_t), ('active_processes', wintypes.DWORD),
                    ('affinity', ctypes.c_size_t), ('priority', wintypes.DWORD),
                    ('scheduling', wintypes.DWORD)]

    class Io(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in
                    ('read_ops', 'write_ops', 'other_ops', 'read_bytes', 'write_bytes', 'other_bytes')]

    class Extended(ctypes.Structure):
        _fields_ = [('basic', Basic), ('io', Io), ('process_memory', ctypes.c_size_t),
                    ('job_memory', ctypes.c_size_t), ('peak_process_memory', ctypes.c_size_t),
                    ('peak_job_memory', ctypes.c_size_t)]

    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.SetInformationJobObject.restype = wintypes.BOOL
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.AssignProcessToJobObject.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    job = kernel.CreateJobObjectW(None, None)
    if not job:
        raise ctypes.WinError(ctypes.get_last_error())
    limits = Extended()
    limits.basic.process_time = MAX_SECONDS * 10_000_000
    limits.basic.active_processes = 1
    # PROCESS_TIME | ACTIVE_PROCESS | PROCESS_MEMORY | KILL_ON_JOB_CLOSE
    limits.basic.flags = 0x2 | 0x8 | 0x100 | 0x2000
    limits.process_memory = MAX_MEMORY
    if not kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
        error = ctypes.get_last_error()
        kernel.CloseHandle(job)
        raise ctypes.WinError(error)
    if not kernel.AssignProcessToJobObject(job, kernel.GetCurrentProcess()):
        error = ctypes.get_last_error()
        kernel.CloseHandle(job)
        raise ctypes.WinError(error)
    # Keep open until process exit. Closing this job while running kills us.
    _windows_job = job


@contextlib.contextmanager
def wall_deadline(seconds=MAX_SECONDS):
    # Exit rather than leave DNS/normalization activity behind after timeout.
    # The host kill+wait path remains responsible for external cancellation.
    timer = threading.Timer(seconds, lambda: os._exit(124))
    timer.daemon = True
    timer.start()
    try:
        yield
    finally:
        timer.cancel()


def normalize_source(source):
    # Import the fixed packaged normalizer after process limits are installed.
    from normalize import Normalizer
    from wiki_source import Corpus
    return Normalizer(Corpus.open(source)).build()


def regular_bytes(directory):
    total = 0
    for path in directory.rglob('*'):
        metadata = path.lstat()
        if (stat.S_ISLNK(metadata.st_mode)
                or getattr(metadata, 'st_file_attributes', 0) & 0x400
                or not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode))):
            raise SourceError('Unexpected source output entry')
        if stat.S_ISREG(metadata.st_mode):
            total += metadata.st_size
            if total > MAX_OUTPUT:
                raise SourceError('Worker output limit')
    return total


def run(output, previous=None, budget_bytes=MAX_OUTPUT, *, acquirer=None, normalizer=None):
    """Produce output/source and output/candidate.json with pre-write budgets.

    Dependency injection is an offline test seam only. The command line always
    calls the fixed canonical acquirer and packaged normalizer.
    """
    output = Path(output)
    if not output.is_absolute() or len(str(output).encode()) > 4096 or '..' in output.parts:
        raise SourceError('Worker output must be an absolute dedicated directory')
    if type(budget_bytes) is not int or budget_bytes <= 0:
        raise SourceError('Invalid worker disk budget')
    budget = min(budget_bytes, MAX_OUTPUT)
    if budget <= RESULT_RESERVE:
        raise SourceError('Insufficient worker result budget')
    data_budget = budget - RESULT_RESERVE
    if previous is not None:
        previous = Path(previous)
        if not previous.is_absolute() or output.resolve().is_relative_to(previous.resolve()):
            raise SourceError('Worker output cannot modify the previous corpus')
    acquirer = acquirer or (lambda *args, **kwargs: acquire(*args, **kwargs, include_images=True))
    normalizer = normalizer or normalize_source
    output.mkdir(mode=0o700, exist_ok=False)
    source = output / 'source'
    candidate = output / 'candidate.json'
    partial = output / '.candidate.tmp'
    try:
        acquirer(source, previous=previous, budget_bytes=min(data_budget, MAX_FILE))
        used = regular_bytes(output)
        if used > data_budget:
            raise SourceError('Source exceeded worker disk budget')
        result = normalizer(source)
        encoded_bytes = 0
        encoder = json.JSONEncoder(ensure_ascii=False, sort_keys=True, separators=(',', ':'), allow_nan=False)
        with partial.open('xb') as handle:
            for piece in encoder.iterencode(result):
                encoded = piece.encode()
                if len(encoded) > min(MAX_FILE - encoded_bytes, data_budget - used - encoded_bytes):
                    raise SourceError('Normalized candidate exceeds remaining disk budget')
                handle.write(encoded)
                encoded_bytes += len(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        # Complete name becomes visible only after the entire bounded file is
        # flushed; no operation here changes the active store or previous corpus.
        partial.rename(candidate)
        return {'candidate': str(candidate), 'corpus': str(source),
                'candidate_bytes': encoded_bytes, 'output_bytes': used + encoded_bytes}
    except BaseException as error:
        if isinstance(error, SourceRetry):
            metadata = {'status': 'retry', 'retry_not_before_ms': error.retry_not_before_ms}
        elif isinstance(error, SourceDenied):
            metadata = {'status': 'stopped', 'reason': getattr(error, 'reason', 'access_denied')}
        else:
            metadata = None
        if metadata is not None:
            encoded = json.dumps(metadata, separators=(',', ':')).encode()
            if len(encoded) > RESULT_RESERVE:
                raise SourceDenied('Worker retry metadata cannot be preserved') from error
            try:
                with (output / 'result.json').open('xb') as handle:
                    handle.write(encoded)
                    handle.flush()
                    os.fsync(handle.fileno())
            except OSError as failure:
                # Losing a server deadline must stop scheduling, never fall back
                # to a shorter generic retry interval (for example on ENOSPC).
                raise SourceDenied('Worker retry metadata cannot be preserved') from failure
        # Keep failed source material available for host cleanup/diagnostics, but
        # never expose a partial normalized file under the candidate name.
        if partial.exists():
            partial.unlink()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--previous', type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--budget-bytes', required=True, type=int)
    args = parser.parse_args()
    try:
        apply_limits()
        with wall_deadline():
            result = run(args.output, args.previous, args.budget_bytes)
            print(json.dumps(result, separators=(',', ':')))
    except SourceDenied:
        parser.exit(3, 'Source acquisition stopped; inspect bounded result metadata.\n')
    except (OSError, ValueError, KeyError, TypeError, MemoryError):
        parser.exit(2, 'Refresh worker failed; no candidate may be published.\n')


if __name__ == '__main__':
    main()
