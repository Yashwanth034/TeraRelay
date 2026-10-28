#!/usr/bin/env python3
"""Native Linux FUSE regression for copy-on-write pending unlink semantics.

Runs only against an already-mounted private synthetic QA Drive supplied by the
caller. It never launches or touches the normal installed app/profile.
"""
from pathlib import Path
import argparse
import errno
import json
import os
import sqlite3
import time
import traceback


def active_legacy_bases(db, limit=4):
    return db.execute(
        """SELECT e.entry_id,e.directory_id,e.display_name,
                  COALESCE(e.plaintext_size,f.total_size)
           FROM drive_file_entries e
           JOIN logical_files f ON f.file_id=e.file_id
           WHERE e.trashed_at IS NULL AND f.status='complete'
             AND NOT EXISTS(
               SELECT 1 FROM drive_pending_files p
               WHERE p.base_entry_id=e.entry_id
             )
           ORDER BY CASE WHEN e.display_name='TeraRelay-Feature-A-notes.pdf'
                         THEN 0 ELSE 1 END,
                    4 ASC
           LIMIT ?""",
        (limit,),
    ).fetchall()


def mounted_target(mount, db, row):
    directories = {
        record[0]: (record[1], record[2])
        for record in db.execute(
            "SELECT id,parent_id,name FROM drive_directories WHERE trashed_at IS NULL"
        )
    }
    parts = []
    current = row[1]
    seen = set()
    while current is not None:
        assert current not in seen, "directory cycle in synthetic fixture"
        seen.add(current)
        parent, name = directories[current]
        parts.append(name)
        current = parent
    return mount.joinpath(*reversed(parts), row[2])


def edit_pending(db, path, entry_id, data):
    with path.open("wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    pending = db.execute(
        "SELECT id,base_entry_id,staging_path,size "
        "FROM drive_pending_files WHERE base_entry_id=?",
        (entry_id,),
    ).fetchone()
    assert pending and pending[1] == entry_id, "edit did not create COW pending"
    assert pending[3] == len(data), "pending size differs from written bytes"
    assert path.read_bytes() == data
    return pending


