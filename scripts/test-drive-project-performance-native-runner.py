#!/usr/bin/env python3
"""Private launcher for paired TeraRelay Drive project performance measurement."""
from pathlib import Path
import argparse
import json
import math
import os
import shutil
import signal
import socket
import stat
import subprocess
import time
import traceback
import uuid


LOAD_LIMITS = {"one": 2.0, "five": 2.8, "fifteen": 2.1}
OWNED_WORKSPACE_ID = "workspace-synthetic-qa-fixture"
OWNED_TASK_MARKER = ".repotunnel-temp.json"
TASK_DIRECTORY_PREFIX = "native-project-performance-"


def verify_owned_benchmark_task(root, task):
    """Require the existing RepoTunnel owner marker before ANY task mutation.

    Merely residing below the Git repository is not sufficient ownership.
    A preserved marker explicitly forbids destructive benchmark preparation.
    """
    require_parent = root / "terarelay-private-qa-tasks"
    if task.parent != require_parent or not task.name.startswith(TASK_DIRECTORY_PREFIX):
        raise SystemExit("benchmark task must be an owned terarelay-private-qa-tasks/native-project-performance-* directory")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    parent_fd = os.open(require_parent, flags)
    try:
        task_fd = os.open(task.name, flags, dir_fd=parent_fd)
        try:
            parent = os.fstat(parent_fd)
            actual = os.fstat(task_fd)
            if (parent.st_dev != actual.st_dev or
                    owned_task_mount_id(parent_fd) != owned_task_mount_id(task_fd)):
                raise SystemExit("benchmark task is a mounted or foreign directory")
            marker_fd = os.open(
                OWNED_TASK_MARKER, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=task_fd,
            )
            try:
                marker_stat = os.fstat(marker_fd)
                if (not stat.S_ISREG(marker_stat.st_mode)
                        or marker_stat.st_dev != actual.st_dev
                        or marker_stat.st_size > 4096):
                    raise SystemExit("invalid RepoTunnel benchmark ownership marker")
                with os.fdopen(marker_fd, "rb", closefd=False) as stream:
                    marker = json.loads(stream.read(4097))
            finally:
                os.close(marker_fd)
            if (type(marker) is not dict or
                    type(marker.get("schemaVersion")) is not int or
                    marker["schemaVersion"] != 1 or
                    marker.get("workspaceId") != OWNED_WORKSPACE_ID or
                    marker.get("taskId") != task.name or
                    marker.get("preserved") is not False):
                raise SystemExit("RepoTunnel task ownership mismatch or preserved task")
            return (actual.st_dev, actual.st_ino)
        finally:
            os.close(task_fd)
    finally:
        os.close(parent_fd)


def owned_task_mount_id(directory_fd):
    """Linux mount ID catches nested bind mounts even on the same device."""
    information = Path(f"/proc/self/fdinfo/{directory_fd}").read_text()
    for line in information.splitlines():
        if line.startswith("mnt_id:"):
            return int(line.partition(":")[2].strip())
    raise RuntimeError("Cannot verify benchmark task mount identity")


def remove_owned_task_children(directory_fd, device, mount_id):
    """Never follow links, leave the original directory on a mount mismatch."""
    with os.scandir(directory_fd) as entries:
        names = [entry.name for entry in entries]
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    for name in names:
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if info.st_dev != device:
            raise RuntimeError("Owned benchmark cleanup crossed filesystem")
        if stat.S_ISDIR(info.st_mode):
            child_fd = os.open(name, flags, dir_fd=directory_fd)
            try:
                opened = os.fstat(child_fd)
                if opened.st_dev != device or opened.st_ino != info.st_ino:
                    raise RuntimeError("Benchmark cleanup child directory was replaced")
                if owned_task_mount_id(child_fd) != mount_id:
                    raise RuntimeError("Refusing to delete an unexpected nested mount")
                remove_owned_task_children(child_fd, device, mount_id)
            finally:
                os.close(child_fd)
            latest = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if not stat.S_ISDIR(latest.st_mode) or latest.st_ino != info.st_ino:
                raise RuntimeError("Benchmark cleanup child directory changed")
            os.rmdir(name, dir_fd=directory_fd)
        elif stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
            os.unlink(name, dir_fd=directory_fd)
        else:
            raise RuntimeError("Refusing unsafe special file in benchmark task cleanup")


