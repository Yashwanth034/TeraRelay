#!/usr/bin/env python3
"""Tiny owned project syscalls for the opt-in native live Telegram example.

Only run through drive_project_live_qa; it supplies a dedicated, private mount.
No network packages: npm installs one local fixture dependency only.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import time

# Explicit opt-in only: let the AI connector operate on the actual live mount.
external_edit = os.environ.get("TERARELAY_PROJECT_AI_EDIT_QA") == "1"

# npm derives executable modes from the process umask. Keep this exact-mode
# fixture deterministic without changing the caller's shell settings.
os.umask(0o022)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--mount", required=True)
parser.add_argument("--receipt-root", required=True)
parser.add_argument("--phase", choices=("create", "verify"), required=True)
args = parser.parse_args()
receipt_root = Path(args.receipt_root).resolve()
mount = Path(args.mount).resolve()
assert mount.is_mount(), "Expected dedicated native FUSE mount"
root = mount / "project"
npm_env = dict(os.environ, npm_config_cache=str(receipt_root / "npm-cache"), npm_config_update_notifier="false")
files = {
    "package.json": json.dumps({"name": "trqa-project", "version": "1.0.0", "private": True,
        "scripts": {"test": "node index.js"}, "dependencies": {"trqa-hello": "file:./packages/hello"}}, separators=(",", ":")) + "\n",
    "index.js": "console.log(require('trqa-hello'));\n",
    "packages/hello/package.json": json.dumps({"name": "trqa-hello", "version": "1.0.0", "main": "index.js",
        "bin": {"trqa-hello": "bin/hello.js"}}, separators=(",", ":")) + "\n",
    "packages/hello/index.js": "module.exports = 'TRQA_PROJECT_OK';\n",
    "packages/hello/bin/hello.js": "#!/usr/bin/env node\nconsole.log('TRQA_BIN_OK');\n",
}
benchmark = os.environ.get("TERARELAY_PROJECT_BENCH_QA") == "1"
if benchmark:
    # Bounded real bytes, not sparse-file or nominal-size claims.
    for index in range(8):
        files[f"bench/source-{index:02d}.txt"] = (f"TRQA_BENCH_{index:02d}\n" * 12000)[:131072]
metadata_seconds = None
started = time.monotonic()
if args.phase == "create":
    root.mkdir()
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
    if benchmark:
        metadata_started = time.monotonic()
        for index in range(512):
            (root / "bench" / f"dir-{index:04d}").mkdir()
        for index in range(128):
            (root / "bench" / f"link-{index:04d}").symlink_to(f"source-{index % 8:02d}.txt")
        metadata_seconds = time.monotonic() - metadata_started
    os.chmod(root / "packages/hello/bin/hello.js", 0o755)
    (root / "alias.js").symlink_to("packages/hello/index.js")
    result = subprocess.run(["npm", "install", "--offline", "--ignore-scripts", "--package-lock=false",
        "--no-audit", "--no-fund"], cwd=root, env=npm_env, capture_output=True, text=True, timeout=150)
    assert result.returncode == 0, "Offline local fixture npm install failed: " + (result.stderr or result.stdout)[-1500:]
    # Existing finalized file: fchmod on a write handle, without changing bytes.
    fd = os.open(root / "index.js", os.O_WRONLY)
    try:
        os.fchmod(fd, 0o640)
    finally:
        os.close(fd)

if external_edit:
    if args.phase == "create":
        ready = receipt_root / "external-edit-ready.json"
        ready.write_text(json.dumps({"project": str(root), "ready": True}))
        deadline = time.monotonic() + 600
        while not (receipt_root / "external-edit-done.json").is_file():
            assert time.monotonic() < deadline, "External AI edit timed out"
            time.sleep(0.2)
        assert json.loads((receipt_root / "external-edit-done.json").read_text())["pass"] is True
    files["index.js"] = "console.log(require('trqa-hello'));\nconsole.log('TRQA_AI_EDIT_OK');\n"
    files["ai-workspace.txt"] = "AI_WORKSPACE_EDIT_OK\n"

assert stat.S_IMODE((root / "index.js").stat().st_mode) == 0o640
assert (root / "alias.js").is_symlink()
assert os.readlink(root / "alias.js") == "packages/hello/index.js"
assert (root / "node_modules/.bin/trqa-hello").is_symlink(), "npm bin link must be a persistent native symlink"
assert stat.S_IMODE((root / "packages/hello/bin/hello.js").stat().st_mode) == 0o755
if benchmark:
    assert len(list((root / "bench").iterdir())) == 648
    for index in range(512):
        assert (root / "bench" / f"dir-{index:04d}").is_dir()
    for index in range(128):
        link = root / "bench" / f"link-{index:04d}"
        assert link.is_symlink() and os.readlink(link) == f"source-{index % 8:02d}.txt"
read_started = time.monotonic()
for relative, content in files.items():
    assert (root / relative).read_bytes() == content.encode(), "Project bytes differ: " + relative
read_seconds = time.monotonic() - read_started
result = subprocess.run(["npm", "test", "--offline"], cwd=root, env=npm_env, capture_output=True, text=True, timeout=60)
assert result.returncode == 0 and "TRQA_PROJECT_OK" in result.stdout, "npm test failed after native read"
result = subprocess.run([str(root / "node_modules/.bin/trqa-hello")], cwd=root,
    capture_output=True, text=True, timeout=30)
assert result.returncode == 0 and result.stdout.strip() == "TRQA_BIN_OK", "Executable npm bin link failed"
print(json.dumps({"phase": args.phase, "npm_test": True, "executable_bin": True,
    "chmod_0755": True, "relative_symlinks": True,
    "benchmark": benchmark, "benchmark_bytes": 1048576 if benchmark else 0,
    "benchmark_directories": 512 if benchmark else 0, "benchmark_symlinks": 128 if benchmark else 0,
    "metadata_create_seconds": metadata_seconds, "source_read_seconds": read_seconds,
    "elapsed_seconds": time.monotonic() - started,
    "source_sha256": {name: hashlib.sha256(content.encode()).hexdigest() for name, content in files.items()}}))
