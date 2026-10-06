#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

python3 - <<'PY'
from pathlib import Path

files = {
    "upload_hook": Path("app/src/hooks/useFileUpload.ts").read_text(),
    "download_hook": Path("app/src/hooks/useFileDownload.ts").read_text(),
    "upload_queue": Path("app/src/components/desktop/dashboard/UploadQueue.tsx").read_text(),
    "download_queue": Path("app/src/components/desktop/dashboard/DownloadQueue.tsx").read_text(),
    "fs": Path("app/src-tauri/src/commands/fs.rs").read_text(),
    "worker": Path("app/src-tauri/src/tdlib_fast_worker.py").read_text(),
}

required = [
    ("upload hook confirmed bytes", "uploadedBytes: event.payload.uploaded_bytes", files["upload_hook"]),
    ("upload hook backend speed", "speedBytesPerSec: event.payload.speed_bytes_per_sec", files["upload_hook"]),
    ("download hook confirmed bytes", "downloadedBytes: event.payload.uploaded_bytes", files["download_hook"]),
    ("download hook backend speed", "speedBytesPerSec: event.payload.speed_bytes_per_sec", files["download_hook"]),
    ("upload starting state", "Starting upload…", files["upload_queue"]),
    ("download starting state", "Starting download…", files["download_queue"]),
    ("upload activity indicator", "animate-progress-indeterminate", files["upload_queue"]),
    ("download activity indicator", "animate-progress-indeterminate", files["download_queue"]),
    ("separate network counter", "network_counter", files["fs"]),
    ("authoritative upload progress", "uploaded_bytes: current", files["fs"]),
    ("TDLib getFile polling", '"@type": "getFile"', files["worker"]),
    ("separate network event", '"event": "network"', files["worker"]),
]

missing = [name for name, needle, haystack in required if needle not in haystack]
if missing:
    raise SystemExit("Transfer UI contract failed: missing " + ", ".join(missing))

for name, text in (("upload hook", files["upload_hook"]), ("download hook", files["download_hook"])):
    banned = ("averageSpeed", "performance.now()", "speedBytesPerSec *")
    hits = [token for token in banned if token in text]
    if hits:
        raise SystemExit(f"Transfer UI contract failed: {name} reintroduced client-side speed/progress estimation: {hits}")

print("PASS transfer UI contract")
print("  confirmed bytes: backend authoritative counters")
print("  speed: separate backend network counter")
print("  zero-byte active state: indeterminate/Starting state")
print("  no frontend speed-derived progress estimation")
PY
