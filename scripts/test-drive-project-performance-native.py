#!/usr/bin/env python3
"""Paired local-vs-TeraRelay Linux FUSE project-workload measurement.

This is a private synthetic QA benchmark. It measures local staging/project
filesystem behavior only and does not claim Telegram/cloud throughput.  The
"restart-first-access" case is an explicit cold *proxy*: the TeraRelay app is
freshly restarted and the benchmark has not touched the fixture yet, but the
host OS page cache is intentionally not dropped globally.
"""
from pathlib import Path
from contextlib import contextmanager
import argparse
import hashlib
import json
import os
import sqlite3
import stat
import statistics
import subprocess
import time
import uuid


def require(value, message):
    if not value:
        raise AssertionError(message)


def private_profile_roots(project, task, mount, app_env):
    """Prove that measured XDG roots belong to the active private QA task.

    Never measure the virtual FUSE mount as local disk usage, and never inspect
    a normal profile selected by the host's environment.
    """
    project = Path(project)
    task = Path(task)
    mount = Path(mount)
    require(task.parent == project / "terarelay-private-qa-tasks" and task.is_dir()
            and not task.is_symlink() and not task.parent.is_symlink()
            and task.resolve(strict=True).is_relative_to(project.resolve(strict=True)),
            "Expected a private RepoTunnel benchmark task")
    require(mount == task / "mount" and mount.is_dir() and not mount.is_symlink(),
            "Expected the private task's FUSE mount")
    require(Path(app_env.get("TERARELAY_DRIVE_MOUNT_PATH", "")) == mount,
            "QA process mount must match the private benchmark task")
    roots = {}
    for name, variable in (
        ("data", "XDG_DATA_HOME"),
        ("cache", "XDG_CACHE_HOME"),
        ("config", "XDG_CONFIG_HOME"),
        ("runtime", "XDG_RUNTIME_DIR"),
    ):
        expected = task / name
        require(Path(app_env.get(variable, "")) == expected,
                f"QA {variable} must match the private benchmark task")
        require(expected.is_dir() and not expected.is_symlink()
                and not os.path.ismount(expected),
                f"QA {variable} cannot be a link or another mounted filesystem")
        roots[name] = expected
    return roots


def owned_disk_footprint(root, task, seen_inodes=None):
    """Count actual local inode blocks without following links or crossing mounts.

    st_blocks is measured in 512-byte units. It does not include physical
    filesystem metadata or external caches, and is not a cloud storage metric.
    """
    root = Path(os.path.abspath(root))
    task = Path(os.path.abspath(task))
    require(task.is_dir() and not task.is_symlink() and not task.parent.is_symlink(),
            "Owned task directory required")
    require(root.parent == task and root.is_dir() and not root.is_symlink(),
            "Refusing disk accounting outside a private task's direct child")
    require(not os.path.ismount(root), "Refusing to account a mounted filesystem as local disk")
    require(root.resolve(strict=True).is_relative_to(task.resolve(strict=True)),
            "Refusing disk accounting through a path outside the task")
    root_device = root.lstat().st_dev
    result = {
        "root": root.name, "regular_files": 0, "directories": 0,
        "logical_file_bytes": 0, "allocated_file_bytes": 0,
        "allocated_directory_bytes": 0, "hardlinks_deduplicated": 0,
        "symlinks_skipped": 0, "mounts_skipped": 0, "other_entries_skipped": 0,
    }
    if seen_inodes is None:
        seen_inodes = set()
    # Use directory descriptors and O_NOFOLLOW rather than pathname scandir:
    # an entry replaced by a symlink between stat and open must not escape
    # the owned QA subtree. Fail closed on a racing directory replacement.
    open_flags = (
        os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_CLOEXEC", 0)
    )
    todo = [(os.open(root, open_flags), root)]
    try:
        while todo:
            directory_fd, directory_path = todo.pop()
            try:
                directory_info = os.fstat(directory_fd)
                require(directory_info.st_dev == root_device,
                        "Owned disk subtree changed filesystem during traversal")
                result["directories"] += 1
                result["allocated_directory_bytes"] += directory_info.st_blocks * 512
                with os.scandir(directory_fd) as listing:
                    for child in listing:
                        info = os.stat(
                            child.name, dir_fd=directory_fd, follow_symlinks=False
                        )
                        if stat.S_ISLNK(info.st_mode):
                            result["symlinks_skipped"] += 1
                        elif info.st_dev != root_device:
                            result["mounts_skipped"] += 1
                        elif stat.S_ISDIR(info.st_mode):
                            child_path = directory_path / child.name
                            if os.path.ismount(child_path):
                                result["mounts_skipped"] += 1
                            else:
                                child_fd = os.open(
                                    child.name, open_flags, dir_fd=directory_fd
                                )
                                todo.append((child_fd, child_path))
                        elif stat.S_ISREG(info.st_mode):
                            result["regular_files"] += 1
                            result["logical_file_bytes"] += info.st_size
                            identity = (info.st_dev, info.st_ino)
                            if identity not in seen_inodes:
                                result["allocated_file_bytes"] += info.st_blocks * 512
                                seen_inodes.add(identity)
                            else:
                                result["hardlinks_deduplicated"] += 1
                        else:
                            result["other_entries_skipped"] += 1
            finally:
                os.close(directory_fd)
    finally:
        for fd, _ in todo:
            os.close(fd)
    result["allocated_total_bytes"] = (
        result["allocated_file_bytes"] + result["allocated_directory_bytes"]
    )
    return result


