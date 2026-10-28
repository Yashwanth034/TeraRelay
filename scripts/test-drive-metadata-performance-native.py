#!/usr/bin/env python3
"""Measure real FUSE metadata scaling; fixtures contain no remote file payload."""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import time
import uuid

parser=argparse.ArgumentParser()
parser.add_argument("--mount", required=True, type=Path)
parser.add_argument("--qa-pid", required=True, type=int)
parser.add_argument("--output", required=True, type=Path)
args=parser.parse_args()
project=Path(__file__).resolve().parent.parent
env=dict(item.split("=",1) for item in Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode().split("\0") if "=" in item)
assert env.get("TERARELAY_QA_FEATURE_A")=="1", "Refusing a real account"
mount=args.mount.resolve(strict=True)
data=Path(env["XDG_DATA_HOME"]).resolve(strict=True)
assert mount.is_relative_to(project) and data.is_relative_to(project)
assert Path(env["TERARELAY_DRIVE_MOUNT_PATH"]).resolve(strict=True)==mount
assert os.path.ismount(mount)
db_path=data/"io.terarelay.desktop/shares.db"
cache=Path(env["XDG_CACHE_HOME"])/"io.terarelay.desktop/drive/blocks"
def cache_bytes():
    return sum(p.stat().st_size for p in cache.rglob("*") if p.is_file()) if cache.exists() else 0
metrics=[]
before=cache_bytes()
for count in [100,1000]:
    identity="metadata-perf-"+uuid.uuid4().hex
    name=identity
    directory=mount/"Archive QA"/name
    with sqlite3.connect(db_path) as db:
        assert db.execute("SELECT count(*) FROM logical_channels WHERE logical_id='qa-feature-a-movies'").fetchone()[0]==1
        now=int(time.time()*1000)
        db.execute("INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES (?,'channel-qa-feature-a-archive',?,?,?)",(identity,name,now,now))
        db.executemany("INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at) VALUES (?,'55555555555555555555555555555555',?,?,?,?)",
            [(identity+"-"+str(i),identity,f"placeholder-{i:04d}.bin",now,now) for i in range(count)])
    try:
        # Enumerate the SAME already-open directory repeatedly, not just once.
        # Capture listing and stat timings independently to tell a slow native
        # readdir(plus) from a delayed getattr, without collecting any names.
        # Four bounded rounds stress the occasional second-open complaint while
        # leaving the retired SSD-relative benchmark goal untouched.
        rounds=[]
        for _ in range(4):
            start=time.perf_counter()
            with os.scandir(directory) as listing:
                entries=list(listing)
            listed=time.perf_counter()
            for entry in entries:
                assert entry.stat().st_size==10_000_000_000_000
            finished=time.perf_counter()
            assert len(entries)==count
            rounds.append({
                "list_seconds":listed-start,
                "stat_seconds":finished-listed,
                "total_seconds":finished-start,
            })
        elapsed=rounds[0]["total_seconds"]
        metrics.append({
            "files":count,
            "seconds":elapsed,
            "seconds_per_file":elapsed/count,
            "repeated_same_folder_rounds":rounds,
            "longest_folder_round_seconds":max(row["total_seconds"] for row in rounds),
        })
        print("MEASURE",metrics[-1],flush=True)
    finally:
        with sqlite3.connect(db_path) as db:
            db.execute("DELETE FROM drive_file_entries WHERE directory_id=?",(identity,))
            db.execute("DELETE FROM drive_directories WHERE id=?",(identity,))
        # Direct fixture DB writes bypass native FUSE invalidations; allow its
        # existing one-second attribute TTL to expire before checking removal.
        stop=time.monotonic()+3
        while directory.exists() and time.monotonic()<stop:
            time.sleep(.05)
        assert not directory.exists(), 'Owned metadata fixture was not removed'
after=cache_bytes()
normalized=metrics[1]["seconds_per_file"]/metrics[0]["seconds_per_file"]
result={"passed":normalized<=3 and before==after,"metrics":metrics,"per_file_cost_growth":normalized,
        "cache_before":before,"cache_after":after,"scope":"Real Linux FUSE metadata-only enumeration; no live Telegram transfer"}
args.output.write_text(json.dumps(result,indent=2))
print(json.dumps(result,indent=2),flush=True)
assert before==after,"Metadata listing acquired file payload"
assert normalized<=3, "Per-file enumeration cost grows with the entire namespace: "+str(normalized)