def remove_owned_task_directory(task, child_name, expected_identity):
    if child_name not in {"data", "cache", "config", "runtime", "mount"}:
        raise RuntimeError("Refusing to remove an unexpected benchmark task directory")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    task_fd = os.open(task, flags)
    try:
        owner = os.fstat(task_fd)
        if (owner.st_dev, owner.st_ino) != expected_identity:
            raise RuntimeError("Benchmark task owner changed before cleanup")
        try:
            before = os.stat(child_name, dir_fd=task_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        if not stat.S_ISDIR(before.st_mode):
            raise RuntimeError("Refusing linked or non-directory benchmark cleanup target")
        child_fd = os.open(child_name, flags, dir_fd=task_fd)
        try:
            opened = os.fstat(child_fd)
            if opened.st_dev != owner.st_dev or opened.st_ino != before.st_ino:
                raise RuntimeError("Benchmark cleanup target was replaced or remounted")
            mount_id = owned_task_mount_id(task_fd)
            if owned_task_mount_id(child_fd) != mount_id:
                raise RuntimeError("Refusing to delete a mounted benchmark task directory")
            remove_owned_task_children(child_fd, owner.st_dev, mount_id)
        finally:
            os.close(child_fd)
        latest = os.stat(child_name, dir_fd=task_fd, follow_symlinks=False)
        if not stat.S_ISDIR(latest.st_mode) or latest.st_ino != before.st_ino:
            raise RuntimeError("Benchmark cleanup target changed")
        os.rmdir(child_name, dir_fd=task_fd)
    finally:
        os.close(task_fd)


def write_owned_run_receipt(task, identity, state):
    """Atomically replace the receipt name, never follow an existing link."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    task_fd = os.open(task, flags)
    temporary = f".performance-run-{uuid.uuid4().hex}.tmp"
    try:
        current = os.fstat(task_fd)
        if (current.st_dev, current.st_ino) != identity:
            raise RuntimeError("Benchmark receipt task owner changed")
        fd = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
            0o600,
            dir_fd=task_fd,
        )
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                stream.write(json.dumps(state, indent=2) + "\n")
                stream.flush()
                os.fsync(stream.fileno())
        except BaseException:
            os.unlink(temporary, dir_fd=task_fd)
            raise
        os.replace(temporary, "performance-run.json",
                   src_dir_fd=task_fd, dst_dir_fd=task_fd)
        os.fsync(task_fd)
    finally:
        os.close(task_fd)


def open_owned_task_log(task, identity, filename):
    """No output pathname may follow a link or overwrite a hardlinked file."""
    if filename not in {"performance-vite.log", "performance-app-prepare.log",
                        "performance-app-measure.log"}:
        raise RuntimeError("Refusing unexpected benchmark log filename")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    task_fd = os.open(task, flags)
    try:
        current = os.fstat(task_fd)
        if (current.st_dev, current.st_ino) != identity:
            raise RuntimeError("Benchmark log task owner changed")
        try:
            existing = os.stat(filename, dir_fd=task_fd, follow_symlinks=False)
        except FileNotFoundError:
            existing = None
        if existing is not None:
            if existing.st_dev != current.st_dev:
                raise RuntimeError("Benchmark log crossed filesystem")
            os.unlink(filename, dir_fd=task_fd)
        fd = os.open(
            filename,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
            0o600,
            dir_fd=task_fd,
        )
        return os.fdopen(fd, "wb")
    finally:
        os.close(task_fd)


def copy_owned_seed_entries(source_fd, target_fd, source_device, mount_id):
    """Copy pinned regular files and directories only, never symlink targets."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    with os.scandir(source_fd) as entries:
        names = [entry.name for entry in entries]
    for name in names:
        info = os.stat(name, dir_fd=source_fd, follow_symlinks=False)
        if info.st_dev != source_device:
            raise RuntimeError("Seed file crossed filesystem")
        if stat.S_ISDIR(info.st_mode):
            child_fd = os.open(name, flags, dir_fd=source_fd)
            try:
                opened = os.fstat(child_fd)
                if opened.st_ino != info.st_ino or opened.st_dev != source_device:
                    raise RuntimeError("Seed directory was swapped")
                if owned_task_mount_id(child_fd) != mount_id:
                    raise RuntimeError("Refusing to copy a nested seed mount")
                os.mkdir(name, mode=0o700, dir_fd=target_fd)
                out_fd = os.open(name, flags, dir_fd=target_fd)
                try:
                    copy_owned_seed_entries(child_fd, out_fd, source_device, mount_id)
                    os.fchmod(out_fd, stat.S_IMODE(info.st_mode))
                finally:
                    os.close(out_fd)
            finally:
                os.close(child_fd)
        elif stat.S_ISREG(info.st_mode):
            in_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=source_fd)
            try:
                opened = os.fstat(in_fd)
                if opened.st_ino != info.st_ino or opened.st_dev != source_device:
                    raise RuntimeError("Seed file was swapped")
                out_fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                                 os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=target_fd)
                try:
                    with os.fdopen(in_fd, "rb", closefd=False) as src:
                        with os.fdopen(out_fd, "wb", closefd=False) as dst:
                            shutil.copyfileobj(src, dst, 1024 * 1024)
                            dst.flush()
                    os.fchmod(out_fd, stat.S_IMODE(info.st_mode))
                finally:
                    os.close(out_fd)
            finally:
                os.close(in_fd)
        else:
            raise RuntimeError("Refusing seed symlink or unexpected special file")