def base_trashed(db, entry_id):
    row = db.execute(
        "SELECT trashed_at FROM drive_file_entries WHERE entry_id=?", (entry_id,)
    ).fetchone()
    return row is not None and row[0] is not None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mount", type=Path, required=True)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = {
        "pass": False,
        "synthetic_only": True,
        "cloud_transfer_tested": False,
        "normal_profile_used": False,
        "checks": {},
    }
    db = None
    descriptors = []
    try:
        mount = args.mount.resolve(strict=True)
        assert os.path.ismount(mount), f"not mounted: {mount}"
        db = sqlite3.connect(args.db, timeout=5, isolation_level=None)
        bases = active_legacy_bases(db)
        assert len(bases) >= 2, "need two active legacy base entries"

        first = bases[0]
        entry_id, _, display_name, base_size = first
        target = mounted_target(mount, db, first)
        data = b"pending COW unlink must remove the namespace entry\n"
        pending = edit_pending(db, target, entry_id, data)
        pending_id, _, staging_path, _ = pending
        stage = Path(staging_path).parent
        marker = stage.parent / ".retained-readers" / pending_id
        descriptor = os.open(target, os.O_RDONLY)
        descriptors.append(descriptor)
        inode = os.fstat(descriptor).st_ino
        os.unlink(target)
        assert not os.path.lexists(target), "unlinked COW path reappeared"
        assert db.execute(
            "SELECT id FROM drive_pending_files WHERE id=?", (pending_id,)
        ).fetchone() is None
        assert base_trashed(db, entry_id), "hidden base was not trashed atomically"
        assert os.pread(descriptor, len(data) + 64, 0) == data
        held = os.fstat(descriptor)
        assert held.st_ino == inode and held.st_nlink == 0 and held.st_size == len(data)
        assert stage.is_dir() and marker.is_file(), "held reader did not retain stage"
        report["checks"]["successful_unlink_with_held_reader"] = {
            "pass": True,
            "entry_id": entry_id,
            "display_name": display_name,
            "base_size": base_size,
            "pending_id": pending_id,
            "inode": inode,
            "nlink": held.st_nlink,
            "stage": str(stage),
            "marker": str(marker),
            "path": str(target),
        }
        os.close(descriptor)
        descriptors.remove(descriptor)

        second = bases[1]
        entry_id2, _, display_name2, base_size2 = second
        target2 = mounted_target(mount, db, second)
        data2 = b"rollback then retry native pending unlink\n"
        pending2 = edit_pending(db, target2, entry_id2, data2)
        pending_id2, _, staging_path2, _ = pending2
        stage2 = Path(staging_path2).parent
        marker2 = stage2.parent / ".retained-readers" / pending_id2
        descriptor2 = os.open(target2, os.O_RDONLY)
        descriptors.append(descriptor2)
        safe_id = pending_id2.replace("'", "''")
        db.execute(
            "CREATE TRIGGER qa_native_unlink_failure "
            "BEFORE DELETE ON drive_pending_files "
            f"WHEN OLD.id='{safe_id}' "
            "BEGIN SELECT RAISE(ABORT,'qa native unlink failure'); END;"
        )
        failed_errno = None
        busy_retries = 0
        deadline = time.monotonic() + 3.0
        try:
            while True:
                try:
                    os.unlink(target2)
                    failed_errno = None
                    break
                except OSError as error:
                    failed_errno = error.errno
                    if failed_errno == errno.EBUSY and time.monotonic() < deadline:
                        busy_retries += 1
                        time.sleep(0.02)
                        continue
                    break
            assert failed_errno == errno.EIO, failed_errno
            assert os.path.lexists(target2) and target2.read_bytes() == data2
            assert db.execute(
                "SELECT id FROM drive_pending_files WHERE id=?", (pending_id2,)
            ).fetchone() is not None
            assert not base_trashed(db, entry_id2), "base trash leaked through failed unlink"
            assert os.pread(descriptor2, len(data2) + 64, 0) == data2
            assert os.fstat(descriptor2).st_nlink == 1
            assert stage2.is_dir() and not marker2.exists(), "failed unlink leaked marker"
        finally:
            db.execute("DROP TRIGGER IF EXISTS qa_native_unlink_failure")

        os.unlink(target2)
        assert not os.path.lexists(target2)
        assert db.execute(
            "SELECT id FROM drive_pending_files WHERE id=?", (pending_id2,)
        ).fetchone() is None
        assert base_trashed(db, entry_id2)
        assert os.pread(descriptor2, len(data2) + 64, 0) == data2
        held2 = os.fstat(descriptor2)
        assert held2.st_nlink == 0 and held2.st_size == len(data2)
        assert stage2.is_dir() and marker2.is_file()
        report["checks"]["rollback_retry_with_held_reader"] = {
            "pass": True,
            "entry_id": entry_id2,
            "display_name": display_name2,
            "base_size": base_size2,
            "pending_id": pending_id2,
            "failure_errno": failed_errno,
            "writer_release_busy_retries": busy_retries,
            "retry_nlink": held2.st_nlink,
            "stage": str(stage2),
            "marker": str(marker2),
            "path": str(target2),
        }
        os.close(descriptor2)
        descriptors.remove(descriptor2)

        report["restart_expected"] = {
            "absent_paths": [str(target), str(target2)],
            "pending_ids": [pending_id, pending_id2],
            "base_entry_ids": [entry_id, entry_id2],
            "retained_stages": [str(stage), str(stage2)],
            "retained_markers": [str(marker), str(marker2)],
        }
        report["pass"] = all(check["pass"] for check in report["checks"].values())
    except BaseException as error:
        report.update(error=str(error), traceback=traceback.format_exc())
    finally:
        for descriptor in descriptors:
            try:
                os.close(descriptor)
            except OSError:
                pass
        if db is not None:
            db.close()
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2), flush=True)
    return 0 if report["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
