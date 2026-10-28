#!/usr/bin/env python3
"""Launch a private synthetic TeraRelay Drive, run native COW unlink QA, restart it, and clean up.

The runner requires workspace-owned seed data and task directories. It never uses
the normal installed profile and always enables both synthetic QA flags.
"""
from pathlib import Path
import argparse
import json
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import time
import traceback


def stop_process(process):
    if process is None:
        return None
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    return process.returncode


def port_free(port):
    probe = socket.socket()
    try:
        probe.settimeout(0.3)
        if probe.connect_ex(("127.0.0.1", port)) == 0:
            return False
    finally:
        probe.close()
    probe = socket.socket()
    probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        probe.bind(("127.0.0.1", port))
    except OSError:
        return False
    finally:
        probe.close()
    return True


def wait_for_port(process, port, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"process exited {process.returncode} before port {port} was ready")
        probe = socket.socket()
        probe.settimeout(0.2)
        try:
            if probe.connect_ex(("127.0.0.1", port)) == 0:
                return
        finally:
            probe.close()
        time.sleep(0.1)
    raise RuntimeError(f"port {port} did not become ready")


def wait_for_mount(process, mount, timeout=25):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"app exited {process.returncode} before Drive mounted")
        if os.path.ismount(mount):
            return
        time.sleep(0.1)
    raise RuntimeError("private Drive mount did not appear")


def wait_for_unmount(task_name, timeout=12):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not mount_present(task_name):
            return True
        time.sleep(0.1)
    return not mount_present(task_name)


