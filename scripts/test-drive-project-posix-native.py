#!/usr/bin/env python3
"""Strict Linux FUSE project regression on a private, owned QA mount.

prepare runs real chmod/symlink/Node/npm operations and leaves one unique fixture.
verify requires a replacement app process, checks persisted metadata and reruns
build/test before removing that fixture. Any unsupported operation exits nonzero.
No account, installer, network dependency, Git or user project is touched.
"""
import argparse
import errno
import json
import os
import shutil
import sqlite3
import stat
import subprocess
import time
import traceback
import uuid
from pathlib import Path


def require(value, message):
    if not value:
        raise AssertionError(message)


def mode(path):
    return stat.S_IMODE(path.stat().st_mode)


def expect_errno(expected, action):
    try:
        action()
    except OSError as error:
        require(error.errno in expected, f"expected errno {expected}, got {error.errno}: {error}")
    else:
        raise AssertionError(f"operation succeeded; expected errno {expected}")


def write(path, text):
    with path.open("w") as stream:
        stream.write(text)
        stream.flush()
        os.fsync(stream.fileno())


def run(command, cwd, local):
    process_env = os.environ.copy()
    process_env.update(npm_config_cache=str(local / "npm-cache"), npm_config_offline="true",
                       npm_config_audit="false", npm_config_fund="false",
                       npm_config_update_notifier="false")
    result = subprocess.run(command, cwd=cwd, env=process_env, capture_output=True,
                            text=True, timeout=60)
    require(result.returncode == 0,
            f"{command!r} exited {result.returncode}: {(result.stdout + result.stderr)[-4000:]}")
    return result.stdout


def process_identity(pid):
    # starttime disambiguates PID reuse; names/credentials are never recorded.
    return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]


def owned_mount(args):
    project = Path(__file__).resolve().parent.parent
    raw_env = Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode()
    app_env = dict(item.split("=", 1) for item in raw_env.split("\0") if "=" in item)
    require(app_env.get("TERARELAY_QA_FEATURE_A") == "1", "Refusing a real account")
    mount = args.mount.resolve(strict=True)
    require(mount.is_relative_to(project) and os.path.ismount(mount), "Expected owned real mount")
    require(Path(app_env["TERARELAY_DRIVE_MOUNT_PATH"]).resolve(strict=True) == mount,
            "Mount does not belong to the QA process")
    data = Path(app_env["XDG_DATA_HOME"]).resolve(strict=True)
    require(data.is_relative_to(project), "Expected private QA profile")
    escaped = str(mount).replace("\\", "\\134").replace(" ", "\\040").replace("\t", "\\011")
    rows = [line.split(" - ", 1) for line in Path("/proc/self/mountinfo").read_text().splitlines()]
    require(any(left.split()[4] == escaped and right.split()[0].startswith("fuse")
                for left, right in rows), "Expected actual Linux FUSE, not a local directory")
    with sqlite3.connect(f"file:{data / 'io.terarelay.desktop/shares.db'}?mode=ro", uri=True) as db:
        require(db.execute("SELECT count(*) FROM logical_channels WHERE logical_id='qa-feature-a-movies'")
                .fetchone()[0] == 1, "Expected seeded private QA account")
    return mount


def safe_cleanup(case):
    # Never follow links, including dangling and looping entries; restore only owned modes.
    if case.is_symlink():
        raise AssertionError("Owned fixture root unexpectedly became a symlink")
    if case.exists():
        for current, directories, files in os.walk(case, topdown=True, followlinks=False):
            os.chmod(current, 0o755)
            for name in directories + files:
                child = Path(current) / name
                if not child.is_symlink():
                    os.chmod(child, 0o755 if child.is_dir() else 0o644)
        shutil.rmtree(case)
    require(not os.path.lexists(case), "Owned audit cleanup failed")


