#!/usr/bin/env python3
import ast
import hashlib
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
source = ROOT / "app/src-tauri/src/tdlib_fast_worker.py"
tree = ast.parse(source.read_text())
method = next(n for c in tree.body if isinstance(c, ast.ClassDef)
              for n in c.body if isinstance(n, ast.FunctionDef) and n.name == "download")
events = []

class Clock:
    def __init__(self):
        self.now = 0.0
    def monotonic(self):
        return self.now
    def sleep(self, seconds):
        self.now += seconds

class TD:
    def __init__(self, root, clock, cached):
        self.root, self.clock = root, clock
        self.content = {1: b"abcdefghijklmnop", 2: b"ABCDEFGHIJKLMNOP"}
        self.cached = {1: cached, 2: cached}
        self.pending = []
        self.paths = {k: root / ("part" + str(k)) for k in self.content}
        for k, p in self.paths.items():
            p.write_bytes(self.content[k][:cached])
    def file(self, k):
        count = min(len(self.content[k]), self.cached[k] + int(self.clock.now * 8))
        self.paths[k].write_bytes(self.content[k][:count])
        return {"id": k, "size": len(self.content[k]), "local": {
            "path": str(self.paths[k]), "downloaded_size": count,
            "is_downloading_completed": count == len(self.content[k])}}
    def request(self, request, **kwargs):
        kind = request["@type"]
        if kind == "getMessage":
            k = request["message_id"] >> 20
            return {"content": {"@type": "messageDocument",
                "document": {"document": self.file(k)}}}
        if kind in ("downloadFile", "getFile"):
            return self.file(request["file_id"])
        raise AssertionError("Unexpected TDLib request: " + kind)
    def send(self, request):
        self.pending.append(dict(request))
    def pop_update(self):
        if not self.pending:
            return None
        request = self.pending.pop(0)
        return {"@type": "file", "@extra": request["@extra"],
                **self.file(request["file_id"])}
    def receive(self, seconds):
        self.clock.sleep(seconds)
        return None

class SlowSnapshotTD(TD):
    def request(self, request, **kwargs):
        if request["@type"] == "getFile":
            self.clock.sleep(2.0)
        return super().request(request, **kwargs)
    def pop_update(self):
        return None
    def receive(self, seconds):
        self.clock.sleep(seconds)
        k = 1 if int(self.clock.now * 10) % 2 else 2
        return {"@type": "updateFile", "file": self.file(k)}

class SpeedLimitedTD(TD):
    def pop_update(self):
        if not getattr(self, "notice_sent", False):
            self.notice_sent = True
            return {"@type": "updateSpeedLimitNotification", "is_upload": False}
        return super().pop_update()

class DownloadProgressTests(unittest.TestCase):
    def run_download(self, cached, td_type=TD, corrupt=False):
        events.clear()
        clock = Clock()
        recorded = []
        def record(event):
            events.append(event)
            recorded.append((clock.now, event))
        namespace = {"Path": Path, "hashlib": hashlib, "time": clock, "emit": record}
        module = ast.Module(body=[method], type_ignores=[])
        exec(compile(ast.fix_missing_locations(module), str(source), "exec"), namespace)
        class Worker:
            ready = True
            def destination_chat(self, destination):
                return 1
            def network_file_totals(self):
                self.stats_calls += 1
                clock.sleep(2.0)
                return (0, 0)  # Aggregated statistics can lag actual file bytes.
        Worker.download = namespace["download"]
        with tempfile.TemporaryDirectory(prefix="terarelay-download-test-") as folder:
            root = Path(folder)
            w = Worker()
            w.stats_calls = 0
            w.td = td_type(root, clock, cached)
            chunks = [{"message_id": k, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                      for k, data in w.td.content.items()]
            if corrupt:
                chunks[0]["sha256"] = "0" * 64
                with self.assertRaisesRegex(RuntimeError, "SHA-256 verification"):
                    w.download(7, {"path": str(root / "result.bin"), "chunks": chunks})
                self.assertTrue(all(e.get("downloaded", 0) < 32 for e in events),
                                "Unverified output was reported as fully complete")
                return
            outcome = w.download(7, {"path": str(root / "result.bin"), "chunks": chunks})
            self.assertEqual((root / "result.bin").read_bytes(), b"abcdefghijklmnopABCDEFGHIJKLMNOP")
            progress = [e["downloaded"] for e in events if e["event"] == "progress"]
            self.assertEqual(progress, sorted(set(progress)))
            self.assertEqual(progress[-1], 32)
            traffic = [e["delta"] for e in events if e["event"] == "network"]
            self.assertEqual(sum(traffic), 32 - cached * 2,
                             "Download speed missed received file bytes while global statistics were stale")
            if cached < 16:
                self.assertGreater(len(traffic), 2, "Received-byte samples were delivered only at completion")
            else:
                self.assertEqual(outcome["average_bytes_per_sec"], 0, "Cached file reported network throughput")
            self.assertEqual(w.stats_calls, 0, "Synchronous statistics requests delayed download progress")
            self.assertLess(outcome["network_seconds"], 3.0)
            if td_type is SpeedLimitedTD:
                self.assertEqual([e for e in events if e["event"] == "speed_limit"],
                                 [{"event": "speed_limit", "id": 7, "is_upload": False}])
            if td_type is SlowSnapshotTD:
                times = [t for t, e in recorded if e["event"] == "progress"]
                self.assertLessEqual(max(b - a for a, b in zip(times, times[1:])), 0.6,
                                     "Blocking snapshots froze incoming download updates")
                self.assertTrue(any(e["event"] == "heartbeat" for _, e in recorded),
                                "Quiet downloads cannot wake the bridge for cancellation")
    def test_fresh_download_uses_confirmed_file_bytes_for_speed(self):
        self.run_download(0)
    def test_resumed_download_excludes_cached_bytes_from_speed(self):
        self.run_download(4)
    def test_fully_cached_download_does_not_report_fake_network_speed(self):
        self.run_download(16)
    def test_slow_snapshots_do_not_block_received_byte_updates(self):
        self.run_download(0, SlowSnapshotTD)
    def test_integrity_failure_is_never_presented_as_complete(self):
        self.run_download(0, corrupt=True)
    def test_download_server_speed_limit_notice_is_reported(self):
        self.run_download(0, SpeedLimitedTD)

if __name__ == "__main__":
    unittest.main()
