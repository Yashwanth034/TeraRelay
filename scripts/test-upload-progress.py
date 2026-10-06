#!/usr/bin/env python3
import ast
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "app/src-tauri/src/tdlib_fast_worker.py"
worker = next(n for n in ast.parse(SOURCE.read_text()).body
              if isinstance(n, ast.ClassDef) and n.name == "Worker")
methods = [n for n in worker.body if isinstance(n, ast.FunctionDef)
           and n.name in ("upload", "network_file_totals")]

class Clock:
    def __init__(self):
        self.now = 0.0
    def monotonic(self):
        return self.now
    def sleep(self, seconds):
        self.now += seconds

class TD:
    def __init__(self, clock, size, file_delay=0, stats_delay=0, quiet=False,
                 notices=(), fail=False, unrelated=False):
        self.clock, self.size = clock, size
        self.file_delay, self.stats_delay = file_delay, stats_delay
        self.quiet, self.fail = quiet, fail
        self.started = None
        self.pending = []
        self.incoming = list(notices)
        self.baseline = 987654321
        self.calls = []
        if unrelated:
            self.incoming += [
                {"@type": "updateFile", "file": {"id": 99, "remote": {"uploaded_size": size * 2}}},
                {"@type": "updateMessageSendSucceeded", "old_message_id": 99,
                 "message": {"id": 999}}]
    def count(self):
        if self.started is None:
            return 0
        return min(self.size, int(max(0, self.clock.now - self.started) * self.size / 2))
    def file(self):
        return {"@type": "file", "id": 1, "remote": {"uploaded_size": self.count()}}
    def stats(self):
        return {"@type": "networkStatistics", "entries": [
            {"@type": "networkStatisticsEntryFile", "sent_bytes": self.baseline + self.count(),
             "received_bytes": 0}]}
    def request(self, request, **kwargs):
        kind = request["@type"]
        self.calls.append(kind)
        if kind == "sendMessage":
            self.sent_request = request
            self.started = self.clock.now
            return {"id": 42, "content": {"document": {"document": self.file()}}}
        if kind == "getFile":
            self.clock.sleep(self.file_delay)
            return self.file()
        if kind == "getNetworkStatistics":
            if self.started is not None:
                self.clock.sleep(self.stats_delay)
            return self.stats()
        raise AssertionError("Unexpected TDLib request: " + kind)
    def send(self, request):
        self.calls.append(request["@type"])
        delay = self.file_delay if request["@type"] == "getFile" else self.stats_delay
        self.pending.append((self.clock.now + delay, dict(request)))
    def pop_update(self):
        if self.incoming:
            return self.incoming.pop(0)
        for i, (due, request) in enumerate(self.pending):
            if due <= self.clock.now:
                self.pending.pop(i)
                response = self.file() if request["@type"] == "getFile" else self.stats()
                return {**response, "@extra": request["@extra"]}
        return None
    def receive(self, timeout):
        self.clock.sleep(timeout)
        elapsed = self.clock.now - self.started
        if self.fail and elapsed >= 0.5:
            return {"@type": "updateMessageSendFailed", "old_message_id": 42,
                    "error": {"message": "test server rejected the upload"}}
        if elapsed >= 2.4:
            return {"@type": "updateMessageSendSucceeded", "old_message_id": 42,
                    "message": {"id": 700}}
        if self.quiet:
            return None
        return {"@type": "updateFile", "file": self.file()}