def prepare(case, local, check):
    def creation_modes():
        old_umask = os.umask(0o027)
        try:
            os.mkdir(case / "created-dir", 0o777)
            fd = os.open(case / "created-file", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o666)
            os.close(fd)
        finally:
            os.umask(old_umask)
        require(mode(case / "created-dir") == 0o750, "mkdir must honor requested mode and umask")
        require(mode(case / "created-file") == 0o640, "create must honor requested mode and umask")
    check("create and mkdir preserve requested mode and umask", creation_modes)

    def chmod_modes():
        path = case / "mode-probe"
        directory = case / "mode-dir"
        write(path, "mode fixture\n")
        directory.mkdir()
        for expected in (0o000, 0o001, 0o640, 0o755, 0o777):
            path.chmod(expected)
            require(mode(path) == expected, f"file chmod {expected:04o} reported {mode(path):04o}")
            directory.chmod(expected)
            require(mode(directory) == expected, f"directory chmod {expected:04o} reported {mode(directory):04o}")
        path.chmod(0o000)
        directory.chmod(0o750)
    check("chmod retains ordinary permission bits including mode 0000", chmod_modes)

    def execute():
        executable = case / "execute.sh"
        write(executable, "#!/bin/sh\nprintf 'drive-exec-ok\\n'\n")
        executable.chmod(0o755)
        require(mode(executable) == 0o755, "execute bit was discarded")
        require(run([str(executable)], case, local).strip() == "drive-exec-ok", "mounted executable failed")
    check("real shell executes a chmodded mounted script", execute)

    write(case / "target.txt", "target remains unchanged\n")

    def symlinks():
        for name, target in [("relative-link", "target.txt"), ("absolute-link", str(case / "target.txt")),
                             ("dangling-link", "absent-target"), ("loop-a", "loop-b"),
                             ("loop-b", "loop-a"), ("unicode-link", "café/../target.txt")]:
            os.symlink(target, case / name)
            require(os.readlink(case / name) == target, f"readlink changed {name}")
            require(stat.S_ISLNK((case / name).lstat().st_mode), f"lstat lost link type: {name}")
            require((case / name).lstat().st_size == len(target.encode()), f"link size is not UTF-8 bytes: {name}")
        require((case / "relative-link").read_text() == "target remains unchanged\n", "relative link did not resolve")
        require((case / "absolute-link").read_text() == "target remains unchanged\n", "absolute link did not resolve")
        expect_errno({errno.ENOENT}, lambda: (case / "dangling-link").read_bytes())
        expect_errno({errno.ELOOP}, lambda: (case / "loop-a").read_bytes())
        os.symlink("a" * 4095, case / "long-link")
        require(os.readlink(case / "long-link") == "a" * 4095, "4095-byte target changed")
    check("symlink lstat/readlink and kernel dangling/loop semantics", symlinks)

    def rename_links():
        os.symlink("target.txt", case / "rename-link")
        os.rename(case / "rename-link", case / "renamed-link")
        require(os.readlink(case / "renamed-link") == "target.txt", "rename changed target")
        os.unlink(case / "renamed-link")
        require((case / "target.txt").read_text() == "target remains unchanged\n", "unlink modified target")
        write(case / "replace-me", "owned destination\n")
        os.symlink("target.txt", case / "replace-link")
        os.replace(case / "replace-link", case / "replace-me")
        require(os.readlink(case / "replace-me") == "target.txt", "link replacing file failed")
        write(case / "replacement-file", "new entry\n")
        os.replace(case / "replacement-file", case / "replace-me")
        require(not (case / "replace-me").is_symlink(), "file replacing link failed")
        require((case / "replace-me").read_text() == "new entry\n", "replacement content changed")
        require((case / "target.txt").read_text() == "target remains unchanged\n", "replacement touched symlink target")
        directory = case / "nonempty-dir"
        directory.mkdir()
        os.symlink("../target.txt", directory / "child-link")
        expect_errno({errno.ENOTEMPTY, errno.EEXIST}, lambda: os.rmdir(directory))
        os.symlink("target.txt", case / "collision-link")
        expect_errno({errno.EISDIR, errno.ENOTDIR}, lambda: os.rename(case / "collision-link", directory))
        require(os.readlink(case / "collision-link") == "target.txt", "failed rename destroyed source")
        require(os.readlink(directory / "child-link") == "../target.txt", "failed rename destroyed destination")
        os.unlink(directory / "child-link")
        os.rmdir(directory)
    check("symlink rename/unlink/replacement and nonempty directory safety", rename_links)

    node = case / "node-project"
    node.mkdir()
    dependency = local / "dependency"
    dependency.mkdir()
    write(dependency / "package.json", json.dumps({"name": "tr-drive-tool-fixture", "version": "0.0.0",
          "bin": {"tr-drive-check": "cli.cjs"}}))
    write(dependency / "cli.cjs", "#!/usr/bin/env node\nconsole.log('drive-npm-bin-ok');\n")
    (dependency / "cli.cjs").chmod(0o755)
    write(node / "package.json", json.dumps({"name": "drive-posix-owned-fixture", "version": "0.0.0",
          "private": True, "scripts": {"build": "node build.cjs", "test": "tr-drive-check && node test.cjs"},
          "dependencies": {"tr-drive-tool-fixture": "file:" + str(dependency)}}))
    write(node / "source.cjs", "module.exports = 'first-edit';\n")
    write(node / "build.cjs", "const fs=require('fs'); fs.mkdirSync('dist',{recursive:true});\n"
          "fs.writeFileSync('dist/output.txt',require('./source.cjs')+'\\n'); console.log('drive-build-ok');\n")
    write(node / "test.cjs", "const fs=require('fs'); const assert=require('assert');\n"
          "assert.strictEqual(fs.readFileSync('dist/output.txt','utf8'),require('./source.cjs')+'\\n');\n"
          "console.log('drive-test-ok');\n")

    def npm_install():
        run(["npm", "install", "--offline", "--ignore-scripts", "--install-links", "--no-audit", "--no-fund"], node, local)
        binary = node / "node_modules/.bin/tr-drive-check"
        require(binary.is_symlink(), "npm bin must be a real symlink")
        require(os.readlink(binary) == "../tr-drive-tool-fixture/cli.cjs", "unexpected npm bin target")
        require(mode(binary) & 0o111 != 0, "npm installed bin lost executable permission")
        require(run([str(binary)], node, local).strip() == "drive-npm-bin-ok", "npm bin did not execute")
    check("actual offline npm install creates and executes its symlinked bin", npm_install)

    def edit_build_test():
        for revision in ("first-edit", "second-edit"):
            write(node / "source.cjs", f"module.exports = '{revision}';\n")
            require("drive-build-ok" in run(["npm", "run", "build"], node, local), "build did not execute")
            output = run(["npm", "run", "test"], node, local)
            require("drive-npm-bin-ok" in output and "drive-test-ok" in output, "test did not execute")
            require((node / "dist/output.txt").read_text() == revision + "\n", "build did not consume edited source")
    check("source edits feed actual npm run build and test processes", edit_build_test)


