#!/usr/bin/env python3
import argparse
import ast
import hashlib
import http.client
import json
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--source", type=Path, default=ROOT / "app/src-tauri/src/tdlib_fast_worker.py")
parser.add_argument("--mib", type=int, choices=(128, 1024), default=128)
parser.add_argument("--repeated-pauses", action="store_true")
args = parser.parse_args()
SIZE = args.mib * 1024 * 1024
BLOCK = 256 * 1024
PAUSE_AT = 75 * 1024 * 1024
state = {"received": 0, "pause_start": None, "pause_end": None,
         "sha256": None, "pauses": []}
lock = threading.Lock()

class Sink(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_PUT(self):
        length = int(self.headers["Content-Length"])
        digest = hashlib.sha256()
        received = 0
        while received < length:
            data = self.rfile.read(min(BLOCK, length - received))
            if not data:
                break
            digest.update(data)
            received += len(data)
            with lock:
                state["received"] = received
            if received == PAUSE_AT or (args.repeated_pauses and received % PAUSE_AT == 0):
                state["pause_start"] = time.monotonic()
                time.sleep(1.0)
                state["pause_end"] = time.monotonic()
                state["pauses"].append((state["pause_start"], state["pause_end"]))
        state["sha256"] = digest.hexdigest()
        body = state["sha256"].encode()
        self.send_response(201 if received == length else 400)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

class Transfer:
    def __init__(self, port, expected):
        self.port, self.expected = port, expected
        self.started, self.completed = None, None
        self.sent = 0
        self.baseline = 987654321
        self.error = None
        self.pending = []
        self.last_update = 0.0
        self.thread = None
    def send_file(self, path):
        try:
            conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=20)
            conn.putrequest("PUT", "/test")
            conn.putheader("Content-Length", str(SIZE))
            conn.endheaders()
            started = time.monotonic()
            with Path(path).open("rb") as source:
                while data := source.read(BLOCK):
                    conn.send(data)
                    self.sent += len(data)
                    delay = self.sent / (26 * 1024 * 1024) - (time.monotonic() - started)
                    if delay > 0:
                        time.sleep(delay)
            result = conn.getresponse()
            assert result.status == 201
            assert result.read().decode() == self.expected, "Server received different file bytes"
            conn.close()
            self.completed = time.monotonic()
        except Exception as error:
            self.error = error
    def file(self):
        if self.error:
            raise self.error
        with lock:
            count = state["received"]
        return {"@type": "file", "id": 1, "remote": {"uploaded_size": count}}
    def stats(self):
        return {"@type": "networkStatistics", "entries": [
            {"@type": "networkStatisticsEntryFile",
             "sent_bytes": self.baseline + self.sent, "received_bytes": 0}]}
    def request(self, request, **kwargs):
        kind = request["@type"]
        if kind == "sendMessage":
            self.started = time.monotonic()
            self.thread = threading.Thread(target=self.send_file, args=(request["input_message_content"]["document"]["path"],), daemon=True)
            self.thread.start()
            return {"id": 42, "content": {"document": {"document": self.file()}}}
        if kind == "getFile":
            time.sleep(2.0)
            return self.file()
        if kind == "getNetworkStatistics":
            if self.started is not None:
                time.sleep(2.0)
            return self.stats()
        raise AssertionError("Unexpected TDLib request: " + kind)
    def send(self, request):
        self.pending.append((time.monotonic() + 2.0, dict(request)))
    def pop_update(self):
        if self.pending and self.pending[0][0] <= time.monotonic():
            _, request = self.pending.pop(0)
            response = self.file() if request["@type"] == "getFile" else self.stats()
            return {**response, "@extra": request["@extra"]}
        return None
    def receive(self, timeout):
        time.sleep(min(timeout, 0.02))
        if self.error:
            raise self.error
        if self.completed:
            return {"@type": "updateMessageSendSucceeded", "old_message_id": 42,
                    "message": {"id": 700}}
        if time.monotonic() - self.last_update < 0.1:
            return None
        self.last_update = time.monotonic()
        return {"@type": "updateFile", "file": self.file()}

tree = ast.parse(args.source.read_text())
worker = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "Worker")
methods = [n for n in worker.body if isinstance(n, ast.FunctionDef)
           and n.name in ("upload", "network_file_totals")]
events = []
def record(event):
    events.append((time.monotonic(), event))
    if event["event"] == "progress":
        with lock:
            assert event["uploaded"] <= state["received"], "Progress exceeded confirmed server bytes"
namespace = {"Path": Path, "time": time, "emit": record}
exec(compile(ast.fix_missing_locations(ast.Module(body=methods, type_ignores=[])),
             str(args.source), "exec"), namespace)
class Worker:
    ready = True
    upload = namespace["upload"]
    network_file_totals = namespace["network_file_totals"]
    def destination_chat(self, destination):
        return 1

server = ThreadingHTTPServer(("127.0.0.1", 0), Sink)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix="terarelay-upload-stress-") as name:
        path = Path(name) / "test.bin"
        block = os.urandom(BLOCK)
        digest = hashlib.sha256()
        with path.open("wb") as output:
            for _ in range(SIZE // BLOCK):
                output.write(block)
                digest.update(block)
        native = Transfer(server.server_port, digest.hexdigest())
        w = Worker()
        w.td = native
        started = time.monotonic()
        result = w.upload(19, {"path": str(path),
                               "timeout": max(30, SIZE / (26 * 1024 * 1024) + 30)})
        elapsed = time.monotonic() - started
        native.thread.join(timeout=2)
        assert result == {"message_id": 700, "bytes_uploaded": SIZE}
        assert state["received"] == SIZE and state["sha256"] == digest.hexdigest()
        progress = [(t, e["uploaded"]) for t,e in events if e["event"] == "progress"]
        assert [n for _,n in progress] == sorted(set(n for _,n in progress))
        assert progress[-1][1] == SIZE
        times = [t for t,n in progress if n < SIZE]
        largest_gap = max((b-a for a,b in zip(times,times[1:])), default=0)
        network_bytes = sum(e["delta"] for _,e in events if e["event"] == "network")
        assert 0 < network_bytes <= SIZE, "Historical traffic inflated this upload"
        during_pause = [e for t,e in events
                        if any(a <= t <= b for a,b in state["pauses"])]
        print(json.dumps({
            "source": "loopback HTTP; TDLib API simulated; injected 1s source pauses; NOT Telegram",
            "injected_source_pauses": len(state["pauses"]),
            "bytes_verified": SIZE,
            "http_transport_average_MB_s": round(SIZE / (native.completed-native.started) / 1e6, 3),
            "whole_operation_average_MB_s": round(SIZE / elapsed / 1e6, 3),
            "largest_progress_gap_seconds_including_source_pause": round(largest_gap, 3),
            "heartbeats_during_source_pause": sum(e["event"] == "heartbeat" for e in during_pause),
        }), flush=True)
        assert largest_gap < 1.6, "Snapshots froze progress beyond the real source pause"
        assert all(any(a <= t <= b and e["event"] == "heartbeat" for t,e in events)
                   for a,b in state["pauses"]), "Cancellation was blocked during a source pause"
finally:
    server.shutdown()
    server.server_close()