def mount_present(task_name):
    return any(task_name in line for line in Path("/proc/self/mountinfo").read_text().splitlines())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-root", type=Path, required=True)
    parser.add_argument("--task-dir", type=Path, required=True)
    parser.add_argument("--seed-data", type=Path, required=True)
    parser.add_argument("--port", type=int, default=1420)
    args = parser.parse_args()
    root = args.project_root.resolve(strict=True)
    task = args.task_dir.resolve(strict=True)
    seed = args.seed_data.resolve(strict=True)
    if not task.is_relative_to(root) or not seed.is_relative_to(root):
        raise SystemExit("task and seed must stay inside the approved project")
    state = {
        "pass": False,
        "synthetic_only": True,
        "normal_profile_used": False,
        "cloud_transfer_tested": False,
    }
    vite = app1 = app2 = None
    vite_file = app_file = restart_file = None
    pids = []
    try:
        for name in ("data", "cache", "config", "runtime", "mount"):
            path = task / name
            if path.exists():
                shutil.rmtree(path)
        shutil.copytree(seed, task / "data")
        for name in ("cache", "config", "runtime", "mount"):
            (task / name).mkdir()
        os.chmod(task / "runtime", 0o700)
        if not port_free(args.port):
            raise RuntimeError(f"port {args.port} has an active listener or cannot be safely bound")
        env = os.environ.copy()
        env.update(
            XDG_DATA_HOME=str(task / "data"),
            XDG_CACHE_HOME=str(task / "cache"),
            XDG_CONFIG_HOME=str(task / "config"),
            XDG_RUNTIME_DIR=str(task / "runtime"),
            TERARELAY_QA_FEATURE_A="1",
            VITE_TERA_QA_FEATURE_A="1",
            TERARELAY_DRIVE_MOUNT_PATH=str(task / "mount"),
        )
        vite_file = (task / "green-vite.log").open("wb")
        app_file = (task / "green-app.log").open("wb")
        restart_file = (task / "green-restart-app.log").open("wb")
        vite = subprocess.Popen(
            ["npm", "run", "dev", "--", "--host", "127.0.0.1", "--port", str(args.port), "--strictPort"],
            cwd=root / "app",
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=vite_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        pids.append(vite.pid)
        wait_for_port(vite, args.port)
        binary = root / "app/src-tauri/target/debug/terarelay"
        app1 = subprocess.Popen(
            [str(binary)],
            cwd=root / "app/src-tauri",
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=app_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        pids.append(app1.pid)
        wait_for_mount(app1, task / "mount")
        probe = subprocess.run(
            [
                "python3",
                str(root / "scripts/test-drive-pending-unlink-native.py"),
                "--mount",
                str(task / "mount"),
                "--db",
                str(task / "data/io.terarelay.desktop/shares.db"),
                "--output",
                str(task / "native-unlink-green.json"),
            ],
            cwd=root,
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=45,
        )
        state.update(
            probe_exit=probe.returncode,
            probe_stdout=probe.stdout[-12000:],
            probe_stderr=probe.stderr[-4000:],
        )
        report = json.loads((task / "native-unlink-green.json").read_text())
        if probe.returncode != 0 or not report.get("pass"):
            raise RuntimeError("native pending-unlink GREEN probe failed")
        first_exit = stop_process(app1)
        app1 = None
        if not wait_for_unmount(task.name):
            subprocess.run(
                ["fusermount3", "-u", str(task / "mount")],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=10,
            )
        if mount_present(task.name):
            raise RuntimeError("first private mount remained attached")

        app2 = subprocess.Popen(
            [str(binary)],
            cwd=root / "app/src-tauri",
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=restart_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        pids.append(app2.pid)
        wait_for_mount(app2, task / "mount")
        expected = report["restart_expected"]
        absent = [not os.path.lexists(path) for path in expected["absent_paths"]]
        with sqlite3.connect(task / "data/io.terarelay.desktop/shares.db", timeout=5) as db:
            pending_absent = [
                db.execute("SELECT id FROM drive_pending_files WHERE id=?", (pending_id,)).fetchone() is None
                for pending_id in expected["pending_ids"]
            ]
            bases_trashed = [
                (lambda row: row is not None and row[0] is not None)(
                    db.execute("SELECT trashed_at FROM drive_file_entries WHERE entry_id=?", (entry_id,)).fetchone()
                )
                for entry_id in expected["base_entry_ids"]
            ]
        stages_gone = [not Path(path).exists() for path in expected["retained_stages"]]
        markers_gone = [not Path(path).exists() for path in expected["retained_markers"]]
        restart = {
            "pass": all(absent + pending_absent + bases_trashed + stages_gone + markers_gone),
            "paths_absent": absent,
            "pending_rows_absent": pending_absent,
            "bases_remain_trashed": bases_trashed,
            "retained_stages_gone": stages_gone,
            "retained_markers_gone": markers_gone,
            "first_app_exit": first_exit,
        }
        (task / "restart-unlink-green.json").write_text(json.dumps(restart, indent=2) + "\n")
        if not restart["pass"]:
            raise RuntimeError("pending-unlink restart verification failed")
        state.update(native_pass=True, restart_pass=True, first_app_exit=first_exit)
        state["pass"] = True
    except BaseException as error:
        state.update(error=str(error), traceback=traceback.format_exc())
    finally:
        app2_exit = stop_process(app2)
        app1_exit = stop_process(app1)
        vite_exit = stop_process(vite)
        if mount_present(task.name):
            subprocess.run(
                ["fusermount3", "-u", str(task / "mount")],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=10,
            )
        exact_pid_states = {str(pid): ("present" if Path(f"/proc/{pid}").exists() else "absent") for pid in pids}
        state.update(
            app2_exit=app2_exit,
            cleanup_app1_exit=app1_exit,
            vite_exit=vite_exit,
            test_mount_present=mount_present(task.name),
            exact_pid_states=exact_pid_states,
        )
        state["pass"] = bool(
            state.get("pass")
            and not state["test_mount_present"]
            and all(status == "absent" for status in exact_pid_states.values())
        )
        (task / "green-run.json").write_text(json.dumps(state, indent=2) + "\n")
        print(json.dumps(state, indent=2), flush=True)
        for stream in (vite_file, app_file, restart_file):
            if stream is not None:
                stream.close()
    return 0 if state["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
