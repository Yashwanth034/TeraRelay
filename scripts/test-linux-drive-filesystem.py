#!/usr/bin/env python3
"""Exercise a real Linux FUSE mount belonging to an isolated debug QA process."""
import argparse
import ctypes
import errno
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--mount", required=True, type=Path)
parser.add_argument("--qa-pid", required=True, type=int)
args = parser.parse_args()
project = Path(__file__).resolve().parent.parent
env = dict(item.split("=", 1) for item in Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode().split("\0") if "=" in item)
if env.get("TERARELAY_QA_FEATURE_A") != "1":
    raise RuntimeError("Refusing to test a real account")
mount = args.mount.resolve(strict=True)
if Path(env["TERARELAY_DRIVE_MOUNT_PATH"]).resolve(strict=True) != mount:
    raise RuntimeError("Mount path does not belong to the QA process")
data = Path(env["XDG_DATA_HOME"]).resolve(strict=True)
if not mount.is_relative_to(project) or not data.is_relative_to(project):
    raise RuntimeError("QA paths must be inside the approved project")
db = (data / "io.terarelay.desktop/shares.db").resolve(strict=True)
if not db.is_relative_to(project):
    raise RuntimeError("QA database must be inside the approved project")
with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
    if conn.execute("SELECT count(*) FROM logical_channels WHERE logical_id='qa-feature-a-movies'").fetchone()[0] != 1:
        raise RuntimeError("Isolated QA fixture was not found")
if not os.path.ismount(mount):
    raise RuntimeError("QA Drive is not mounted")
case = mount / "Archive QA" / ("qa-fs-" + uuid.uuid4().hex)
case.mkdir()
checks = 0

def check(name, condition):
    global checks
    assert condition, name
    checks += 1
    print("PASS", name, flush=True)

try:
    nested = case / "日本語 folder"
    nested.mkdir()
    check("nested Unicode directory creation and listing", nested.name in os.listdir(case))
    payload = bytes(range(256)) * 128
    file = nested / "read-write.bin"
    check("new file write accepts every byte", file.write_bytes(payload) == len(payload))
    check("closed file reads back byte-for-byte", file.read_bytes() == payload)
    with file.open("r+b") as stream:
        stream.seek(100)
        stream.write(b"updated")
        stream.flush()
        os.fsync(stream.fileno())
    expected = payload[:100] + b"updated" + payload[107:]
    check("offset writes preserve untouched ranges", file.read_bytes() == expected)
    with file.open("r+b") as stream:
        stream.truncate(777)
    expected = expected[:777]
    check("truncate shrinks the file accurately", file.read_bytes() == expected)
    with file.open("r+b") as stream:
        stream.seek(65536)
        stream.write(b"tail")
    expected = expected + bytes(65536 - len(expected)) + b"tail"
    check("sparse growth zero-fills unwritten bytes", file.read_bytes() == expected)
    with file.open("r+b") as first, file.open("r+b") as second:
        first.seek(0)
        first.write(b"first")
        first.flush()
        second.seek(10)
        second.write(b"second")
        second.flush()
    expected = b"first" + expected[5:10] + b"second" + expected[16:]
    check("multiple writer handles preserve both changes", file.read_bytes() == expected)
    # Reopen an already-staged file with O_TRUNC, as editors and shell redirects
    # do when replacing a document. Old trailing bytes must not survive.
    replaced = nested / "replace-pending.bin"
    replaced.write_bytes(b"old pending bytes that must disappear")
    fd = os.open(replaced, os.O_WRONLY | os.O_TRUNC)
    with os.fdopen(fd, "wb") as stream:
        stream.write(b"new")
        stream.flush()
        os.fsync(stream.fileno())
    check("O_TRUNC replaces unfinished staged file without stale trailing bytes", replaced.read_bytes() == b"new")
    renamed = nested / "renamed.bin"
    file.rename(renamed)
    check("file rename preserves content and removes its old path", not file.exists() and renamed.read_bytes() == expected)
    moved_dir = case / "moved-directory"
    nested.rename(moved_dir)
    moved = moved_dir / renamed.name
    check("nonempty directory rename preserves descendants", moved.read_bytes() == expected and not nested.exists())
    destination = case / "destination.bin"
    destination.write_bytes(b"old")
    os.replace(moved, destination)
    check("replace-existing rename is atomic and keeps source content", not moved.exists() and destination.read_bytes() == expected)
    empty = case / "empty.txt"
    empty.write_bytes(b"")
    check("zero-byte files can be opened and read", empty.stat().st_size == 0 and empty.read_bytes() == b"")
    libc = ctypes.CDLL(None, use_errno=True)
    if hasattr(libc, "renameat2"):
        result = libc.renameat2(-100, os.fsencode(destination), -100, os.fsencode(empty), 1)
        check("rename-no-replace rejects an existing destination", result == -1 and ctypes.get_errno() == errno.EEXIST and destination.read_bytes() == expected and empty.read_bytes() == b"")
    try:
        moved_dir.rmdir()
    except OSError as error:
        raise AssertionError("empty directory removal failed") from error
    check("empty directory removal succeeds", not moved_dir.exists())
    nonempty = case / "nonempty"
    nonempty.mkdir()
    child = nonempty / "child.txt"
    child.write_bytes(b"child")
    try:
        nonempty.rmdir()
    except OSError as error:
        check("nonempty directory removal fails without deleting its child", error.errno == errno.ENOTEMPTY and child.read_bytes() == b"child")
    else:
        raise AssertionError("nonempty rmdir silently deleted contents")
    link = case / "metadata-link"
    target = "./" + destination.name
    os.symlink(target, link)
    link_stat = link.lstat()
    check("symlink is a link rather than an ordinary hydrated file",
          stat.S_ISLNK(link_stat.st_mode) and not stat.S_ISREG(link_stat.st_mode)
          and link_stat.st_size == len(target.encode("utf-8")) and link_stat.st_blocks == 0)
    check("readlink preserves the exact opaque target", os.readlink(link) == target)
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
        record = conn.execute("SELECT directory_id,target FROM drive_symlinks "
                              "WHERE display_name=? AND trashed_at IS NULL", (link.name,)).fetchall()
        check("symlink has only its metadata record and no staged or content entry",
              len(record) == 1 and record[0][1] == target
              and conn.execute("SELECT count(*) FROM drive_pending_files WHERE directory_id IS ? AND display_name=?",
                               (record[0][0], link.name)).fetchone()[0] == 0
              and conn.execute("SELECT count(*) FROM drive_file_entries WHERE directory_id IS ? AND display_name=? AND trashed_at IS NULL",
                               (record[0][0], link.name)).fetchone()[0] == 0
              and conn.execute("SELECT count(*) FROM drive_entries_v2 WHERE directory_id IS ? AND display_name=? AND trashed_at IS NULL",
                               (record[0][0], link.name)).fetchone()[0] == 0)
    check("kernel follows the virtual link to the original file", link.read_bytes() == expected)
    link.unlink()
    check("unlink removes only the symlink and preserves its target",
          not os.path.lexists(link) and destination.read_bytes() == expected)
    capacity = os.statvfs(mount)
    check("Drive reports unknown Telegram capacity without inventing free space", capacity.f_blocks == 0 and capacity.f_bfree == 0 and capacity.f_bavail == 0 and capacity.f_bsize > 0)
    destination.unlink()
    check("file deletion removes the visible path", not destination.exists())
finally:
    # Only remove the unique directory created by this test.
    if case.exists():
        shutil.rmtree(case)
    assert not case.exists(), "QA filesystem cleanup failed"
print(f"PASS {checks} real FUSE filesystem checks; isolated test directory removed")