def measure_local_storage_footprint(task, private_roots, local_control):
    """Account only private QA disk roots, never the mounted Drive namespace."""
    require(set(private_roots) == {"data", "cache", "config", "runtime"},
            "Incomplete private QA profile root ownership")
    # A private QA inode hardlinked between data and cache consumes blocks
    # once. Per-root file/logical counts remain independent; allocated bytes
    # are uniquely attributed to the first profile root containing that inode.
    shared_qa_inodes = set()
    profile = {
        name: owned_disk_footprint(private_roots[name], task, shared_qa_inodes)
        for name in ("data", "cache", "config", "runtime")
    }
    # Local control remains a separate comparison sample, not part of the
    # private QA profile's aggregate allocation.
    control = owned_disk_footprint(local_control, task)
    return {
        "classification": "QA private local on-disk footprint, not cloud transfer or FUSE virtual size",
        "unit": "filesystem st_blocks * 512 bytes, deduplicating hard links",
        "exclusions": "mounted Drive namespace, normal user profile, unowned storage, external OS caches",
        "qa_profile": profile,
        "qa_profile_allocated_bytes": sum(
            item["allocated_total_bytes"] for item in profile.values()
        ),
        "local_control_fixture": control,
    }


def process_identity(pid):
    return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]


def mount_present(mount):
    escaped = str(mount).replace("\\", "\\134").replace(" ", "\\040").replace("\t", "\\011")
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        left, right = line.split(" - ", 1)
        if left.split()[4] == escaped and right.split()[0].startswith("fuse"):
            return True
    return False


def owned_mount(args):
    project = Path(__file__).resolve().parent.parent
    raw_env = Path(f"/proc/{args.qa_pid}/environ").read_bytes().decode()
    app_env = dict(item.split("=", 1) for item in raw_env.split("\0") if "=" in item)
    require(app_env.get("TERARELAY_QA_FEATURE_A") == "1", "Refusing a real account")
    require(app_env.get("VITE_TERA_QA_FEATURE_A") == "1", "Missing frontend synthetic QA flag")
    mount = args.mount.resolve(strict=True)
    require(mount.is_relative_to(project) and mount_present(mount), "Expected owned Linux FUSE mount")
    require(Path(app_env["TERARELAY_DRIVE_MOUNT_PATH"]).resolve(strict=True) == mount,
            "Mount does not belong to the QA process")
    data = Path(app_env["XDG_DATA_HOME"]).resolve(strict=True)
    require(data.is_relative_to(project), "Expected private QA profile")
    with sqlite3.connect(f"file:{data/'io.terarelay.desktop/shares.db'}?mode=ro", uri=True) as db:
        require(db.execute("SELECT count(*) FROM logical_channels WHERE logical_id='qa-feature-a-movies'")
                .fetchone()[0] == 1, "Expected seeded private QA account")
    return project, mount, app_env


