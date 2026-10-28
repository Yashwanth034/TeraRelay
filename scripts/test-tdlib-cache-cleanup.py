#!/usr/bin/env python3
"""Verify saved downloads release only disposable TDLib cache bytes."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("td_worker", ROOT / "app/src-tauri/src/tdlib_fast_worker.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
spec2 = importlib.util.spec_from_file_location("download_fixture", ROOT / "scripts/test-download-progress.py")
fixture = importlib.util.module_from_spec(spec2)
spec2.loader.exec_module(fixture)

class CacheTD(fixture.TD):
    def __init__(self, root, clock, cached):
        super().__init__(root, clock, cached)
        self.deleted = []
    def request(self, request, **kwargs):
        if request["@type"] == "deleteFile":
            k = request["file_id"]
            self.deleted.append(k)
            self.paths[k].unlink(missing_ok=True)
            return {"@type": "ok"}
        return super().request(request, **kwargs)

class CleanupTests(unittest.TestCase):
    def worker(self, root):
        w = module.Worker.__new__(module.Worker)
        w.ready = True
        w.files_dir = root / "cache"
        w.files_dir.mkdir()
        clock = fixture.Clock()
        w.td = CacheTD(w.files_dir, clock, 16)
        w.destination_chat = lambda destination: 1
        return w, clock

    def run_download(self, w, clock, output, corrupt=False):
        previous_time, previous_emit = module.time, module.emit
        module.time = clock
        module.emit = lambda event: None
        self.addCleanup(setattr, module, "time", previous_time)
        self.addCleanup(setattr, module, "emit", previous_emit)
        import hashlib
        chunks = [{"message_id": k, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                  for k, data in w.td.content.items()]
        if corrupt:chunks[-1]["sha256"] = "0" * 64
        return w.download(7, {"path": str(output), "chunks": chunks})

    def test_verified_stage_keeps_cache_until_export_is_published(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            w, clock = self.worker(root)
            output = root / "saved.bin"
            self.run_download(w, clock, output)
            self.assertEqual(output.read_bytes(), b"abcdefghijklmnopABCDEFGHIJKLMNOP")
            self.assertEqual(w.td.deleted, [])
            self.assertTrue(all(p.exists() for p in w.td.paths.values()))

    def test_integrity_failure_keeps_parts_for_retry(self):
        with tempfile.TemporaryDirectory() as temp:
            w, clock = self.worker(Path(temp))
            with self.assertRaisesRegex(RuntimeError, "SHA-256"):
                self.run_download(w, clock, Path(temp) / "saved.bin", corrupt=True)
            self.assertEqual(w.td.deleted, [])
            self.assertTrue(all(p.exists() for p in w.td.paths.values()))

    def test_saved_output_cannot_be_put_in_disposable_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            w, clock = self.worker(Path(temp))
            with self.assertRaisesRegex(RuntimeError, "outside.*cache"):
                self.run_download(w, clock, w.files_dir / "saved.bin")
            self.assertEqual(w.td.deleted, [])

    def test_failed_output_flush_keeps_cache_for_retry(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as temp:
            w, clock = self.worker(Path(temp))
            with patch("os.fsync", side_effect=OSError("test disk flush failed")):
                with self.assertRaisesRegex(OSError, "disk flush failed"):
                    self.run_download(w, clock, Path(temp) / "saved.bin")
            self.assertEqual(w.td.deleted, [])
            self.assertTrue(all(p.exists() for p in w.td.paths.values()))

    def test_unauthorized_worker_cannot_run_idle_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            w, _ = self.worker(Path(temp))
            w.ready = False
            with self.assertRaisesRegex(RuntimeError, "authorized idle"):
                w.clean_file_cache()
            self.assertTrue(all(p.exists() for p in w.td.paths.values()))

    def test_idle_trim_uses_local_storage_api(self):
        with tempfile.TemporaryDirectory() as temp:
            w, _ = self.worker(Path(temp))
            class IdleTD:
                def request(self, request, **kwargs):
                    self.request_data = request
                    return {"size": 0, "count": 0}
            w.td = IdleTD()
            w.clean_file_cache()
            self.assertEqual(w.td.request_data["@type"], "optimizeStorage")
            self.assertEqual(w.td.request_data["size"], 0)
            self.assertEqual(w.td.request_data["immunity_delay"], 0)

if __name__ == "__main__":
    unittest.main()
