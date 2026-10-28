#!/usr/bin/env python3
"""Measure real FUSE reads using an isolated QA profile, never a real Telegram account."""
import argparse
import errno
import json
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--mount", required=True, type=Path)
parser.add_argument("--qa-pid", required=True, type=int)
parser.add_argument("--output", required=True, type=Path)
args = parser.parse_args()
project = Path(__file__).resolve().parent.parent
env = dict(item.split("=", 1) for item in Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode().split("\0") if "=" in item)
assert env.get("TERARELAY_QA_FEATURE_A") == "1", "Refusing a real account"
mount = args.mount.resolve(strict=True)
profile = Path(env["XDG_CACHE_HOME"]).resolve(strict=True)
assert mount.is_relative_to(project) and profile.is_relative_to(project), "QA paths must stay in the approved project"
assert mount == profile / "io.terarelay.desktop/TeraRelay Drive"
assert os.path.ismount(mount), "QA Drive is not mounted"
blocks = profile / "io.terarelay.desktop/drive/blocks"
checks, failures, metrics = [], [], {}
owned = []

def check(name, action):
    try:
        action()
        checks.append(name)
        print("PASS", name, flush=True)
    except Exception as error:
        failures.append({"name": name, "error": str(error)})
        print("FAIL", name, str(error), flush=True)

def usage():
    return sum(p.stat().st_size for p in blocks.rglob("*") if p.is_file()) if blocks.exists() else 0

def browse():
    before = usage()
    files = [(p, p.stat()) for p in mount.rglob("*") if p.is_file()]
    assert files, "No remote QA files found"
    metrics["logical_bytes"] = sum(s.st_size for _, s in files)
    metrics["reported_allocated_bytes"] = sum(s.st_blocks * 512 for _, s in files)
    metrics["cache_before_browsing"] = before
    metrics["cache_after_browsing"] = usage()
    assert metrics["logical_bytes"] > 1_000_000_000
    assert usage() == before, "Folder browsing downloaded file contents"

def allocation():
    assert metrics["reported_allocated_bytes"] == 0, (
        "Remote placeholders claim local allocation: " + str(metrics["reported_allocated_bytes"]))
    result = subprocess.run(["du", "-s", "--block-size=1", str(mount)], capture_output=True, text=True, check=True)
    assert int(result.stdout.split()[0]) == 0, result.stdout

def cached_read(file_id, name, payload, expected, offset=0):
    path = blocks / file_id / "0000000000000000.bin"
    assert not path.exists(), "Refusing to overwrite an existing cache fixture"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(payload)
    owned.append(path)
    with (mount / "Movies QA" / name).open("rb", buffering=0) as f:
        actual = os.pread(f.fileno(), len(expected), offset)
    assert actual == expected, "Cached read returned incorrect bytes"

def reject_bad_cache(file_id, name, payload):
    path = blocks / file_id / "0000000000000000.bin"
    assert not path.exists(), "Refusing to overwrite an existing cache fixture"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(payload)
    owned.append(path)
    try:
        with (mount / "Movies QA" / name).open("rb", buffering=0) as f:
            actual = os.pread(f.fileno(), 128, 0)
    except OSError as error:
        assert error.errno == errno.EIO, str(error)
    else:
        raise AssertionError("Invalid cache was served as real content: " + str(len(actual)) + " bytes")

try:
    check("cold folder browsing does not acquire file contents", browse)
    check("remote logical size is not reported as local disk allocation", allocation)
    pattern = bytes(range(256)) * (4 * 1024 * 1024 // 256)
    check("valid cached range reads return exact bytes through real FUSE",
          lambda: cached_read("11111111111111111111111111111111", "Interstellar.1080p.BluRay.mkv", pattern, pattern[123:251], 123))
    check("truncated cache cannot silently shorten a requested read",
          lambda: reject_bad_cache("22222222222222222222222222222222", "Interstellar.2160p.HDR.mkv", b"bad"))
    check("oversized cache cannot be served as a valid remote block",
          lambda: reject_bad_cache("33333333333333333333333333333333", "Dune.Part.Two.2160p.mkv", bytes(4 * 1024 * 1024 + 1)))
finally:
    for path in owned:
        path.unlink(missing_ok=True)
        try:
            path.parent.rmdir()
        except OSError:
            pass
    result = {"passed": not failures, "checks": checks, "failures": failures, "metrics": metrics,
              "scope": "Real Linux FUSE with metadata/cache fixtures; no live Telegram transfer",
              "owned_cache_files_removed": all(not p.exists() for p in owned)}
    args.output.write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2), flush=True)
raise SystemExit(1 if failures else 0)