class UploadProgressTests(unittest.TestCase):
    def run_upload(self, caption="", **options):
        clock = Clock()
        recorded = []
        def record(event):
            recorded.append((clock.now, event))
        namespace = {"Path": Path, "time": clock, "emit": record}
        exec(compile(ast.fix_missing_locations(ast.Module(body=methods, type_ignores=[])),
                     str(SOURCE), "exec"), namespace)
        class Worker:
            ready = True
            upload = namespace["upload"]
            network_file_totals = namespace["network_file_totals"]
            def destination_chat(self, destination):
                return 77
        with tempfile.TemporaryDirectory(prefix="terarelay-upload-test-") as name:
            path = Path(name) / "sample.bin"
            path.write_bytes(b"ABCD" * (128 * 1024))
            w = Worker()
            w.td = TD(clock, path.stat().st_size, **options)
            if options.get("fail"):
                with self.assertRaisesRegex(RuntimeError, "test server rejected"):
                    w.upload(9, {"path": str(path), "caption": caption})
                self.assertTrue(all(e.get("uploaded", 0) < path.stat().st_size for _, e in recorded))
                return recorded, clock, w.td
            result = w.upload(9, {"path": str(path), "caption": caption})
            self.assertEqual(result, {"message_id": 700, "bytes_uploaded": path.stat().st_size})
            progress = [e["uploaded"] for _, e in recorded if e["event"] == "progress"]
            self.assertEqual(progress, sorted(set(progress)))
            self.assertEqual(progress[-1], path.stat().st_size)
            self.assertTrue(all(n < path.stat().st_size for n in progress[:-1]))
            network = [e["delta"] for _, e in recorded if e["event"] == "network"]
            self.assertLessEqual(sum(network), path.stat().st_size,
                                 "Historical traffic was counted as this upload")
            return recorded, clock, w.td
    def test_upload_consumes_updates_without_file_or_statistics_queries(self):
        _, _, td = self.run_upload(file_delay=2, stats_delay=2)
        self.assertEqual(td.calls, ["sendMessage"],
                         "Upload must use the proven update-driven transport")

    def test_multipart_caption_is_preserved(self):
        caption = '{"name":"video.mp4","part":2,"total":3}\\nTGDISK'
        _, _, td = self.run_upload(caption=caption)
        message = td.sent_request["input_message_content"]
        self.assertEqual(td.sent_request["chat_id"], 77)
        self.assertEqual(message["caption"]["text"], caption)
        self.assertEqual(message["document"]["@type"], "inputFileLocal")
        self.assertTrue(message["document"]["path"].endswith("/sample.bin"))

    def test_transfer_rate_counts_confirmed_file_bytes(self):
        recorded, _, td = self.run_upload()
        samples = [e for _, e in recorded if e["event"] == "network"]
        self.assertTrue(samples)
        self.assertEqual(sum(e["delta"] for e in samples), td.size)
        self.assertTrue(all(e.get("measurement") == "acknowledged" for e in samples),
                        "Confirmed file bytes must not be labeled as wire traffic")

    def test_delayed_snapshots_do_not_freeze_file_updates(self):
        recorded, clock, _ = self.run_upload(file_delay=2, stats_delay=2)
        self.assertLess(clock.now, 2.8, "Snapshot waits delayed send completion")
        times = [t for t, e in recorded if e["event"] == "progress"]
        self.assertLessEqual(max(b-a for a,b in zip(times,times[1:])), 0.6)
    def test_delayed_network_statistics_do_not_block_progress(self):
        recorded, clock, _ = self.run_upload(stats_delay=2)
        self.assertLess(clock.now, 2.8, "Network statistics blocked transfer events")
        times = [t for t, e in recorded if e["event"] == "progress"]
        self.assertLessEqual(max(b-a for a,b in zip(times,times[1:])), 0.6,
                             "Statistics queries froze incoming file updates")
    def test_success_is_reported_after_the_matching_message_acknowledgement(self):
        self.run_upload()
    def test_other_file_and_message_updates_do_not_complete_this_upload(self):
        self.run_upload(unrelated=True)
    def test_send_failure_is_not_reported_as_complete(self):
        self.run_upload(fail=True)
    def test_quiet_upload_still_wakes_cancellation_reader(self):
        recorded, _, _ = self.run_upload(quiet=True)
        times = [t for t,e in recorded if e["event"] == "heartbeat"]
        self.assertGreater(len(times), 3, "Quiet uploads cannot wake cancellation")
        self.assertLessEqual(max(b-a for a,b in zip(times,times[1:])), 0.5)
    def test_server_speed_limit_notice_is_reported_without_failing_upload(self):
        recorded, _, _ = self.run_upload(notices=[
            {"@type": "updateSpeedLimitNotification", "is_upload": True}])
        notices = [e for _,e in recorded if e["event"] == "speed_limit"]
        self.assertEqual(notices, [{"event": "speed_limit", "id": 9, "is_upload": True}])
    def test_invalid_notice_does_not_fabricate_server_throttling(self):
        recorded, _, _ = self.run_upload(notices=[
            {"@type": "updateSpeedLimitNotification", "is_upload": "yes"}])
        self.assertFalse(any(e["event"] == "speed_limit" for _,e in recorded))
    def test_current_network_samples_exclude_traffic_before_this_upload(self):
        recorded, _, _ = self.run_upload()
        self.assertGreater(sum(e["delta"] for _,e in recorded if e["event"] == "network"), 0)

if __name__ == "__main__":
    unittest.main()
