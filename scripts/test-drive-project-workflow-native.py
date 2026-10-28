#!/usr/bin/env python3
"""Capability audit on an owned QA mount, not a claim of full cloud development.
Runs tiny offline Git/Node/npm/Cargo fixtures; no app installer, commits, pushes,
user project modifications or live Telegram uploads. Unsupported operations are
reported, and only the unique audit directories are removed.
"""
import argparse, errno, fcntl, json, os, shutil, sqlite3, subprocess, tempfile, time, uuid
from pathlib import Path
parser=argparse.ArgumentParser()
parser.add_argument("--mount",type=Path,required=True)
parser.add_argument("--qa-pid",type=int,required=True)
parser.add_argument("--output",type=Path,required=True)
args=parser.parse_args()
project=Path(__file__).resolve().parent.parent
env=dict(item.split("=",1) for item in Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode().split("\0") if "=" in item)
assert env.get("TERARELAY_QA_FEATURE_A")=="1","Refusing a real account"
mount=args.mount.resolve(strict=True)
assert mount.is_relative_to(project) and os.path.ismount(mount)
assert Path(env["TERARELAY_DRIVE_MOUNT_PATH"]).resolve(strict=True)==mount
data=Path(env["XDG_DATA_HOME"]).resolve(strict=True)
assert data.is_relative_to(project)
with sqlite3.connect(f"file:{data/'io.terarelay.desktop/shares.db'}?mode=ro",uri=True) as db:
    assert db.execute("SELECT count(*) FROM logical_channels WHERE logical_id='qa-feature-a-movies'").fetchone()[0]==1
case=mount/"Archive QA"/("project-audit-"+uuid.uuid4().hex)
case.mkdir()
results={}
def operation(name,action):
    started=time.perf_counter()
    try:
        detail=action()
        results[name]={"supported":True,"seconds":time.perf_counter()-started,"detail":detail}
    except Exception as error:
        results[name]={"supported":False,"seconds":time.perf_counter()-started,"error":str(error)[:1800]}
    print(name,json.dumps(results[name]),flush=True)
def run(command,cwd=None):
    p=subprocess.run(command,cwd=cwd,capture_output=True,text=True,timeout=45)
    if p.returncode:
        raise RuntimeError(f"exit {p.returncode}: "+(p.stderr+p.stdout)[-1500:])
    return p.stdout.strip()
try:
    with tempfile.TemporaryDirectory(prefix="drive-project-local-",dir=project/".repotunnel-tmp") as local:
        local=Path(local)
        node=case/"node-source";node.mkdir()
        (node/"index.js").write_text("console.log('drive-node-ok');\n")
        def node_read():
            value=run(["node",str(node/"index.js")])
            assert value=="drive-node-ok",value
            return value
        operation("Node reads source from mounted Drive",node_read)
        git=case/"git-probe";git.mkdir()
        def git_work():
            run(["git","init","--quiet",str(git)])
            (git/"source.txt").write_text("owned fixture\n")
            value=run(["git","status","--porcelain"],git)
            assert "source.txt" in value,value
            return "git init and status work; no commits or staging"
        operation("Git metadata creation and status",git_work)
        def symlink():
            link=case/"source-link"
            os.symlink("node-source/index.js",link)
            assert link.read_text()==(node/"index.js").read_text()
            return "relative symlink resolves correctly"
        operation("Relative symlinks required by package tools",symlink)
        def hardlink():
            target=case/"hardlink-source"
            os.link(node/"index.js",target)
            assert target.read_bytes()==(node/"index.js").read_bytes()
            return "hardlink resolves correctly"
        operation("Hardlink creation",hardlink)
        def executable():
            script=case/"execute-probe.sh"
            script.write_text("#!/bin/sh\nprintf 'drive-exec-ok'\n")
            script.chmod(0o755)
            value=run([str(script)])
            assert value=="drive-exec-ok",value
            return value
        operation("Persist executable permissions and execute",executable)
        def lock():
            path=case/"lock-probe"
            with path.open("w+b") as first,path.open("r+b") as second:
                fcntl.flock(first,fcntl.LOCK_EX|fcntl.LOCK_NB)
                try:
                    fcntl.flock(second,fcntl.LOCK_EX|fcntl.LOCK_NB)
                except OSError as error:
                    assert error.errno in (errno.EAGAIN,errno.EACCES),str(error)
                else:
                    raise AssertionError("Independent descriptor ignored the exclusive lock")
            return "exclusive lock blocks an independent descriptor"
        operation("File lock exclusion",lock)
        dependency=local/"dependency";dependency.mkdir()
        (dependency/"package.json").write_text(json.dumps({"name":"tr-drive-tool-fixture","version":"0.0.0","bin":{"tr-drive-check":"cli.js"}}))
        (dependency/"cli.js").write_text("#!/usr/bin/env node\nconsole.log('drive-npm-ok');\n")
        (dependency/"cli.js").chmod(0o755)
        (node/"package.json").write_text(json.dumps({"name":"drive-project-probe","version":"0.0.0","private":True,"dependencies":{"tr-drive-tool-fixture":"file:"+str(dependency)}}))
        def npm_install():
            run(["npm","install","--offline","--ignore-scripts","--install-links","--no-audit","--no-fund","--cache",str(local/"npm-cache")],node)
            binary=node/"node_modules/.bin/tr-drive-check"
            value=run([str(binary)],node)
            assert value=="drive-npm-ok",value
            return "offline dependency install and linked bin execute"
        operation("npm offline install and bin execution in Drive",npm_install)
        rust=case/"rust-source";rust.mkdir();(rust/"src").mkdir()
        (rust/"Cargo.toml").write_text('[package]\nname="drive_project_probe"\nversion="0.0.0"\nedition="2021"\n[workspace]\n')
        (rust/"src/main.rs").write_text('fn main(){ println!("drive-rust-ok"); }\n')
        def cargo_local():
            target=local/"cargo-target"
            run(["cargo","build","--offline","--manifest-path",str(rust/"Cargo.toml"),"--target-dir",str(target)],rust)
            value=run([str(target/"debug/drive_project_probe")])
            assert value=="drive-rust-ok",value
            return "source on Drive; build output in owned local directory"
        operation("Cargo builds Drive source with local target",cargo_local)
finally:
    # The whole unique case belongs to this audit; no user directories are removed.
    if case.exists(): shutil.rmtree(case)
    assert not case.exists(),"Owned audit cleanup failed"
    report={"audit_finished":True,"capabilities":results,
            "direct_development_supported":all(v["supported"] for v in results.values()),
            "scope":"Real Linux FUSE with local QA staging and tiny offline tools; no live cloud transfer or 100GB workload",
            "owned_mount_directory_removed":True}
    args.output.write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report,indent=2),flush=True)