def clear_owned_task_probe_receipts(task, expected_identity):
    """Reset only known probe receipts inside the originally verified task."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    task_fd = os.open(task, flags)
    try:
        current = os.fstat(task_fd)
        if (current.st_dev, current.st_ino) != expected_identity:
            raise RuntimeError("Benchmark task owner changed before receipt reset")
        for name in ("performance-state.json", "performance-prepare.json",
                     "performance-measure.json"):
            try:
                info = os.stat(name, dir_fd=task_fd, follow_symlinks=False)
            except FileNotFoundError:
                continue
            if not (stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)):
                raise RuntimeError("Refusing unexpected benchmark receipt type")
            os.unlink(name, dir_fd=task_fd)
    finally:
        os.close(task_fd)


def create_owned_task_runtime_directories(task, expected_identity):
    """Create runtime roots relative to the pinned, unchanged task inode."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    parent_fd = os.open(task.parent, flags)
    try:
        task_fd = os.open(task.name, flags, dir_fd=parent_fd)
        try:
            owner = os.fstat(task_fd)
            if (owner.st_dev, owner.st_ino) != expected_identity:
                raise RuntimeError("Benchmark task owner changed before directory creation")
            if owned_task_mount_id(task_fd) != owned_task_mount_id(parent_fd):
                raise RuntimeError("Benchmark task was remounted during setup")
            for name in ("cache", "config", "runtime", "mount"):
                mode = 0o700 if name == "runtime" else 0o755
                os.mkdir(name, mode=mode, dir_fd=task_fd)
                if name == "runtime":
                    runtime_fd = os.open(name, flags, dir_fd=task_fd)
                    try:
                        runtime = os.fstat(runtime_fd)
                        if runtime.st_dev != owner.st_dev:
                            raise RuntimeError("Benchmark runtime crossed filesystem")
                        os.fchmod(runtime_fd, 0o700)
                    finally:
                        os.close(runtime_fd)
        finally:
            os.close(task_fd)
    finally:
        os.close(parent_fd)


