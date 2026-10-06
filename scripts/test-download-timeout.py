#!/usr/bin/env python3
"""Exercise the production download method with only TDLib/bridge timing replaced."""
import ast
import hashlib
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "app/src-tauri/src/tdlib_fast_worker.py"
TREE = ast.parse(SOURCE.read_text())
METHOD = next(node for cls in TREE.body if isinstance(cls, ast.ClassDef)
              for node in cls.body if isinstance(node, ast.FunctionDef) and node.name == "download")
PAYLOAD = b"download payload"


class Clock:
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class TimedTD:
    def __init__(self, root, clock, interval, progress_steps=None, cached=0, unrelated=False):
        self.path = root / "cached-document.bin"
        self.clock, self.interval = clock, interval
        self.received, self.steps = cached, 0
        self.progress_steps = progress_steps
        self.unrelated = unrelated
        self.pending = []
        self.path.write_bytes(PAYLOAD[:cached])

    def file(self):
        return {"@type": "file", "id": 11, "size": len(PAYLOAD), "local": {
            "path": str(self.path), "downloaded_size": self.received,
            "is_downloading_completed": self.received == len(PAYLOAD),
        }}

    def request(self, request, **kwargs):
        kind = request["@type"]
        if kind == "getMessage":
            assert request["message_id"] == 7 << 20
            return {"content": {"@type": "messageDocument", "document": {"document": self.file()}}}
        if kind in ("downloadFile", "getFile"):
            assert request["file_id"] == 11
            return self.file()
        raise AssertionError("Unexpected TDLib request: " + kind)

    def send(self, request):
        self.pending.append(request.copy())

    def pop_update(self):
        if not self.pending:
            return None
        request = self.pending.pop(0)
        return {**self.file(), "@extra": request["@extra"]}

    def receive(self, seconds):
        self.clock.sleep(self.interval)
        self.steps += 1
        if self.progress_steps is None or self.steps <= self.progress_steps:
            self.received = min(len(PAYLOAD), self.received + 1)
            self.path.write_bytes(PAYLOAD[:self.received])
            return {"@type": "updateFile", "file": self.file()}
        if self.unrelated:
            return {"@type": "updateFile", "file": {
                "@type": "file", "id": 99, "size": 100000,
                "local": {"downloaded_size": self.steps * 100, "is_downloading_completed": False},
            }}
        return {"@type": "updateFile", "file": self.file()}


class BridgeCancelled(RuntimeError):
    pass


class DownloadTimeoutTests(unittest.TestCase):
    def make_worker(self, root, clock, interval, progress_steps=None, cached=0, unrelated=False, cancel=False):
        events = []
        def emit(event):
            events.append(event.copy())
            # NativeWorker::request rechecks cancel_rx whenever a worker line arrives.
            # Model termination at that existing bridge boundary, leaving the worker
            # heartbeat/progress production behavior under test.
            if cancel and event["event"] == "heartbeat":
                raise BridgeCancelled("Transfer cancelled")
        namespace = {"Path": Path, "time": clock, "hashlib": hashlib, "emit": emit}
        exec(compile(ast.fix_missing_locations(ast.Module(body=[METHOD], type_ignores=[])),
                     str(SOURCE), "exec"), namespace)
        class Worker:
            ready = True
            download = namespace["download"]
            def destination_chat(self, destination):
                return 1
        worker = Worker()
        worker.td = TimedTD(root, clock, interval, progress_steps, cached, unrelated)
        return worker, events

    def request(self, root, **options):
        return {
            "path": str(root / "result.bin"),
            "chunks": [{"message_id": 7, "size": len(PAYLOAD),
                        "sha256": hashlib.sha256(PAYLOAD).hexdigest()}],
            **options,
        }

    # An absolute 7200-second deadline fails although fresh bytes arrive every 1800 seconds.
    def test_confirmed_progress_can_continue_beyond_two_hours(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, events = self.make_worker(root, clock, 1800)
            try:
                outcome = worker.download(17, self.request(root, inactivity_timeout=7200))
            except TimeoutError:
                self.fail("Confirmed progressing download hit an absolute two-hour deadline")
            self.assertGreater(clock.now, 7200)
            self.assertEqual((root / "result.bin").read_bytes(), PAYLOAD)
            self.assertEqual(outcome["bytes_downloaded"], len(PAYLOAD))
            self.assertEqual(sum(e["delta"] for e in events if e["event"] == "network"), len(PAYLOAD))

    def test_legacy_timeout_parameter_uses_confirmed_progress(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, _ = self.make_worker(root, clock, 1)
            try:
                worker.download(17, self.request(root, timeout=4))
            except TimeoutError:
                self.fail("Legacy timeout killed a download that kept making byte progress")
            self.assertGreater(clock.now, 4)
            self.assertEqual((root / "result.bin").read_bytes(), PAYLOAD)

    # Heartbeats, matching same-byte snapshots and unrelated file updates cannot hide a stall.
    def test_unchanged_snapshots_and_heartbeats_do_not_extend_stalled_download(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, events = self.make_worker(root, clock, 1, progress_steps=1)
            with self.assertRaisesRegex(TimeoutError, "download"):
                worker.download(17, self.request(root, inactivity_timeout=4, timeout=100))
            self.assertEqual(clock.now, 5)  # One byte at t=1; four seconds without another byte.
            self.assertTrue(any(e["event"] == "heartbeat" for e in events))
            self.assertFalse((root / "result.bin").exists())

    def test_unrelated_file_progress_does_not_reset_tracked_download_timeout(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, _ = self.make_worker(root, clock, 1, progress_steps=1, unrelated=True)
            with self.assertRaisesRegex(TimeoutError, "download"):
                worker.download(17, self.request(root, inactivity_timeout=4, timeout=100))
            self.assertEqual(clock.now, 5)

    def test_cached_startup_bytes_do_not_keep_a_stalled_request_alive(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, events = self.make_worker(root, clock, 1, progress_steps=0, cached=8)
            with self.assertRaisesRegex(TimeoutError, "download"):
                worker.download(17, self.request(root, inactivity_timeout=4, timeout=100))
            self.assertEqual(clock.now, 4)
            self.assertEqual(sum(e["delta"] for e in events if e["event"] == "network"), 0)

    def test_bridge_cancellation_still_interrupts_progress_wait(self):
        with tempfile.TemporaryDirectory(prefix="tera-download-timeout-") as folder:
            root, clock = Path(folder), Clock()
            worker, events = self.make_worker(root, clock, 1, progress_steps=0, cancel=True)
            with self.assertRaisesRegex(BridgeCancelled, "Transfer cancelled"):
                worker.download(17, self.request(root, inactivity_timeout=7200))
            self.assertLess(clock.now, 7200)
            self.assertFalse((root / "result.bin").exists())
            self.assertTrue(any(e["event"] == "heartbeat" for e in events))


if __name__ == "__main__":
    unittest.main()
