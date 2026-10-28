#!/usr/bin/env python3
"""Isolated synthetic TeraRelay restore-event regression. Never uses Telegram."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import time
import uuid

NAMES = ("notify-folder", "notify-file.txt", "notify-link")

def owned_root(value):
    root = Path(value).resolve()
    repo = Path(__file__).resolve().parents[1]
    assert root.is_relative_to(repo / ".repotunnel-tmp"), "requires an owned QA run"
    assert json.loads((root / "launch.json").read_text())["mode"] == "synthetic-restore-events"
    assert (root / "mount").is_mount(), "owned test mount is not active"
    return root

def prepare(root):
    mount = root / "mount"
    assert not any(os.path.lexists(mount / name) for name in NAMES)
    (root / "target.txt").write_bytes(b"OWNED_RESTORE_TARGET\n")
    (mount / NAMES[0]).mkdir(mode=0o750)
    os.symlink("../target.txt", mount / NAMES[2])
    conn = sqlite3.connect(root / "data/io.terarelay.desktop/shares.db")
    now = int(time.time() * 1000)
    obj, entry = uuid.uuid4().hex, uuid.uuid4().hex
    conn.execute("INSERT INTO drive_objects(object_id,plaintext_size,encryption_version,content_fingerprint,block_count,created_at) VALUES(?,0,0,?,0,?)",
                 (obj, hashlib.sha256(b"").hexdigest(), now))
    conn.execute("INSERT INTO drive_entries_v2(entry_id,object_id,directory_id,display_name,created_at,updated_at) VALUES(?,?,NULL,?,?,?)",
                 (entry, obj, NAMES[1], now, now))
    conn.commit()
    conn.close()
    os.chmod(mount / NAMES[1], 0o640)
    before = {name: {"inode": (mount / name).lstat().st_ino,
                     "mode": stat.S_IMODE((mount / name).lstat().st_mode)}
              for name in NAMES}
    (mount / NAMES[0]).rmdir()
    (mount / NAMES[1]).unlink()
    (mount / NAMES[2]).unlink()
    assert not any(os.path.lexists(mount / name) for name in NAMES)
    before["target_sha256"] = hashlib.sha256((root / "target.txt").read_bytes()).hexdigest()
    (root / "before.json").write_text(json.dumps(before, indent=2))
    print("PREPARED", flush=True)

def watch(root, seconds):
    import gi
    gi.require_version("Gio", "2.0")
    from gi.repository import Gio, GLib
    events = []
    loop = GLib.MainLoop()
    monitor = Gio.File.new_for_path(str(root / "mount")).monitor_directory(
        Gio.FileMonitorFlags.WATCH_MOVES, None)
    arrived = set()
    def changed(_monitor, file, other, event):
        name = file.get_basename()
        events.append({"name": name, "event": event.value_nick,
                       "other": other.get_basename() if other else None})
        if event == Gio.FileMonitorEvent.CREATED and name in NAMES:
            arrived.add(name)
        if arrived == set(NAMES):
            GLib.timeout_add(300, lambda: (loop.quit(), False)[1])
    monitor.connect("changed", changed)
    (root / "monitor-ready.json").write_text(json.dumps({"ready": True, "pid": os.getpid()}))
    GLib.timeout_add_seconds(seconds, lambda: (loop.quit(), False)[1])
    loop.run()
    monitor.cancel()
    (root / "events.json").write_text(json.dumps(events, indent=2))
    missing = sorted(set(NAMES) - arrived)
    (root / "event-result.json").write_text(json.dumps({"pass": not missing, "missing_created_events": missing}, indent=2))
    assert not missing, "restore did not emit native creation events: " + repr(missing)
    verify(root)
    print("RESTORE_EVENTS_OK", flush=True)

def verify(root):
    before = json.loads((root / "before.json").read_text())
    mount = root / "mount"
    for name in NAMES:
        info = (mount / name).lstat()
        assert info.st_ino == before[name]["inode"], "inode changed: " + name
        assert stat.S_IMODE(info.st_mode) == before[name]["mode"], "mode changed: " + name
    assert (mount / NAMES[0]).is_dir()
    assert (mount / NAMES[1]).is_file() and (mount / NAMES[1]).stat().st_size == 0
    assert (mount / NAMES[2]).is_symlink()
    assert os.readlink(mount / NAMES[2]) == "../target.txt"
    assert hashlib.sha256((root / "target.txt").read_bytes()).hexdigest() == before["target_sha256"]
    (root / "integrity-result.json").write_text(json.dumps({"pass": True, "checked": list(NAMES)}, indent=2))
    print("RESTORE_INTEGRITY_OK", flush=True)

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("prepare", "watch", "verify"))
    parser.add_argument("root")
    parser.add_argument("--seconds", type=int, default=45)
    args = parser.parse_args()
    root = owned_root(args.root)
    if args.action == "watch":
        watch(root, args.seconds)
    else:
        globals()[args.action](root)