def copy_owned_seed_tree(seed, task, expected_identity):
    """Create private task/data under the marked task, copying without links."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    task_fd = os.open(task, flags)
    try:
        current = os.fstat(task_fd)
        if (current.st_dev, current.st_ino) != expected_identity:
            raise RuntimeError("Benchmark task owner changed before seed copy")
        seed_fd = os.open(seed, flags)
        try:
            source_device = os.fstat(seed_fd).st_dev
            source_mount = owned_task_mount_id(seed_fd)
            os.mkdir("data", mode=0o700, dir_fd=task_fd)
            data_fd = os.open("data", flags, dir_fd=task_fd)
            try:
                copy_owned_seed_entries(seed_fd, data_fd, source_device, source_mount)
            finally:
                os.close(data_fd)
        finally:
            os.close(seed_fd)
    finally:
        os.close(task_fd)


def host_load_average():
    try:
        values = Path("/proc/loadavg").read_text().split()
        sample = {"one": float(values[0]), "five": float(values[1]), "fifteen": float(values[2])}
    except (OSError, ValueError, IndexError):
        return None
    return sample


def benchmark_load_failure(sample):
    if not isinstance(sample, dict):
        return "host load average is unavailable"
    for interval, limit in LOAD_LIMITS.items():
        value = sample.get(interval)
        if (not isinstance(value, (int, float)) or isinstance(value, bool)
                or not math.isfinite(value) or value < 0):
            return f"host {interval}-minute load average is missing or invalid"
        if value > limit:
            return f"host {interval}-minute load average {value:.2f} exceeds limit {limit:.2f}"
    return None


def fuse_device_failure(device_path=Path("/dev/fuse")):
    device_path = Path(device_path)
    try:
        device_stat = device_path.stat()
    except OSError as error:
        return f"FUSE device {device_path} is unavailable: {error}"
    if not stat.S_ISCHR(device_stat.st_mode):
        return f"FUSE device {device_path} is not a character device"
    try:
        descriptor = os.open(str(device_path), os.O_RDWR | getattr(os, "O_CLOEXEC", 0))
    except OSError as error:
        return f"FUSE device {device_path} cannot be opened: {error}"
    try:
        os.close(descriptor)
    except OSError as error:
        return f"FUSE device {device_path} could not be closed after preflight: {error}"
    return None


def native_build_inputs(root):
    crate = root / "app/src-tauri"
    inputs = [crate / "Cargo.toml", crate / "Cargo.lock", crate / "build.rs"]
    inputs.extend(
        path for path in (crate / "src").rglob("*")
        if path.is_file() and path.suffix in {".rs", ".py"}
    )
    return sorted((path for path in inputs if path.is_file()), key=str)


def stale_native_binary_inputs(binary, source_paths):
    existing = sorted((Path(path) for path in source_paths if Path(path).is_file()), key=str)
    if not binary.is_file():
        return existing
    binary_mtime_ns = binary.stat().st_mtime_ns
    return [path for path in existing if path.stat().st_mtime_ns > binary_mtime_ns]


def mount_present(task_name):
    return any(task_name in line for line in Path("/proc/self/mountinfo").read_text().splitlines())


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
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Vite exited {process.returncode} before port {port} was ready")
        probe = socket.socket()
        probe.settimeout(0.2)
        try:
            if probe.connect_ex(("127.0.0.1", port)) == 0:
                return
        finally:
            probe.close()
        time.sleep(0.1)
    raise RuntimeError(f"port {port} did not become ready")


def wait_for_mount(process, mount, task_name, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"TeraRelay exited {process.returncode} before Drive mounted")
        if mount_present(task_name) and os.path.isdir(mount):
            return
        time.sleep(0.1)
    raise RuntimeError("private Drive mount did not appear")


def wait_for_unmount(task_name, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not mount_present(task_name):
            return True
        time.sleep(0.1)
    return not mount_present(task_name)


def detach_if_needed(task, task_name):
    if not mount_present(task_name):
        return None
    result = subprocess.run(["fusermount3", "-u", str(task / "mount")],
                            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
    return {"exit": result.returncode, "stderr": result.stderr[-1000:]}


def launch_app(binary, root, env, log_path, task_identity):
    stream = open_owned_task_log(log_path.parent, task_identity, log_path.name)
    process = subprocess.Popen([str(binary)], cwd=root / "app/src-tauri", env=env,
                               stdin=subprocess.DEVNULL, stdout=stream,
                               stderr=subprocess.STDOUT, start_new_session=True)
    return process, stream


def run_probe(root, env, task, app_pid, phase):
    output = task / f"performance-{phase}.json"
    state = task / "performance-state.json"
    command = [
        "python3", str(root / "scripts/test-drive-project-performance-native.py"),
        "--mount", str(task / "mount"),
        "--qa-pid", str(app_pid),
        "--output", str(output),
        "--state", str(state),
        "--phase", phase,
    ]
    # The optional 1024-file fixture performs additional initial FUSE writes.
    # Never change the default runner's timeout or host-load preflight.
    timeout = 480 if env.get("TERARELAY_PERF_LARGE_PROJECT_FILES") == "1024" else 180
    result = subprocess.run(command, cwd=root, env=env, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=timeout)
    return result, output


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-root", type=Path, required=True)
    parser.add_argument("--task-dir", type=Path, required=True)
    parser.add_argument("--seed-data", type=Path, required=True)
    parser.add_argument("--port", type=int, default=1420)
    parser.add_argument("--large-project-files", type=int, choices=(0, 1024), default=0,
                        help="Opt in to a bounded 1024-file metadata comparison; default unchanged")
    parser.add_argument("--kernel-cache-diagnostics", action="store_true",
                        help="Opt in to unbuffered same-handle vs reopened 4KiB reads")
    args = parser.parse_args()
    root = args.project_root.resolve(strict=True)
    task = args.task_dir.resolve(strict=True)
    seed = args.seed_data.resolve(strict=True)
    if not task.is_relative_to(root) or not seed.is_relative_to(root):
        raise SystemExit("task and seed must stay inside the approved project")
    # This preflight is intentionally outside the receipt-writing finally:
    # rejected paths must not have ANY files replaced, deleted or written.
    task_identity = verify_owned_benchmark_task(root, task)
    state = {"pass": False, "synthetic_only": True, "normal_profile_used": False,
             "cloud_transfer_tested": False}
    vite = app1 = app2 = None
    vite_stream = app1_stream = app2_stream = None
    pids = []
    binary = root / "app/src-tauri/target/debug/terarelay"
    try:
        inputs = native_build_inputs(root)
        stale = stale_native_binary_inputs(binary, inputs)
        state["native_binary_path"] = str(binary.relative_to(root))
        state["native_binary_mtime_ns"] = binary.stat().st_mtime_ns if binary.is_file() else None
        state["newer_native_build_inputs"] = [str(path.relative_to(root)) for path in stale]
        if not binary.is_file():
            raise RuntimeError("native debug executable is missing; build app/src-tauri first")
        if stale:
            sample = ", ".join(str(path.relative_to(root)) for path in stale[:12])
            extra = f" (+{len(stale) - 12} more)" if len(stale) > 12 else ""
            raise RuntimeError(
                f"native debug executable is older than source/build inputs; rebuild before measuring: {sample}{extra}"
            )
        state["fuse_device_path"] = "/dev/fuse"
        fuse_failure = fuse_device_failure()
        state["fuse_preflight_failure"] = fuse_failure
        state["fuse_preflight_passed"] = fuse_failure is None
        if fuse_failure:
            raise RuntimeError(
                f"host FUSE preflight rejected benchmark before fixture cleanup or process startup: {fuse_failure}"
            )
        load_sample = host_load_average()
        state["preflight_load"] = load_sample
        state["load_limits"] = LOAD_LIMITS
        load_failure = benchmark_load_failure(load_sample)
        if load_failure:
            raise RuntimeError(
                f"host load gate rejected benchmark before fixture cleanup or process startup: {load_failure}"
            )
        state["load_gate_passed"] = True
        # Check the port before removing even legitimate private fixtures.
        if not port_free(args.port):
            raise RuntimeError(f"port {args.port} has an active listener or cannot be safely bound")
        state["large_project_files"] = args.large_project_files
        state["kernel_cache_diagnostics"] = args.kernel_cache_diagnostics
        for name in ("data", "cache", "config", "runtime", "mount"):
            remove_owned_task_directory(task, name, task_identity)
        clear_owned_task_probe_receipts(task, task_identity)
        copy_owned_seed_tree(seed, task, task_identity)
        create_owned_task_runtime_directories(task, task_identity)
        env = os.environ.copy()
        env.update(
            XDG_DATA_HOME=str(task / "data"),
            XDG_CACHE_HOME=str(task / "cache"),
            XDG_CONFIG_HOME=str(task / "config"),
            XDG_RUNTIME_DIR=str(task / "runtime"),
            TERARELAY_QA_FEATURE_A="1",
            VITE_TERA_QA_FEATURE_A="1",
            TERARELAY_DRIVE_MOUNT_PATH=str(task / "mount"),
            TERARELAY_PERF_LARGE_PROJECT_FILES=str(args.large_project_files),
            TERARELAY_PERF_CACHE_DIAGNOSTICS="1" if args.kernel_cache_diagnostics else "0",
        )
        vite_stream = open_owned_task_log(task, task_identity, "performance-vite.log")
        vite = subprocess.Popen(
            ["npm", "run", "dev", "--", "--host", "127.0.0.1", "--port", str(args.port), "--strictPort"],
            cwd=root / "app", env=env, stdin=subprocess.DEVNULL,
            stdout=vite_stream, stderr=subprocess.STDOUT, start_new_session=True)
        pids.append(vite.pid)
        wait_for_port(vite, args.port)

        app1, app1_stream = launch_app(
            binary, root, env, task / "performance-app-prepare.log", task_identity
        )
        pids.append(app1.pid)
        wait_for_mount(app1, task / "mount", task.name)
        prepare, prepare_output = run_probe(root, env, task, app1.pid, "prepare")
        state.update(prepare_exit=prepare.returncode,
                     prepare_stdout=prepare.stdout[-6000:], prepare_stderr=prepare.stderr[-3000:])
        if prepare.returncode != 0 or not json.loads(prepare_output.read_text()).get("pass"):
            raise RuntimeError("paired performance fixture preparation failed")

        first_exit = stop_process(app1)
        app1 = None
        if not wait_for_unmount(task.name):
            state["first_detach"] = detach_if_needed(task, task.name)
        if mount_present(task.name):
            raise RuntimeError("prepare mount remained attached")

        app2, app2_stream = launch_app(
            binary, root, env, task / "performance-app-measure.log", task_identity
        )
        pids.append(app2.pid)
        wait_for_mount(app2, task / "mount", task.name)
        measure, measure_output = run_probe(root, env, task, app2.pid, "measure")
        state.update(measure_exit=measure.returncode,
                     measure_stdout=measure.stdout[-12000:], measure_stderr=measure.stderr[-4000:])
        report = json.loads(measure_output.read_text())
        if measure.returncode != 0 or not report.get("pass"):
            raise RuntimeError("paired project performance measurement failed")
        state.update({"pass": True, "first_app_exit": first_exit,
                      "measurement_file": str(measure_output.relative_to(root))})
    except BaseException as error:
        state.update(error=str(error), traceback=traceback.format_exc())
    finally:
        app2_exit = stop_process(app2)
        app1_exit = stop_process(app1)
        vite_exit = stop_process(vite)
        if mount_present(task.name):
            state["cleanup_detach"] = detach_if_needed(task, task.name)
        exact_pid_states = {str(pid): ("present" if Path(f"/proc/{pid}").exists() else "absent") for pid in pids}
        state.update(app2_exit=app2_exit, cleanup_app1_exit=app1_exit, vite_exit=vite_exit,
                     test_mount_present=mount_present(task.name), exact_pid_states=exact_pid_states)
        # Postflight load is attribution metadata, not a process/fixture correctness
        # gate: the benchmark itself can affect the short load average. The
        # preflight gate still refuses to launch under non-comparable host load.
        postflight_load = host_load_average()
        state["postflight_load"] = postflight_load
        state["postflight_load_gate_failure"] = benchmark_load_failure(postflight_load)
        state["pass"] = bool(state.get("pass") and not state["test_mount_present"]
                             and all(value == "absent" for value in exact_pid_states.values()))
        write_owned_run_receipt(task, task_identity, state)
        print(json.dumps(state, indent=2), flush=True)
        for stream in (app2_stream, app1_stream, vite_stream):
            if stream is not None:
                stream.close()
    return 0 if state["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