def snapshot(case):
    paths = ["created-file", "created-dir", "mode-probe", "mode-dir", "execute.sh",
             "node-project/node_modules/tr-drive-tool-fixture/cli.cjs"]
    links = ["relative-link", "absolute-link", "dangling-link", "loop-a", "loop-b", "unicode-link",
             "long-link", "node-project/node_modules/.bin/tr-drive-check"]
    return {"modes": {path: mode(case / path) for path in paths},
            "links": {path: os.readlink(case / path) for path in links}}


def verify(case, local, state, check):
    def metadata():
        actual = snapshot(case)
        require(actual == state["metadata"], f"metadata changed after crash: {actual!r}")
        require((case / "mode-probe").stat().st_mode & 0o777 == 0, "mode 0000 lost after restart")
        require((case / "target.txt").read_text() == "target remains unchanged\n", "link target changed after restart")
        require((case / "node-project/source.cjs").read_text() == "module.exports = 'second-edit';\n", "source edit lost")
        expect_errno({errno.ENOENT}, lambda: (case / "dangling-link").read_bytes())
        expect_errno({errno.ELOOP}, lambda: (case / "loop-a").read_bytes())
    check("modes and symlinks persist after actual QA application crash/restart", metadata)

    def processes():
        require(run([str(case / "execute.sh")], case, local).strip() == "drive-exec-ok", "persisted executable failed")
        node = case / "node-project"
        require("drive-build-ok" in run(["npm", "run", "build"], node, local), "restart build failed")
        output = run(["npm", "run", "test"], node, local)
        require("drive-npm-bin-ok" in output and "drive-test-ok" in output, "restart test failed")
        require((node / "dist/output.txt").read_text() == "second-edit\n", "persisted source not consumed")
    check("mounted executable and npm build/test work after restart", processes)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mount", type=Path, required=True)
    parser.add_argument("--qa-pid", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--phase", choices=("prepare", "verify"), default="prepare")
    parser.add_argument("--state", type=Path, help="Owned restart receipt outside the mount")
    args = parser.parse_args()
    args.output = args.output.resolve()
    state_path = (args.state or args.output.with_name("project-posix-state.json")).resolve()
    report = {"phase": args.phase, "pass_result": False, "checks": {},
              "scope": "Strict actual Linux FUSE with tiny offline Node/npm fixture; no live cloud transfer"}
    case = local = None
    keep = False

    def check(name, action):
        started = time.perf_counter()
        try:
            action()
            report["checks"][name] = {"passed": True, "seconds": time.perf_counter() - started}
        except Exception as error:
            report["checks"][name] = {"passed": False, "seconds": time.perf_counter() - started,
                                      "error": str(error), "traceback": traceback.format_exc()}
        print(name, json.dumps(report["checks"][name]), flush=True)

    try:
        mount = owned_mount(args)
        project = Path(__file__).resolve().parent.parent
        require(args.output.is_relative_to(project) and state_path.is_relative_to(project), "Receipt must be inside owned workspace")
        require(not args.output.is_relative_to(mount) and not state_path.is_relative_to(mount), "Receipt must be outside mount")
        if args.phase == "prepare":
            require(not state_path.exists(), "Refusing to replace an existing restart receipt")
            token = uuid.uuid4().hex
            case = mount / "Archive QA" / ("project-posix-" + token)
            local = args.output.parent / ("project-posix-local-" + token)
            case.mkdir()
            local.mkdir()
            prepare(case, local, check)
            require(all(item["passed"] for item in report["checks"].values()), "Strict project requirements failed; unsupported is not a pass")
            state = {"token": token, "mount": str(mount), "case": str(case), "local": str(local),
                     "prepare_pid": args.qa_pid, "prepare_starttime": process_identity(args.qa_pid),
                     "metadata": snapshot(case)}
            state_path.write_text(json.dumps(state, indent=2) + "\n")
            keep = True
            report.update(pass_result=True, restart_verification_required=True, state=str(state_path))
        else:
            state = json.loads(state_path.read_text())
            token = state["token"]
            require(len(token) == 32 and all(c in "0123456789abcdef" for c in token), "Invalid owned receipt token")
            case = mount / "Archive QA" / ("project-posix-" + token)
            local = args.output.parent / ("project-posix-local-" + token)
            require(state["case"] == str(case) and state["mount"] == str(mount) and state["local"] == str(local), "Receipt fixture ownership mismatch")
            require(args.qa_pid != state["prepare_pid"] or process_identity(args.qa_pid) != state["prepare_starttime"], "Persistence verification requires a replacement application process")
            old_stat = Path(f"/proc/{state['prepare_pid']}/stat")
            require(not old_stat.exists() or process_identity(state["prepare_pid"]) != state["prepare_starttime"], "Prepared application is still alive")
            verify(case, local, state, check)
            require(all(item["passed"] for item in report["checks"].values()), "Restart verification failed")
            report.update(pass_result=True, restart_verification_required=False)
    except Exception as error:
        report.update(error=str(error), traceback=traceback.format_exc())
    finally:
        if not keep:
            try:
                if case is not None:
                    safe_cleanup(case)
                if local is not None and local.exists():
                    shutil.rmtree(local)
                report["owned_fixture_removed"] = case is not None
            except Exception as error:
                report.update(pass_result=False, cleanup_error=str(error))
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2), flush=True)
    return 0 if report["pass_result"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
