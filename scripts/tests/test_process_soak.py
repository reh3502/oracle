import importlib.util
import pathlib
import unittest

PATH = pathlib.Path(__file__).resolve().parents[1] / "check-process-soak.py"
SPEC = importlib.util.spec_from_file_location("process_soak", PATH)
soak = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(soak)


def samples():
    return [{"cycle": n, "mode": ["graceful", "forced", "crash"][n % 3],
             "host": {"rss_kib": 1000, "fds": 8}, "guest": {"rss_kib": 900, "fds": 6},
             "stop": {"cleanup_error": None, "descendants_reaped": 1, "forced": n % 3 == 1, "exit_code": 23 if n % 3 == 2 else 0},
             "stop_ms": 10, "elapsed_seconds": n + 1} for n in range(12)]


class ProcessSoakTests(unittest.TestCase):
    def check(self, data):
        return soak.evaluate(data, 12, 3, 128, 0, 8)

    def test_flat_resources_pass_and_measure_actual_work(self):
        result = self.check(samples())
        self.assertTrue(result["passed"])
        self.assertEqual(result["descendants_reaped"], 12)
        self.assertEqual(result["host_rss_slope_kib_per_cycle"], 0)

    def test_persistent_memory_leak_fails(self):
        data = samples()
        for n, sample in enumerate(data):
            sample["host"]["rss_kib"] += n * 64
        self.assertFalse(self.check(data)["passed"])

    def test_descriptor_leak_fails(self):
        data = samples()
        data[-1]["host"]["fds"] += 1
        self.assertFalse(self.check(data)["passed"])

    def test_missing_duplicate_and_unreaped_cycles_fail(self):
        for kind in ["missing", "duplicate", "unreaped", "unforced", "not_crashed"]:
            with self.subTest(kind=kind):
                data = samples()
                if kind == "missing":
                    data.pop()
                elif kind == "duplicate":
                    data[-1]["cycle"] = 0
                elif kind == "unreaped":
                    data[-1]["stop"]["descendants_reaped"] = 0
                elif kind == "unforced":
                    data[1]["stop"]["forced"] = False
                else:
                    data[2]["stop"]["exit_code"] = 0
                with self.assertRaises(ValueError):
                    self.check(data)

    def test_timeout_cleanup_reaps_child_in_separate_process_group(self):
        import subprocess
        import sys
        process = subprocess.Popen(
            [sys.executable, "-c", "import subprocess; p = subprocess.Popen(['/bin/sleep', '300'], start_new_session=True); print(p.pid, flush=True); p.wait()"],
            stdout=subprocess.PIPE, text=True, start_new_session=True)
        try:
            pid = int(process.stdout.readline())
            soak.stop_workload(process)
            self.assertIsNotNone(process.returncode)
            self.assertFalse(pathlib.Path(f"/proc/{pid}").exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            process.stdout.close()