def payload(index, size=4096):
    seed = hashlib.sha256(f"terarelay-perf-{index}".encode()).digest()
    return (seed * ((size + len(seed) - 1) // len(seed)))[:size]


# Node still opens and reads its source INSIDE the timed child process.
# /proc/self/fd/<pinned-parent>/editor.txt fixes the ownership race without
# changing the path-open/read nature of the historical Node workload.
DEFAULT_BENCHMARK_METHODOLOGY_VERSION = "secure-fd-v2"

NODE_SOURCE = (
    "const fs=require('fs');"
    "const p='/proc/self/fd/'+process.env.TERARELAY_PERF_ROOT_FD+'/editor.txt';"
    "const f=fs.openSync(p,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);"
    "try{const s=fs.fstatSync(f);"
    "if(!s.isFile()||s.size!==65536||String(s.dev)!==process.env.TERARELAY_PERF_ROOT_DEV)"
    "process.exit(3);"
    "const b=fs.readFileSync(f);if(b.length!==65536)process.exit(3);"
    "console.log('perf-node-ok');}finally{fs.closeSync(f);}\n"
)


def require_probe_modes(state, large_files, kernel_cache_diagnostics):
    require(state.get("large_project_files", 0) == large_files,
            "Large-project benchmark mode changed across the required restart")
    require(state.get("kernel_cache_diagnostics", False) == kernel_cache_diagnostics,
            "Kernel-cache diagnostic mode changed across the required restart")


def kernel_cache_diagnostics_setting(env):
    """Extra Linux read-open samples are off unless explicitly requested."""
    raw = env.get("TERARELAY_PERF_CACHE_DIAGNOSTICS", "0")
    require(raw in ("0", "1"), "Kernel-cache diagnostics must be disabled or set to 1")
    return raw == "1"


def large_project_files_setting(env):
    """Opt-in only: exactly 1024 tiny source files or the unchanged default."""
    raw = env.get("TERARELAY_PERF_LARGE_PROJECT_FILES", "0")
    require(raw in ("0", "1024"), "Large-project fixture must be disabled or exactly 1024 files")
    return int(raw)


def prepare_large_project_fixture(root):
    """Reproducible 32x32 metadata working set under the owned QA/local root.

    Each source file is 256 bytes (256 KiB logical total); real disk allocation
    is accounted separately. This is synthetic metadata pressure, not a claim
    about actual cloud transfer or laptop SSD savings.
    """
    with owned_default_directory(root) as (root_fd, device):
        try:
            os.mkdir("large-tree", dir_fd=root_fd)
        except FileExistsError as error:
            raise AssertionError(
                "Refusing to replace an existing large-project benchmark fixture"
            ) from error
        tree_fd = open_large_project_directory(root_fd, "large-tree", device)
        try:
            for directory in range(32):
                dirname = f"dir-{directory:02d}"
                os.mkdir(dirname, dir_fd=tree_fd)
                folder_fd = open_large_project_directory(tree_fd, dirname, device)
                try:
                    for index in range(32):
                        secure_sync_write(
                            folder_fd,
                            f"file-{index:02d}.bin",
                            payload(10_000 + directory * 32 + index, 256),
                        )
                finally:
                    os.close(folder_fd)
        finally:
            os.close(tree_fd)


# Never follow linked parent directories or file entries into an unowned
# tree. dir_fd-scoped operations close the symlink-swap window between checking
# a directory and reading its children. Linux is required by this FUSE probe.
_LARGE_DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def open_large_project_directory(parent_fd, name, filesystem_device):
    fd = os.open(name, _LARGE_DIR_FLAGS, dir_fd=parent_fd)
    if os.fstat(fd).st_dev != filesystem_device:
        os.close(fd)
        raise AssertionError("Large-project directory crossed a filesystem mount")
    return fd


def open_large_project_tree(root):
    root_fd = os.open(root, _LARGE_DIR_FLAGS)
    try:
        return open_large_project_directory(
            root_fd, "large-tree", os.fstat(root_fd).st_dev
        )
    finally:
        os.close(root_fd)


def checked_large_file_stat(folder_fd, name, filesystem_device):
    metadata = os.stat(name, dir_fd=folder_fd, follow_symlinks=False)
    require(
        stat.S_ISREG(metadata.st_mode) and metadata.st_dev == filesystem_device,
        "Large-project source file is not a regular owned file",
    )
    require(metadata.st_size == 256, "Large-project source file has wrong size")
    return metadata.st_size


def scan_large_project_metadata(root):
    """Paired readdir+stat from no-follow descriptors under the QA fixture."""
    tree_fd = open_large_project_tree(root)
    try:
        device = os.fstat(tree_fd).st_dev
        with os.scandir(tree_fd) as listing:
            directories = sorted(entry.name for entry in listing)
        require(directories == [f"dir-{index:02d}" for index in range(32)],
                "Large-project scan observed unexpected source directories")
        files = bytes_total = 0
        for dirname in directories:
            folder_fd = open_large_project_directory(tree_fd, dirname, device)
            try:
                with os.scandir(folder_fd) as listing:
                    names = sorted(entry.name for entry in listing)
                require(names == [f"file-{index:02d}.bin" for index in range(32)],
                        "Large-project scan observed unexpected source files")
                for name in names:
                    bytes_total += checked_large_file_stat(folder_fd, name, device)
                    files += 1
            finally:
                os.close(folder_fd)
        return {"directories": len(directories), "files": files, "bytes": bytes_total}
    finally:
        os.close(tree_fd)


def stat_large_project_files(root):
    """Known-name stat without readdir; never follow intermediate symlinks."""
    tree_fd = open_large_project_tree(root)
    try:
        device = os.fstat(tree_fd).st_dev
        total = 0
        for directory in range(32):
            folder_fd = open_large_project_directory(tree_fd, f"dir-{directory:02d}", device)
            try:
                for index in range(32):
                    total += checked_large_file_stat(
                        folder_fd, f"file-{index:02d}.bin", device
                    )
            finally:
                os.close(folder_fd)
        require(total == 1024 * 256, "Large-project metadata stat missed fixture")
        return {"files": 1024, "bytes": total}
    finally:
        os.close(tree_fd)


@contextmanager
def create_owned_fixture_root(root):
    """Create QA fixtures under an already-owned, non-symlink parent."""
    parent_fd = os.open(root.parent, _LARGE_DIR_FLAGS)
    try:
        parent_device = os.fstat(parent_fd).st_dev
        os.mkdir(root.name, mode=0o755, dir_fd=parent_fd)
        root_fd = open_large_project_directory(parent_fd, root.name, parent_device)
        try:
            yield root_fd, parent_device
        finally:
            os.close(root_fd)
    finally:
        os.close(parent_fd)


def prepare_fixture(root):
    with create_owned_fixture_root(root) as (root_fd, device):
        os.mkdir("tree", dir_fd=root_fd)
        tree_fd = open_large_project_directory(root_fd, "tree", device)
        try:
            for directory in range(8):
                dirname = f"dir-{directory:02d}"
                os.mkdir(dirname, dir_fd=tree_fd)
                folder_fd = open_large_project_directory(tree_fd, dirname, device)
                try:
                    for index in range(8):
                        secure_sync_write(
                            folder_fd, f"file-{index:02d}.bin",
                            payload(directory * 8 + index),
                        )
                finally:
                    os.close(folder_fd)
        finally:
            os.close(tree_fd)
        secure_sync_write(root_fd, "editor.txt", payload(999, 64 * 1024))
        os.mkdir("rename", dir_fd=root_fd)
        rename_fd = open_large_project_directory(root_fd, "rename", device)
        try:
            for index in range(24):
                secure_sync_write(
                    rename_fd, f"item-{index:02d}.txt", payload(2000 + index, 1024)
                )
        finally:
            os.close(rename_fd)
        os.mkdir("node", dir_fd=root_fd)
        node_fd = open_large_project_directory(root_fd, "node", device)
        try:
            secure_sync_write(node_fd, "index.js", NODE_SOURCE.encode())
        finally:
            os.close(node_fd)


def owned_cleanup_mount_id(directory_fd):
    """Linux mount ID prevents traversing nested bind mounts on the same device."""
    info = Path(f"/proc/self/fdinfo/{directory_fd}").read_text()
    for line in info.splitlines():
        if line.startswith("mnt_id:"):
            return int(line.partition(":")[2].strip())
    raise AssertionError("Cannot verify owned benchmark directory mount identity")


def remove_owned_fixture_children(directory_fd, device, mount_id):
    """Remove only entries reached under pinned, same-mount directories."""
    with os.scandir(directory_fd) as listing:
        children = [entry.name for entry in listing]
    for name in children:
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        require(info.st_dev == device, "Owned fixture entry crossed filesystem")
        if stat.S_ISDIR(info.st_mode):
            child_fd = os.open(name, _LARGE_DIR_FLAGS, dir_fd=directory_fd)
            try:
                actual = os.fstat(child_fd)
                require(actual.st_dev == device and actual.st_ino == info.st_ino,
                        "Owned fixture directory changed during cleanup")
                require(owned_cleanup_mount_id(child_fd) == mount_id,
                        "Refusing to cross a mounted directory during benchmark cleanup")
                remove_owned_fixture_children(child_fd, device, mount_id)
            finally:
                os.close(child_fd)
            latest = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            require(stat.S_ISDIR(latest.st_mode) and latest.st_ino == info.st_ino,
                    "Owned fixture directory was replaced during cleanup")
            os.rmdir(name, dir_fd=directory_fd)
        elif stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
            os.unlink(name, dir_fd=directory_fd)
        else:
            raise AssertionError("Refusing an unexpected file kind in owned benchmark cleanup")


def safe_cleanup(root):
    """Fail closed on parent links, directory swaps and nested mounts.

    Neither unlinking nor permissions work ever uses a pathname below the
    verified parent. No path-based chmod walk can affect unrelated files.
    """
    parent_fd = os.open(root.parent, _LARGE_DIR_FLAGS)
    try:
        try:
            info = os.stat(root.name, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        require(stat.S_ISDIR(info.st_mode),
                "Owned benchmark root unexpectedly became a non-directory or symlink")
        root_fd = os.open(root.name, _LARGE_DIR_FLAGS, dir_fd=parent_fd)
        try:
            actual = os.fstat(root_fd)
            require(actual.st_ino == info.st_ino
                    and actual.st_dev == os.fstat(parent_fd).st_dev,
                    "Owned benchmark root is replaced or mounted elsewhere")
            mount_id = owned_cleanup_mount_id(parent_fd)
            require(owned_cleanup_mount_id(root_fd) == mount_id,
                    "Refusing a mounted benchmark cleanup root")
            remove_owned_fixture_children(root_fd, actual.st_dev, mount_id)
        finally:
            os.close(root_fd)
        latest = os.stat(root.name, dir_fd=parent_fd, follow_symlinks=False)
        require(stat.S_ISDIR(latest.st_mode) and latest.st_ino == info.st_ino,
                "Owned benchmark root was replaced during cleanup")
        os.rmdir(root.name, dir_fd=parent_fd)
        try:
            os.stat(root.name, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        raise AssertionError("Owned benchmark cleanup failed")
    finally:
        os.close(parent_fd)


def timed(action):
    started = time.perf_counter()
    detail = action()
    return time.perf_counter() - started, detail


@contextmanager
def owned_default_directory(root, *children):
    """Pin the fixture and every child by descriptor; never follow a parent link."""
    directory_fd = os.open(root, _LARGE_DIR_FLAGS)
    try:
        device = os.fstat(directory_fd).st_dev
        for child in children:
            next_fd = open_large_project_directory(directory_fd, child, device)
            os.close(directory_fd)
            directory_fd = next_fd
        yield directory_fd, device
    finally:
        os.close(directory_fd)


def default_file_metadata(folder_fd, filename, device, expected_size=4096):
    info = os.stat(filename, dir_fd=folder_fd, follow_symlinks=False)
    require(stat.S_ISREG(info.st_mode) and info.st_dev == device
            and info.st_size == expected_size,
            "Default benchmark file is not an owned regular file of expected size")
    return info


def open_default_regular_file(folder_fd, filename, device, expected_size=4096):
    fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC,
                 dir_fd=folder_fd)
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_dev == device
                and info.st_size == expected_size,
                "Default benchmark handle is not an owned regular file")
        return fd
    except BaseException:
        os.close(fd)
        raise


def default_source_directory_names():
    return [f"dir-{index:02d}" for index in range(8)]


def default_source_file_names():
    return [f"file-{index:02d}.bin" for index in range(8)]


def scan_tree(root):
    files = bytes_total = 0
    with owned_default_directory(root, "tree") as (tree_fd, device):
        with os.scandir(tree_fd) as entries:
            names = sorted(entry.name for entry in entries)
        require(names == default_source_directory_names(), "Default scan saw unexpected directories")
        for directory in names:
            folder_fd = open_large_project_directory(tree_fd, directory, device)
            try:
                with os.scandir(folder_fd) as entries:
                    children = sorted(entry.name for entry in entries)
                require(children == default_source_file_names(), "Default scan saw unexpected files")
                for filename in children:
                    bytes_total += default_file_metadata(folder_fd, filename, device).st_size
                    files += 1
            finally:
                os.close(folder_fd)
    require(files == 64 and bytes_total == 64 * 4096, "Tree scan observed wrong fixture")
    return {"files": files, "bytes": bytes_total}


def read_tree(root):
    digest = hashlib.sha256()
    count = 0
    with owned_default_directory(root, "tree") as (tree_fd, device):
        for directory in default_source_directory_names():
            folder_fd = open_large_project_directory(tree_fd, directory, device)
            try:
                for filename in default_source_file_names():
                    fd = open_default_regular_file(folder_fd, filename, device)
                    with os.fdopen(fd, "rb") as stream:
                        data = stream.read(4097)
                    require(len(data) == 4096, "Small-file read was truncated")
                    digest.update(data)
                    count += 1
            finally:
                os.close(folder_fd)
    require(count == 64, "Small-file read missed fixture files")
    return {"files": count, "sha256": digest.hexdigest()}


def known_file_paths(root):
    return [
        root / "tree" / f"dir-{directory:02d}" / f"file-{index:02d}.bin"
        for directory in range(8)
        for index in range(8)
    ]


def stat_known_files(root):
    bytes_total = 0
    with owned_default_directory(root, "tree") as (tree_fd, device):
        for directory in default_source_directory_names():
            folder_fd = open_large_project_directory(tree_fd, directory, device)
            try:
                for filename in default_source_file_names():
                    bytes_total += default_file_metadata(folder_fd, filename, device).st_size
            finally:
                os.close(folder_fd)
    require(bytes_total == 64 * 4096, "Known-file stat observed wrong fixture")
    return {"files": 64, "bytes": bytes_total}


def open_close_known_files(root):
    count = 0
    with owned_default_directory(root, "tree") as (tree_fd, device):
        for directory in default_source_directory_names():
            folder_fd = open_large_project_directory(tree_fd, directory, device)
            try:
                for filename in default_source_file_names():
                    fd = open_default_regular_file(folder_fd, filename, device)
                    os.close(fd)
                    count += 1
            finally:
                os.close(folder_fd)
    require(count == 64, "Known-file open/close missed paths")
    return {"files": count}


def open_known_files(root):
    # Preserve the default buffered-handle behavior; only change how the
    # original source file descriptors are safely obtained.
    return open_known_files_for_cache_diagnostics(root, buffering=-1)


def open_known_files_for_cache_diagnostics(root, buffering=0):
    """Pin the 64 read handles safely without following linked QA parents."""
    handles = []
    root_fd = os.open(root, _LARGE_DIR_FLAGS)
    try:
        device = os.fstat(root_fd).st_dev
        tree_fd = open_large_project_directory(root_fd, "tree", device)
        try:
            for directory in range(8):
                folder_fd = open_large_project_directory(
                    tree_fd, f"dir-{directory:02d}", device
                )
                try:
                    for index in range(8):
                        fd = open_default_regular_file(
                            folder_fd, f"file-{index:02d}.bin", device
                        )
                        try:
                            handle = os.fdopen(fd, "rb", buffering=buffering)
                        except BaseException:
                            os.close(fd)
                            raise
                        handles.append(handle)
                finally:
                    os.close(folder_fd)
        finally:
            os.close(tree_fd)
        return handles
    except BaseException:
        for handle in handles:
            handle.close()
        raise
    finally:
        os.close(root_fd)


def pread_open_handles(handles):
    """Bypass Python stream buffering while keeping the same live open fds."""
    digest = hashlib.sha256()
    bytes_total = 0
    for handle in handles:
        data = os.pread(handle.fileno(), 4096, 0)
        require(len(data) == 4096, "Unbuffered same-handle read was truncated")
        digest.update(data)
        bytes_total += len(data)
    require(len(handles) == 64 and bytes_total == 64 * 4096,
            "Unbuffered same-handle read missed known files")
    return {"files": 64, "bytes": bytes_total, "sha256": digest.hexdigest()}


def pread_reopened_known_files(root):
    """Reopen known paths safely, measuring path/open overhead plus pread.

    The Drive/local pair share this syscall sequence. It is NOT a pure kernel
    cache measurement because path resolution/open/close are included.
    """
    root_fd = os.open(root, _LARGE_DIR_FLAGS)
    digest = hashlib.sha256()
    total = 0
    try:
        device = os.fstat(root_fd).st_dev
        tree_fd = open_large_project_directory(root_fd, "tree", device)
        try:
            for directory in range(8):
                folder_fd = open_large_project_directory(
                    tree_fd, f"dir-{directory:02d}", device
                )
                try:
                    for index in range(8):
                        name = f"file-{index:02d}.bin"
                        fd = os.open(
                            name,
                            os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=folder_fd,
                        )
                        try:
                            info = os.fstat(fd)
                            require(
                                stat.S_ISREG(info.st_mode) and info.st_dev == device
                                and info.st_size == 4096,
                                "Unbuffered reopened file is not an owned 4KiB source",
                            )
                            data = os.pread(fd, 4096, 0)
                            require(len(data) == 4096, "Unbuffered reopened read was truncated")
                            digest.update(data)
                            total += len(data)
                        finally:
                            os.close(fd)
                finally:
                    os.close(folder_fd)
        finally:
            os.close(tree_fd)
    finally:
        os.close(root_fd)
    require(total == 64 * 4096, "Unbuffered reopened read missed fixture files")
    return {"files": 64, "bytes": total, "sha256": digest.hexdigest()}


def read_open_files(handles):
    digest = hashlib.sha256()
    bytes_total = 0
    for handle in handles:
        handle.seek(0)
        data = handle.read(4096)
        require(len(data) == 4096, "Open-handle read was truncated")
        digest.update(data)
        bytes_total += len(data)
    require(len(handles) == 64 and bytes_total == 64 * 4096,
            "Open-handle read missed fixture")
    return {"files": len(handles), "bytes": bytes_total, "sha256": digest.hexdigest()}


def read_known_files(root):
    digest = hashlib.sha256()
    bytes_total = 0
    with owned_default_directory(root, "tree") as (tree_fd, device):
        for directory in default_source_directory_names():
            folder_fd = open_large_project_directory(tree_fd, directory, device)
            try:
                for filename in default_source_file_names():
                    fd = open_default_regular_file(folder_fd, filename, device)
                    with os.fdopen(fd, "rb") as stream:
                        data = stream.read(4097)
                    require(len(data) == 4096, "Known-file read was truncated")
                    digest.update(data)
                    bytes_total += len(data)
            finally:
                os.close(folder_fd)
    require(bytes_total == 64 * 4096, "Known-file read missed fixture")
    return {"files": 64, "bytes": bytes_total, "sha256": digest.hexdigest()}


def secure_sync_write(directory_fd, name, data, mode=0o640):
    """Create one private benchmark file without following an existing link."""
    fd = os.open(
        name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
        mode, dir_fd=directory_fd,
    )
    with os.fdopen(fd, "wb") as stream:
        os.fchmod(stream.fileno(), mode)
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def editor_save(root, revision):
    data = payload(5000 + revision, 64 * 1024)
    temp = f".editor-{revision}.tmp"
    with owned_default_directory(root) as (root_fd, device):
        default_file_metadata(root_fd, "editor.txt", device, 64 * 1024)
        secure_sync_write(root_fd, temp, data)
        os.replace(temp, "editor.txt", src_dir_fd=root_fd, dst_dir_fd=root_fd)
        fd = open_default_regular_file(root_fd, "editor.txt", device, 64 * 1024)
        with os.fdopen(fd, "rb") as stream:
            actual = stream.read(64 * 1024 + 1)
            permission = os.fstat(stream.fileno()).st_mode & 0o777
        require(actual == data, "Editor-style replacement changed bytes")
        require(permission == 0o640, "Editor-style replacement lost mode")
    return {"bytes": len(data)}


def rename_roundtrip(root, revision):
    with owned_default_directory(root, "rename") as (directory_fd, device):
        expected = [f"item-{index:02d}.txt" for index in range(24)]
        with os.scandir(directory_fd) as entries:
            names = sorted(entry.name for entry in entries)
        require(names == expected, "Rename roundtrip saw unexpected entries")
        for name in expected:
            default_file_metadata(directory_fd, name, device, 1024)
        for index in range(24):
            os.rename(
                f"item-{index:02d}.txt", f"item-{index:02d}.r{revision}",
                src_dir_fd=directory_fd, dst_dir_fd=directory_fd,
            )
        for index in range(24):
            os.rename(
                f"item-{index:02d}.r{revision}", f"item-{index:02d}.txt",
                src_dir_fd=directory_fd, dst_dir_fd=directory_fd,
            )
        with os.scandir(directory_fd) as entries:
            remaining = sorted(entry.name for entry in entries)
        require(remaining == expected, "Rename roundtrip lost entries")
    return {"renames": 48}


def node_read(root):
    # Pin the owned project and script directories against parent-link swaps.
    # The child itself must still open its editor source through the pinned
    # root, so the timed workload includes the source open AND 64KiB read.
    with owned_default_directory(root) as (root_fd, device):
        node_fd = open_large_project_directory(root_fd, "node", device)
        try:
            source_fd = open_default_regular_file(
                node_fd, "index.js", device, len(NODE_SOURCE.encode())
            )
            try:
                source = os.pread(source_fd, len(NODE_SOURCE.encode()) + 1, 0)
                require(source == NODE_SOURCE.encode(), "Node source fixture was replaced")
                env = dict(
                    os.environ,
                    TERARELAY_PERF_ROOT_FD=str(root_fd),
                    TERARELAY_PERF_ROOT_DEV=str(device),
                )
                result = subprocess.run(
                    ["node", f"/proc/self/fd/{source_fd}"],
                    cwd=f"/proc/self/fd/{node_fd}",
                    pass_fds=(root_fd, node_fd, source_fd),
                    env=env, capture_output=True, text=True, timeout=20,
                )
            finally:
                os.close(source_fd)
        finally:
            os.close(node_fd)
    require(result.returncode == 0 and result.stdout.strip() == "perf-node-ok",
            f"Node source read failed: {(result.stdout + result.stderr)[-1000:]}")
    return {"stdout": "perf-node-ok"}


def loadavg():
    try:
        values = Path("/proc/loadavg").read_text().split()
        return {"one": float(values[0]), "five": float(values[1]), "fifteen": float(values[2])}
    except Exception:
        return None


def pair_once(name, roots, order, action, revision=None):
    observed = {}
    for label in order:
        root = roots[label]
        elapsed, detail = timed((lambda r=root: action(r)) if revision is None
                                else (lambda r=root, n=revision: action(r, n)))
        observed[label] = {"seconds": elapsed, "detail": detail}
    local = observed["local"]["seconds"]
    drive = observed["drive"]["seconds"]
    observed["local_relative_percent"] = (local / drive * 100.0) if drive > 0 else None
    observed["name"] = name
    observed["order"] = order
    return observed


def summarize(rounds):
    drive = [item["drive"]["seconds"] for item in rounds]
    local = [item["local"]["seconds"] for item in rounds]
    drive_median = statistics.median(drive)
    local_median = statistics.median(local)
    paired_ratios = [
        local_seconds / drive_seconds * 100.0 if drive_seconds else None
        for drive_seconds, local_seconds in zip(drive, local)
    ]
    return {
        "rounds": len(rounds),
        "drive_median_seconds": drive_median,
        "local_median_seconds": local_median,
        "median_local_relative_percent": local_median / drive_median * 100.0 if drive_median else None,
        "median_paired_local_relative_percent": statistics.median(
            ratio for ratio in paired_ratios if ratio is not None
        ) if any(ratio is not None for ratio in paired_ratios) else None,
        "drive_mad_seconds": statistics.median(
            abs(value - drive_median) for value in drive
        ),
        "local_mad_seconds": statistics.median(
            abs(value - local_median) for value in local
        ),
        "paired_local_relative_percent": paired_ratios,
        "drive_samples_seconds": drive,
        "local_samples_seconds": local,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mount", type=Path, required=True)
    parser.add_argument("--qa-pid", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--phase", choices=("prepare", "measure"), required=True)
    args = parser.parse_args()
    project, mount, app_env = owned_mount(args)
    large_files = large_project_files_setting(os.environ)
    kernel_cache_diag = kernel_cache_diagnostics_setting(os.environ)
    output = args.output.resolve()
    state_path = args.state.resolve()
    require(output.is_relative_to(project) and state_path.is_relative_to(project), "Receipts must stay in workspace")
    require(not output.is_relative_to(mount) and not state_path.is_relative_to(mount), "Receipts must stay outside mount")
    private_roots = private_profile_roots(project, output.parent, mount, app_env)
    report = {"phase": args.phase, "pass": False, "synthetic_only": True,
              "normal_profile_used": False, "cloud_transfer_tested": False,
              "large_project_files": large_files,
              "kernel_cache_diagnostics_enabled": kernel_cache_diag}
    if args.phase == "prepare":
        require(not state_path.exists(), "Refusing to replace existing performance state")
        token = uuid.uuid4().hex
        drive = mount / "Archive QA" / ("project-performance-" + token)
        local = output.parent / ("project-performance-local-" + token)
        prepare_fixture(drive)
        prepare_fixture(local)
        if large_files:
            prepare_large_project_fixture(drive)
            prepare_large_project_fixture(local)
        state = {"token": token, "drive": str(drive), "local": str(local),
                 "large_project_files": large_files,
                 "kernel_cache_diagnostics": kernel_cache_diag,
                 "prepare_pid": args.qa_pid, "prepare_starttime": process_identity(args.qa_pid)}
        state_path.write_text(json.dumps(state, indent=2) + "\n")
        report.update({"pass": True, "state": state, "restart_required": True})
    else:
        state = json.loads(state_path.read_text())
        token = state["token"]
        require(len(token) == 32 and all(c in "0123456789abcdef" for c in token), "Invalid benchmark token")
        drive = mount / "Archive QA" / ("project-performance-" + token)
        local = output.parent / ("project-performance-local-" + token)
        require(state["drive"] == str(drive) and state["local"] == str(local), "Benchmark ownership mismatch")
        require_probe_modes(state, large_files, kernel_cache_diag)
        require(args.qa_pid != state["prepare_pid"] or process_identity(args.qa_pid) != state["prepare_starttime"],
                "Measurement requires replacement TeraRelay process")
        old_stat = Path(f"/proc/{state['prepare_pid']}/stat")
        require(not old_stat.exists() or process_identity(state["prepare_pid"]) != state["prepare_starttime"],
                "Prepare process is still alive")
        require(drive.is_dir() and local.is_dir(), "Prepared benchmark fixtures missing")
        roots = {"drive": drive, "local": local}
        report["load_before"] = loadavg()
        # Explicit restart-first-access cold proxy.  No benchmark read/stat has touched either fixture since restart.
        first_scan = pair_once("restart_first_access_scan", roots, ["drive", "local"], scan_tree)
        first_read = pair_once("restart_first_access_read", roots, ["local", "drive"], read_tree)
        first_large_scan = (
            pair_once("large_project_restart_first_scan", roots, ["drive", "local"],
                      scan_large_project_metadata) if large_files else None
        )
        large_scan = []
        large_known_stat = []
        warm_scan = []
        warm_read = []
        isolated_stat = []
        isolated_read = []
        open_close = []
        open_handle_read = []
        same_open_pread = []
        reopen_pread = []
        editor = []
        renames = []
        node = []
        preopened = {label: open_known_files(root) for label, root in roots.items()}
        diagnostic_preopened = {}
        try:
            if kernel_cache_diag:
                for label, root in roots.items():
                    diagnostic_preopened[label] = open_known_files_for_cache_diagnostics(root)
            for index in range(5):
                order = ["drive", "local"] if index % 2 == 0 else ["local", "drive"]
                warm_scan.append(pair_once("warm_scan", roots, order, scan_tree))
                if large_files:
                    large_scan.append(pair_once("large_project_scan", roots, order,
                                                scan_large_project_metadata))
                    large_known_stat.append(pair_once(
                        "large_project_known_path_stat", roots, list(reversed(order)),
                        stat_large_project_files
                    ))
                warm_read.append(pair_once("warm_read", roots, list(reversed(order)), read_tree))
                isolated_stat.append(pair_once("known_file_stat", roots, order, stat_known_files))
                isolated_read.append(pair_once("known_file_read", roots, list(reversed(order)), read_known_files))
                open_close.append(pair_once("known_file_open_close", roots, order, open_close_known_files))
                open_handle_read.append(pair_once("open_handle_read", preopened, list(reversed(order)), read_open_files))
                if kernel_cache_diag:
                    same_open_pread.append(pair_once(
                        "unbuffered_pread_same_open_64x4KiB", diagnostic_preopened,
                        order, pread_open_handles
                    ))
                    reopen_pread.append(pair_once(
                        "unbuffered_pread_reopen_64x4KiB", roots,
                        list(reversed(order)), pread_reopened_known_files
                    ))
                editor.append(pair_once("editor_replace_save", roots, order, editor_save, index))
                renames.append(pair_once("rename_roundtrip", roots, list(reversed(order)), rename_roundtrip, index))
                node.append(pair_once("node_source_read", roots, order, node_read))
        finally:
            for handles in list(preopened.values()) + list(diagnostic_preopened.values()):
                for handle in handles:
                    handle.close()
        report["pass"] = True
        report.update(
            cache_cases={
                "restart_first_access": {
                    "classification": "cold proxy: fresh TeraRelay process and first benchmark access; host OS page cache was not globally dropped",
                    "scan": first_scan,
                    "small_file_read": first_read,
                },
                "warm_cached": {
                    "classification": "immediate repeated access in the same mounted/local fixtures",
                    "scan": summarize(warm_scan),
                    "small_file_read": summarize(warm_read),
                },
            },
            isolated_operations={
                "known_file_stat_64": summarize(isolated_stat),
                "known_file_read_64x4KiB": summarize(isolated_read),
                "known_file_open_close_64": summarize(open_close),
                "open_handle_read_64x4KiB": summarize(open_handle_read),
            },
            representative_workloads={
                "editor_replace_save_64KiB": summarize(editor),
                "rename_24_files_out_and_back": summarize(renames),
                "node_process_reads_64KiB_source": summarize(node),
            },
            load_after=loadavg(),
            methodology={
                "version": DEFAULT_BENCHMARK_METHODOLOGY_VERSION,
                "access_policy": "descriptor-rooted O_NOFOLLOW source/open/rename/editor/Node and mount-checked owner cleanup",
                "historical_comparison": "Path/open syscall sequences differ from accepted L-V historical runs; do not interpret relative changes across harness versions as a Rust speedup",
                "node_method": "Node opens and reads its editor source inside the timed child using a pinned root-directory descriptor",
                "pairing": "Drive and local control executed adjacent in time; order alternated across five warm/mutation rounds",
                "relative_percent": "local median seconds / Drive median seconds * 100; 100 means equal elapsed time, lower is slower than local",
                "cold_limit": "No global drop_caches or privileged cache eviction was used; restart-first-access is not physical-disk cold",
                "scope": "private synthetic Linux FUSE/local staging only; no live Telegram/cloud throughput",
            },
        )
        if kernel_cache_diag:
            report["kernel_cache_diagnostics"] = {
                "same_open_handle_pread": summarize(same_open_pread),
                "reopen_path_and_pread": summarize(reopen_pread),
                "scope": "optional synthetic local/FUSE diagnostic; reopen includes lookup/open/close overhead, not standalone kernel cache proof",
            }
        if large_files:
            report["large_project_metadata"] = {
                "classification": "opt-in synthetic 1024 x 256-byte source-file working set; QA FUSE/local only",
                "files": large_files,
                "directories": 32,
                "restart_first_access_scan": first_large_scan,
                "warm_scan_readdir_and_stat": summarize(large_scan),
                "known_path_stat": summarize(large_known_stat),
                "limitations": "not live cloud throughput, physical cold cache, or actual offload savings",
            }
        # Disk accounting is intentionally AFTER all timed operations.
        # Counting the local control before restart-first-access would warm its
        # inode/page cache and contaminate the cold-proxy measurement.
        try:
            report["local_storage_footprint"] = measure_local_storage_footprint(
                output.parent, private_roots, local
            )
        except (AssertionError, OSError) as error:
            report["pass"] = False
            report["local_storage_footprint_error"] = str(error)
        try:
            safe_cleanup(drive)
            safe_cleanup(local)
            report["owned_fixture_cleanup"] = True
        except Exception as error:
            report.update({"pass": False, "owned_fixture_cleanup": False, "cleanup_error": str(error)})
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2), flush=True)
    return 0 if report["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
