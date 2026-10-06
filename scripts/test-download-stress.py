#!/usr/bin/env python3
import argparse
import ast
import hashlib
import json
import os
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--source", type=Path, default=ROOT / "app/src-tauri/src/tdlib_fast_worker.py")
parser.add_argument("--part-mib", type=int, choices=(64, 512), default=64)
args = parser.parse_args()
PART_SIZE = args.part_mib * 1024 * 1024
BLOCK_SIZE = 256 * 1024
blocks = {1: os.urandom(BLOCK_SIZE), 2: os.urandom(BLOCK_SIZE)}
expected_hashes = {}
combined = hashlib.sha256()
for k in (1, 2):
    digest = hashlib.sha256()
    for _ in range(PART_SIZE // BLOCK_SIZE):
        digest.update(blocks[k])
        combined.update(blocks[k])
    expected_hashes[k] = digest.hexdigest()

class Source(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        k = int(self.path.strip("/"))
        self.send_response(200)
        self.send_header("Content-Length", str(PART_SIZE))
        self.end_headers()
        started = time.monotonic()
        for i in range(PART_SIZE // BLOCK_SIZE):
            self.wfile.write(blocks[k])
            self.wfile.flush()
            delay = (i + 1) * BLOCK_SIZE / (16 * 1024 * 1024) - (time.monotonic() - started)
            if delay > 0:
                time.sleep(delay)

class NativeTransfer:
    def __init__(self, folder, address):
        self.folder, self.address = folder, address
        self.lock = threading.Lock()
        self.received = {1: 0, 2: 0}
        self.completed = {}
        self.pending = []
        self.threads = []
        self.error = None
        self.first_request = None
        self.last_update = 0.0
        self.next_file = 1
    def fetch(self, k):
        try:
            with urllib.request.urlopen(self.address + "/" + str(k), timeout=10) as source:
                with (self.folder / str(k)).open("wb") as output:
                    while True:
                        data = source.read(BLOCK_SIZE)
                        if not data:
                            break
                        output.write(data)
                        with self.lock:
                            self.received[k] += len(data)
            with self.lock:
                self.completed[k] = time.monotonic()
        except Exception as error:
            self.error = error
    def file(self, k):
        if self.error:
            raise self.error
        with self.lock:
            received = self.received[k]
            complete = k in self.completed
        return {"@type": "file", "id": k, "size": PART_SIZE, "local": {
            "path": str(self.folder / str(k)), "downloaded_size": received,
            "is_downloading_completed": complete}}
    def request(self, request, **kwargs):
        kind = request["@type"]
        if kind == "getMessage":
            k = request["message_id"] >> 20
            return {"content": {"@type": "messageDocument", "document": {"document": self.file(k)}}}
        k = request["file_id"]
        if kind == "downloadFile":
            self.first_request = self.first_request or time.monotonic()
            thread = threading.Thread(target=self.fetch, args=(k,), daemon=True)
            self.threads.append(thread)
            thread.start()
        elif kind == "getFile":
            time.sleep(2.0)
        else:
            raise AssertionError("Unexpected request " + kind)
        return self.file(k)
    def send(self, request):
        self.pending.append((time.monotonic() + 2.0, dict(request)))
    def pop_update(self):
        if self.pending and self.pending[0][0] <= time.monotonic():
            _, request = self.pending.pop(0)
            return {**self.file(request["file_id"]), "@extra": request["@extra"]}
        return None
    def receive(self, timeout):
        time.sleep(min(timeout, 0.02))
        if time.monotonic() - self.last_update < 0.1:
            return None
        self.last_update = time.monotonic()
        k = self.next_file
        self.next_file = 3 - k
        return {"@type": "updateFile", "file": self.file(k)}

tree = ast.parse(args.source.read_text())
method = next(n for c in tree.body if isinstance(c, ast.ClassDef)
              for n in c.body if isinstance(n, ast.FunctionDef) and n.name == "download")
events = []
started = time.monotonic()
namespace = {"Path": Path, "time": time, "hashlib": hashlib,
             "emit": lambda event: events.append((time.monotonic() - started, event))}
exec(compile(ast.fix_missing_locations(ast.Module(body=[method], type_ignores=[])),
             str(args.source), "exec"), namespace)
class Worker:
    ready = True
    download = namespace["download"]
    def destination_chat(self, destination):
        return 1

server = ThreadingHTTPServer(("127.0.0.1", 0), Source)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix="terarelay-download-stress-") as name:
        folder = Path(name)
        worker = Worker()
        native = worker.td = NativeTransfer(folder, "http://127.0.0.1:" + str(server.server_port))
        chunks = [{"message_id": k, "size": PART_SIZE, "sha256": expected_hashes[k]} for k in (1, 2)]
        outcome = worker.download(17, {"path": str(folder / "output.bin"), "chunks": chunks,
                                       "timeout": max(30, PART_SIZE / (16 * 1024 * 1024) + 30)})
        elapsed = time.monotonic() - started
        for thread in native.threads:
            thread.join(timeout=2)
        wire_seconds = max(native.completed.values()) - native.first_request
        actual = hashlib.sha256()
        with (folder / "output.bin").open("rb") as output:
            while data := output.read(4 * 1024 * 1024):
                actual.update(data)
        assert actual.hexdigest() == combined.hexdigest(), "Reconstruction changed real transferred bytes"
        total = 2 * PART_SIZE
        assert outcome["bytes_downloaded"] == total
        assert sum(e["delta"] for _, e in events if e["event"] == "network") == total
        progress = [(t, e["downloaded"]) for t, e in events if e["event"] == "progress"]
        assert [n for _, n in progress] == sorted(set(n for _, n in progress))
        assert progress[-1][1] == total
        active_times = [t for t, n in progress if n < total]
        largest_gap = max((b - a for a, b in zip(active_times, active_times[1:])), default=0)
        buckets = {}
        for t, event in events:
            if event["event"] == "network":
                key = int(t)
                buckets[key] = buckets.get(key, 0) + event["delta"]
        print(json.dumps({
            "source": "loopback HTTP; TDLib response timing simulated; NOT a Telegram speed measurement",
            "bytes_verified": total,
            "http_transport_average_MB_s": round(total / wire_seconds / 1e6, 3),
            "reported_average_MB_s": round(outcome["average_bytes_per_sec"] / 1e6, 3),
            "whole_operation_average_MB_s": round(total / elapsed / 1e6, 3),
            "largest_active_progress_gap_seconds": round(largest_gap, 3),
            "received_MB_by_second": {k: round(v / 1e6, 3) for k, v in buckets.items()},
        }), flush=True)
        assert largest_gap < 1.0, "Snapshot waiting froze progress while the HTTP transfer continued"
finally:
    server.shutdown()
    server.server_close()
