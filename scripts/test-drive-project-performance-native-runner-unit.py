from importlib.util import module_from_spec, spec_from_file_location
from contextlib import redirect_stdout
import hashlib
import io
import json
import os
import stat
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

RUNNER_PATH = Path(__file__).with_name("test-drive-project-performance-native-runner.py")
SPEC = spec_from_file_location("terarelay_performance_runner", RUNNER_PATH)
RUNNER = module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)

PROBE_PATH = Path(__file__).with_name("test-drive-project-performance-native.py")
PROBE_SPEC = spec_from_file_location("terarelay_performance_probe", PROBE_PATH)
PROBE = module_from_spec(PROBE_SPEC)
PROBE_SPEC.loader.exec_module(PROBE)


class NativeBinaryFreshnessTests(unittest.TestCase):
    def test_runner_has_native_binary_freshness_guard(self):
        self.assertTrue(
            hasattr(RUNNER, "stale_native_binary_inputs"),
            "the benchmark runner must reject an executable older than source inputs",
        )

    def test_reports_only_source_inputs_newer_than_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "terarelay"
            stale_source = root / "drive_mount.rs"
            current_source = root / "older.rs"
            for path in (binary, stale_source, current_source):
                path.write_text("fixture", encoding="utf-8")
            os.utime(binary, ns=(100, 100))
            os.utime(stale_source, ns=(300, 300))
            os.utime(current_source, ns=(50, 50))

            self.assertEqual(
                RUNNER.stale_native_binary_inputs(binary, [current_source, stale_source]),
                [stale_source],
            )

    def test_missing_binary_marks_every_existing_input_stale(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "missing-terarelay"
            first = root / "drive_mount.rs"
            second = root / "drive_read_lifetime.rs"
            first.write_text("fixture", encoding="utf-8")
            second.write_text("fixture", encoding="utf-8")

            self.assertEqual(
                RUNNER.stale_native_binary_inputs(binary, [first, second]),
                [first, second],
            )


class BenchmarkTaskOwnershipTests(unittest.TestCase):
    def make_synthetic_runner(self, base):
        root = base / "mock-project"
        root.mkdir()
        crate = root / "app/src-tauri"
        (crate / "src").mkdir(parents=True)
        binary = crate / "target/debug/terarelay"
        binary.parent.mkdir(parents=True)
        for path in (crate / "Cargo.toml", crate / "Cargo.lock",
                     crate / "build.rs", crate / "src/main.rs"):
            path.write_text("mock-source")
        binary.write_text("mock-binary")
        os.utime(binary, ns=(4_000_000_000_000_000_000,
                             4_000_000_000_000_000_000))
        seed = root / "seed"
        seed.mkdir()
        (seed / "seed-file.txt").write_text("synthetic-seed")
        return root, seed

    def run_until_port_gate(self, root, seed, task, port_available=False,
                            expect_launch=False):
        argv = ["runner.py", "--project-root", str(root),
                "--task-dir", str(task), "--seed-data", str(seed)]
        with mock.patch.object(sys, "argv", argv):
            with mock.patch.object(RUNNER, "fuse_device_failure", return_value=None):
                with mock.patch.object(RUNNER, "host_load_average",
                                       return_value={"one": 1.0, "five": 1.0,
                                                     "fifteen": 1.0}):
                    with mock.patch.object(RUNNER, "port_free",
                                           return_value=port_available):
                        with mock.patch.object(
                            RUNNER.subprocess, "Popen",
                            side_effect=RuntimeError("Synthetic launch intercepted")
                        ) as start:
                            with redirect_stdout(io.StringIO()):
                                try:
                                    status = RUNNER.main()
                                except SystemExit as error:
                                    status = error.code
        if expect_launch:
            start.assert_called_once()
        else:
            start.assert_not_called()
        return status

    def test_refuses_unowned_task_before_deleting_any_data_or_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "unrelated-important-work"
            (task / "data").mkdir(parents=True)
            sentinel = task / "data" / "must-survive.txt"
            sentinel.write_text("unrelated-user-data")
            self.assertNotEqual(self.run_until_port_gate(root, seed, task), 0)
            self.assertEqual(sentinel.read_text(), "unrelated-user-data")
            self.assertFalse((task / "performance-run.json").exists(),
                             "unowned path must not even receive a receipt")

    def test_preserved_or_wrong_owner_marker_cannot_clear_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            for kind in ("preserved", "wrong-workspace", "wrong-task"):
                with self.subTest(kind=kind):
                    task = root / "terarelay-private-qa-tasks" / ("native-project-performance-" + kind)
                    (task / "data").mkdir(parents=True)
                    sentinel = task / "data" / "must-survive.txt"
                    sentinel.write_text("keep")
                    marker = {
                        "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                        "taskId": task.name, "label": "Synthetic marker",
                        "createdAt": 123456789, "preserved": False,
                    }
                    if kind == "preserved":
                        marker["preserved"] = True
                    elif kind == "wrong-workspace":
                        marker["workspaceId"] = "workspace-someone-else"
                    else:
                        marker["taskId"] = "native-project-performance-someone-else"
                    (task / ".repotunnel-temp.json").write_text(json.dumps(marker))
                    self.assertNotEqual(self.run_until_port_gate(root, seed, task), 0)
                    self.assertEqual(sentinel.read_text(), "keep")
                    self.assertFalse((task / "performance-run.json").exists())

    def test_seed_symlink_does_not_copy_unowned_source_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root, seed = self.make_synthetic_runner(base)
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-linked-seed"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic linked seed",
                "createdAt": 123456789, "preserved": False,
            }))
            outside = base / "unowned-private-source.txt"
            outside.write_text("outside-private-content")
            (seed / "linked.txt").symlink_to(outside)
            self.assertNotEqual(
                self.run_until_port_gate(root, seed, task, port_available=True), 0
            )
            self.assertEqual(outside.read_text(), "outside-private-content")
            self.assertFalse((task / "data" / "linked.txt").exists(),
                             "seed copy must never read an external symlink target")

    def test_owned_receipt_symlink_cannot_overwrite_unowned_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-linked-receipt"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic receipt symlink",
                "createdAt": 123456789, "preserved": False,
            }))
            outside = root / "private-unrelated-report.json"
            outside.write_text("unrelated-data")
            (task / "performance-run.json").symlink_to(outside)
            self.assertNotEqual(self.run_until_port_gate(root, seed, task), 0)
            self.assertEqual(outside.read_text(), "unrelated-data")

    def test_linked_log_filename_never_overwrites_unowned_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-linked-log"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic linked log",
                "createdAt": 123456789, "preserved": False,
            }))
            outside = root / "unowned-log.txt"
            outside.write_text("outside-original")
            (task / "performance-vite.log").symlink_to(outside)
            self.assertEqual(
                self.run_until_port_gate(root, seed, task,
                                         port_available=True, expect_launch=True), 1
            )
            self.assertEqual(outside.read_text(), "outside-original")
            self.assertFalse((task / "performance-vite.log").is_symlink())

    def test_marked_task_refuses_same_device_bind_mount(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-mounted-root"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic mounted task",
                "createdAt": 123456789, "preserved": False,
            }))
            original = RUNNER.owned_task_mount_id
            def simulate_mount(fd):
                base = original(fd)
                return base + 1 if os.readlink(f"/proc/self/fd/{fd}").endswith(task.name) else base
            with mock.patch.object(RUNNER, "owned_task_mount_id", side_effect=simulate_mount):
                self.assertNotEqual(self.run_until_port_gate(root, seed, task), 0)
            self.assertFalse((task / "performance-run.json").exists())

    def test_owned_cleanup_refuses_nested_mount_even_on_same_device(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-bind-test"
            nested = task / "data" / "nested"
            nested.mkdir(parents=True)
            sentinel = nested / "outside-mount-sentinel.txt"
            sentinel.write_text("must-survive")
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic nested mount test",
                "createdAt": 123456789, "preserved": False,
            }))
            actual_mount_id = RUNNER.owned_task_mount_id
            seen = []

            def simulated_mount_id(fd):
                location = os.readlink(f"/proc/self/fd/{fd}")
                seen.append(location)
                mount_id = actual_mount_id(fd)
                return mount_id + 1 if location.endswith("/nested") else mount_id

            with mock.patch.object(RUNNER, "owned_task_mount_id",
                                   side_effect=simulated_mount_id):
                self.assertNotEqual(
                    self.run_until_port_gate(root, seed, task, port_available=True), 0
                )
            self.assertTrue(any(path.endswith("/nested") for path in seen))
            self.assertEqual(sentinel.read_text(), "must-survive")
            self.assertFalse((task / "data" / "seed-file.txt").exists())

    def test_task_path_swap_after_seed_copy_cannot_create_unowned_dirs(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root, seed = self.make_synthetic_runner(base)
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-after-copy"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic post-copy swap",
                "createdAt": 123456789, "preserved": False,
            }))
            outside = base / "unowned-sibling"
            outside.mkdir()
            original_copy = RUNNER.copy_owned_seed_tree
            swapped = []

            def swap_after_copy(*args, **kwargs):
                original_copy(*args, **kwargs)
                task.rename(task.parent / (task.name + "-old"))
                task.symlink_to(outside, target_is_directory=True)
                swapped.append(True)

            with mock.patch.object(RUNNER, "copy_owned_seed_tree",
                                   side_effect=swap_after_copy):
                try:
                    self.run_until_port_gate(root, seed, task,
                                             port_available=True)
                except (OSError, RuntimeError):
                    pass  # A swapped task may block safe receipt writing.
            self.assertTrue(swapped)
            self.assertEqual(list(outside.iterdir()), [],
                             "post-copy setup must not create outside directories")

    def test_task_path_swap_before_receipt_reset_cannot_delete_unowned_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root, seed = self.make_synthetic_runner(base)
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-before-reset"
            task.mkdir(parents=True)
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic receipt reset",
                "createdAt": 123456789, "preserved": False,
            }))
            outside = base / "unowned-sibling"
            outside.mkdir()
            sentinel = outside / "performance-state.json"
            sentinel.write_text("must-survive")
            original_cleanup = RUNNER.remove_owned_task_directory
            swapped = []

            def swap_after_last_cleanup(*args, **kwargs):
                original_cleanup(*args, **kwargs)
                if args[1] == "mount" and not swapped:
                    task.rename(task.parent / (task.name + "-old"))
                    task.symlink_to(outside, target_is_directory=True)
                    swapped.append(True)

            with mock.patch.object(RUNNER, "remove_owned_task_directory",
                                   side_effect=swap_after_last_cleanup):
                try:
                    self.run_until_port_gate(root, seed, task,
                                             port_available=True)
                except (OSError, RuntimeError):
                    pass
            self.assertTrue(swapped)
            self.assertEqual(sentinel.read_text(), "must-survive")

    def test_accepts_exact_existing_ownership_marker_for_private_task(self):
        with tempfile.TemporaryDirectory() as directory:
            root, seed = self.make_synthetic_runner(Path(directory))
            task = root / "terarelay-private-qa-tasks" / "native-project-performance-allowed"
            (task / "data").mkdir(parents=True)
            (task / "data" / "old-owned.txt").write_text("owned-old")
            marker = {
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic private benchmark",
                "createdAt": 123456789, "preserved": False,
            }
            (task / ".repotunnel-temp.json").write_text(json.dumps(marker))
            self.assertEqual(self.run_until_port_gate(root, seed, task), 1)
            self.assertTrue((task / "data" / "old-owned.txt").exists(),
                            "busy port must not clear existing private data")
            self.assertFalse((task / "data" / "seed-file.txt").exists())
            self.assertEqual(
                self.run_until_port_gate(
                    root, seed, task, port_available=True, expect_launch=True
                ), 1
            )
            self.assertFalse((task / "data" / "old-owned.txt").exists())
            self.assertTrue((task / "data" / "seed-file.txt").exists())
            self.assertTrue((task / "performance-run.json").exists())


class BenchmarkLoadGateTests(unittest.TestCase):
    def test_accepts_only_load_within_the_historical_low_load_envelope(self):
        accepted = [
            {"one": 1.77, "five": 2.65, "fifteen": 1.99},
            {"one": 1.03, "five": 1.29, "fifteen": 1.24},
            {"one": 1.49, "five": 1.50, "fifteen": 1.33},
        ]
        for sample in accepted:
            with self.subTest(sample=sample):
                self.assertIsNone(RUNNER.benchmark_load_failure(sample))

    def test_rejects_high_load_in_any_average(self):
        cases = [
            {"one": 2.01, "five": 1.0, "fifteen": 1.0},
            {"one": 1.0, "five": 2.81, "fifteen": 1.0},
            {"one": 1.0, "five": 1.0, "fifteen": 2.11},
            {"one": 1.99, "five": 3.47, "fifteen": 6.72},
        ]
        for sample in cases:
            with self.subTest(sample=sample):
                self.assertIsNotNone(RUNNER.benchmark_load_failure(sample))

    def test_rejects_unavailable_or_malformed_load(self):
        for sample in (None, {}, {"one": 1.0, "five": "busy", "fifteen": 1.0},
                       {"one": float("nan"), "five": 1.0, "fifteen": 1.0}):
            with self.subTest(sample=sample):
                self.assertIsNotNone(RUNNER.benchmark_load_failure(sample))

    def test_high_load_preflight_rejects_before_fixture_cleanup_or_process_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "app/src-tauri"
            (crate / "src").mkdir(parents=True)
            binary = crate / "target/debug/terarelay"
            binary.parent.mkdir(parents=True)
            for path in (crate / "Cargo.toml", crate / "Cargo.lock",
                         crate / "build.rs", crate / "src/main.rs"):
                path.write_text("fixture", encoding="utf-8")
            binary.write_text("fixture", encoding="utf-8")
            os.utime(binary, ns=(4_000_000_000_000_000_000,
                                 4_000_000_000_000_000_000))

            task = root / "terarelay-private-qa-tasks/native-project-performance-load-gate"
            seed = root / "terarelay-private-qa-tasks/seed"
            task.mkdir(parents=True)
            seed.mkdir()
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic load gate fixture",
                "createdAt": 123456789, "preserved": False,
            }))
            data = task / "data"
            data.mkdir()

            argv = [
                "runner.py",
                "--project-root", str(root),
                "--task-dir", str(task),
                "--seed-data", str(seed),
            ]
            high_load = {"one": 1.99, "five": 3.47, "fifteen": 6.72}
            with mock.patch.object(RUNNER, "fuse_device_failure", return_value=None):
                with mock.patch.object(RUNNER, "host_load_average", return_value=high_load):
                    with mock.patch.object(RUNNER.shutil, "rmtree") as remove_tree:
                        with mock.patch.object(RUNNER.subprocess, "Popen") as start_process:
                            with mock.patch.object(sys, "argv", argv):
                                with redirect_stdout(io.StringIO()):
                                    exit_code = RUNNER.main()

            self.assertEqual(exit_code, 1)
            self.assertTrue(data.is_dir(), "load rejection must not delete fixtures")
            remove_tree.assert_not_called()
            start_process.assert_not_called()
            receipt = json.loads((task / "performance-run.json").read_text(encoding="utf-8"))
            self.assertEqual(receipt["preflight_load"], high_load)
            self.assertEqual(receipt["postflight_load"], high_load)
            self.assertIsNotNone(receipt["postflight_load_gate_failure"])
            self.assertFalse(receipt["pass"])
            self.assertIn("host load gate rejected benchmark", receipt["error"])


class FuseCapabilityTests(unittest.TestCase):
    def test_missing_fuse_device_reports_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "fuse"
            failure = RUNNER.fuse_device_failure(missing)
            self.assertIn("FUSE device", failure)
            self.assertIn("unavailable", failure)

    def test_non_character_fuse_path_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fuse"
            path.write_text("not a device", encoding="utf-8")
            failure = RUNNER.fuse_device_failure(path)
            self.assertIn("not a character device", failure)

    def test_unopenable_character_device_reports_permission_error(self):
        fake_stat = mock.Mock(st_mode=stat.S_IFCHR | 0o600)
        with mock.patch.object(Path, "stat", return_value=fake_stat):
            with mock.patch.object(RUNNER.os, "open", side_effect=PermissionError("denied")):
                failure = RUNNER.fuse_device_failure(Path("/dev/fuse"))
        self.assertIn("cannot be opened", failure)
        self.assertIn("denied", failure)

    def test_openable_character_device_is_closed_after_probe(self):
        fake_stat = mock.Mock(st_mode=stat.S_IFCHR | 0o600)
        flags = os.O_RDWR | getattr(os, "O_CLOEXEC", 0)
        with mock.patch.object(Path, "stat", return_value=fake_stat):
            with mock.patch.object(RUNNER.os, "open", return_value=123) as open_device:
                with mock.patch.object(RUNNER.os, "close") as close_device:
                    failure = RUNNER.fuse_device_failure(Path("/dev/fuse"))
        self.assertIsNone(failure)
        open_device.assert_called_once_with("/dev/fuse", flags)
        close_device.assert_called_once_with(123)

    def test_missing_fuse_preflight_rejects_before_cleanup_or_process_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "app/src-tauri"
            (crate / "src").mkdir(parents=True)
            binary = crate / "target/debug/terarelay"
            binary.parent.mkdir(parents=True)
            for path in (crate / "Cargo.toml", crate / "Cargo.lock",
                         crate / "build.rs", crate / "src/main.rs"):
                path.write_text("fixture", encoding="utf-8")
            binary.write_text("fixture", encoding="utf-8")
            os.utime(binary, ns=(4_000_000_000_000_000_000,
                                 4_000_000_000_000_000_000))

            task = root / "terarelay-private-qa-tasks/native-project-performance-fuse-gate"
            seed = root / "terarelay-private-qa-tasks/seed"
            task.mkdir(parents=True)
            seed.mkdir()
            (task / ".repotunnel-temp.json").write_text(json.dumps({
                "schemaVersion": 1, "workspaceId": "workspace-synthetic-qa-fixture",
                "taskId": task.name, "label": "Synthetic FUSE gate fixture",
                "createdAt": 123456789, "preserved": False,
            }))
            data = task / "data"
            data.mkdir()
            sentinel = data / "must-survive"
            sentinel.write_text("preserved", encoding="utf-8")

            argv = [
                "runner.py",
                "--project-root", str(root),
                "--task-dir", str(task),
                "--seed-data", str(seed),
            ]
            with mock.patch.object(RUNNER, "fuse_device_failure",
                                   return_value="FUSE device /dev/fuse is unavailable"):
                with mock.patch.object(RUNNER, "host_load_average",
                                       return_value={"one": 1.0, "five": 1.0, "fifteen": 1.0}):
                    with mock.patch.object(RUNNER.shutil, "rmtree") as remove_tree:
                        with mock.patch.object(RUNNER.subprocess, "Popen") as start_process:
                            with mock.patch.object(sys, "argv", argv):
                                with redirect_stdout(io.StringIO()):
                                    exit_code = RUNNER.main()

            self.assertEqual(exit_code, 1)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserved")
            remove_tree.assert_not_called()
            start_process.assert_not_called()
            receipt = json.loads((task / "performance-run.json").read_text(encoding="utf-8"))
            self.assertFalse(receipt["pass"])
            self.assertIn("FUSE device /dev/fuse is unavailable", receipt["error"])
            self.assertIn("FUSE preflight", receipt["error"])
            self.assertEqual(receipt["fuse_preflight_failure"],
                             "FUSE device /dev/fuse is unavailable")


class IsolatedFileOperationTests(unittest.TestCase):
    def test_known_file_paths_do_not_touch_the_filesystem(self):
        root = Path("/private/nonexistent-benchmark-root")
        paths = PROBE.known_file_paths(root)
        self.assertEqual(len(paths), 64)
        self.assertEqual(paths[0], root / "tree/dir-00/file-00.bin")
        self.assertEqual(paths[-1], root / "tree/dir-07/file-07.bin")

    def test_known_file_read_matches_expected_byte_count_and_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = hashlib.sha256()
            for path in PROBE.known_file_paths(root):
                path.parent.mkdir(parents=True, exist_ok=True)
                data = bytes([len(path.name)]) * 4096
                path.write_bytes(data)
                expected.update(data)

            observed = PROBE.read_known_files(root)
            self.assertEqual(observed["files"], 64)
            self.assertEqual(observed["bytes"], 64 * 4096)
            self.assertEqual(observed["sha256"], expected.hexdigest())

    def test_known_file_stat_reports_only_explicit_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in PROBE.known_file_paths(root):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"x" * 4096)

            observed = PROBE.stat_known_files(root)
            self.assertEqual(observed, {"files": 64, "bytes": 64 * 4096})

    def test_open_close_benchmark_opens_every_known_path_without_reading(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in PROBE.known_file_paths(root):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"x" * 4096)

            observed = PROBE.open_close_known_files(root)
            self.assertEqual(observed, {"files": 64})

    def test_open_handle_read_reads_all_bytes_from_preopened_descriptors(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = hashlib.sha256()
            for index, path in enumerate(PROBE.known_file_paths(root)):
                path.parent.mkdir(parents=True, exist_ok=True)
                data = bytes([index]) * 4096
                path.write_bytes(data)
                expected.update(data)

            handles = PROBE.open_known_files(root)
            try:
                observed = PROBE.read_open_files(handles)
            finally:
                for handle in handles:
                    handle.close()

            self.assertEqual(observed["files"], 64)
            self.assertEqual(observed["bytes"], 64 * 4096)
            self.assertEqual(observed["sha256"], expected.hexdigest())

    def test_summary_reports_robust_dispersion_and_paired_ratios(self):
        rounds = [
            {"drive": {"seconds": 10.0}, "local": {"seconds": 2.0}},
            {"drive": {"seconds": 20.0}, "local": {"seconds": 1.0}},
            {"drive": {"seconds": 30.0}, "local": {"seconds": 3.0}},
            {"drive": {"seconds": 40.0}, "local": {"seconds": 2.0}},
            {"drive": {"seconds": 50.0}, "local": {"seconds": 5.0}},
        ]

        observed = PROBE.summarize(rounds)

        self.assertEqual(observed["drive_mad_seconds"], 10.0)
        self.assertEqual(observed["local_mad_seconds"], 1.0)
        self.assertEqual(observed["paired_local_relative_percent"],
                         [20.0, 5.0, 10.0, 5.0, 10.0])
        self.assertEqual(observed["median_paired_local_relative_percent"], 10.0)

class PrivateDiskFootprintTests(unittest.TestCase):
    def test_owned_disk_footprint_counts_private_regular_files_without_following_links(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            task = project / "terarelay-private-qa-tasks" / "bench"
            data = task / "data"
            data.mkdir(parents=True)
            (data / "subdir").mkdir()
            (data / "first.bin").write_bytes(b"a" * 11)
            (data / "subdir" / "second.bin").write_bytes(b"b" * 17)
            outside = project / "normal-profile-secret.bin"
            outside.write_bytes(b"z" * 12345)
            (data / "outside-link").symlink_to(outside)
            (data / "outside-directory").symlink_to(project, target_is_directory=True)

            observed = PROBE.owned_disk_footprint(data, task)

            self.assertEqual(observed["regular_files"], 2)
            self.assertEqual(observed["logical_file_bytes"], 28)
            self.assertEqual(observed["symlinks_skipped"], 2)
            self.assertGreaterEqual(observed["allocated_file_bytes"], 0)
            self.assertEqual(observed["root"], "data")

    def test_private_profile_rejects_symlinked_task_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            project = base / "project"
            project.mkdir()
            outside = base / "outside"
            task = outside / "bench"
            for name in ("data", "cache", "config", "runtime", "mount"):
                (task / name).mkdir(parents=True)
            (project / "terarelay-private-qa-tasks").symlink_to(outside, target_is_directory=True)
            linked_task = project / "terarelay-private-qa-tasks" / "bench"
            env = {
                "XDG_DATA_HOME": str(linked_task / "data"),
                "XDG_CACHE_HOME": str(linked_task / "cache"),
                "XDG_CONFIG_HOME": str(linked_task / "config"),
                "XDG_RUNTIME_DIR": str(linked_task / "runtime"),
                "TERARELAY_DRIVE_MOUNT_PATH": str(linked_task / "mount"),
            }
            with self.assertRaises(AssertionError):
                PROBE.private_profile_roots(project, linked_task,
                                           linked_task / "mount", env)

    def test_owned_disk_footprint_scans_via_directory_handles_not_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            task = Path(directory) / "terarelay-private-qa-tasks" / "bench"
            root = task / "data"
            (root / "nested").mkdir(parents=True)
            (root / "nested" / "payload").write_bytes(b"abc")
            real_scandir = os.scandir

            def check_scandir_argument(target):
                self.assertIsInstance(
                    target, int,
                    "path-based scandir can follow a directory replaced by a symlink",
                )
                return real_scandir(target)

            with mock.patch.object(PROBE.os, "scandir", side_effect=check_scandir_argument):
                observed = PROBE.owned_disk_footprint(root, task)
            self.assertEqual(observed["logical_file_bytes"], 3)

    def test_owned_disk_footprint_refuses_outside_and_linked_roots(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            task = project / "terarelay-private-qa-tasks" / "bench"
            task.mkdir(parents=True)
            outside = project / "normal-profile"
            outside.mkdir()
            (task / "linked").symlink_to(outside, target_is_directory=True)
            with self.assertRaises(AssertionError):
                PROBE.owned_disk_footprint(outside, task)
            with self.assertRaises(AssertionError):
                PROBE.owned_disk_footprint(task / "linked", task)

    def test_private_profile_roots_require_exact_qa_task_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            task = project / "terarelay-private-qa-tasks" / "bench"
            for name in ("data", "cache", "config", "runtime", "mount"):
                (task / name).mkdir(parents=True)
            env = {
                "XDG_DATA_HOME": str(task / "data"),
                "XDG_CACHE_HOME": str(task / "cache"),
                "XDG_CONFIG_HOME": str(task / "config"),
                "XDG_RUNTIME_DIR": str(task / "runtime"),
                "TERARELAY_DRIVE_MOUNT_PATH": str(task / "mount"),
            }
            paths = PROBE.private_profile_roots(project, task, task / "mount", env)
            self.assertEqual(set(paths), {"data", "cache", "config", "runtime"})
            self.assertEqual(paths["data"], task / "data")
            invalid = dict(env, XDG_CACHE_HOME=str(project))
            with self.assertRaises(AssertionError):
                PROBE.private_profile_roots(project, task, task / "mount", invalid)

    def test_cross_root_hardlinks_count_actual_blocks_only_once(self):
        with tempfile.TemporaryDirectory() as directory:
            task = Path(directory) / "terarelay-private-qa-tasks" / "bench"
            roots = {}
            for name in ("data", "cache", "config", "runtime"):
                (task / name).mkdir(parents=True)
                roots[name] = task / name
            local = task / "project-performance-local-fixture"
            local.mkdir()
            original = roots["data"] / "original.bin"
            original.write_bytes(b"x" * 8192)
            os.link(original, roots["cache"] / "linked.bin")
            one_inode_blocks = original.stat().st_blocks * 512

            observed = PROBE.measure_local_storage_footprint(task, roots, local)

            self.assertEqual(observed["qa_profile"]["data"]["regular_files"], 1)
            self.assertEqual(observed["qa_profile"]["cache"]["regular_files"], 1)
            self.assertEqual(
                sum(part["allocated_file_bytes"] for part in observed["qa_profile"].values()),
                one_inode_blocks,
                "one physical hardlinked inode must be counted once across all QA roots",
            )
            self.assertEqual(observed["qa_profile"]["cache"]["hardlinks_deduplicated"], 1)

    def test_measure_local_footprint_excludes_virtual_mount_and_labels_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            task = Path(directory) / "terarelay-private-qa-tasks" / "bench"
            roots = {}
            for name in ("data", "cache", "config", "runtime"):
                (task / name).mkdir(parents=True)
                roots[name] = task / name
            local = task / "project-performance-local-fixture"
            local.mkdir()
            (local / "sample").write_bytes(b"l" * 37)
            virtual_mount = task / "mount"
            virtual_mount.mkdir()
            (virtual_mount / "remote-only").write_bytes(b"r" * 4096)
            (roots["data"] / "metadata.db").write_bytes(b"d" * 11)

            observed = PROBE.measure_local_storage_footprint(task, roots, local)

            self.assertEqual(observed["classification"], "QA private local on-disk footprint, not cloud transfer or FUSE virtual size")
            self.assertEqual(observed["qa_profile"]["data"]["logical_file_bytes"], 11)
            self.assertEqual(observed["local_control_fixture"]["logical_file_bytes"], 37)
            self.assertNotIn("mount", observed["qa_profile"])
            self.assertEqual(observed["qa_profile_allocated_bytes"],
                             sum(part["allocated_total_bytes"] for part in observed["qa_profile"].values()))

class OptionalLargeProjectFixtureTests(unittest.TestCase):
    def test_runner_passes_opt_in_mode_to_both_probe_phases_without_default_timeout_change(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / "terarelay-private-qa-tasks" / "bench"
            task.mkdir(parents=True)
            for enabled, expected_timeout in ((False, 180), (True, 480)):
                env = ({"TERARELAY_PERF_LARGE_PROJECT_FILES": "1024"}
                       if enabled else {})
                for phase in ("prepare", "measure"):
                    with self.subTest(enabled=enabled, phase=phase):
                        with mock.patch.object(RUNNER.subprocess, "run") as execute:
                            RUNNER.run_probe(root, env, task, 12345, phase)
                        kwargs = execute.call_args.kwargs
                        self.assertEqual(kwargs["timeout"], expected_timeout)
                        self.assertEqual(kwargs["env"], env)
                        self.assertIn("--phase", execute.call_args.args[0])
                        self.assertIn(phase, execute.call_args.args[0])

    def test_bounded_opt_in_configuration_only_accepts_1024_or_disabled(self):
        self.assertEqual(PROBE.large_project_files_setting({}), 0)
        self.assertEqual(PROBE.large_project_files_setting(
            {"TERARELAY_PERF_LARGE_PROJECT_FILES": "0"}), 0)
        self.assertEqual(PROBE.large_project_files_setting(
            {"TERARELAY_PERF_LARGE_PROJECT_FILES": "1024"}), 1024)
        for invalid in ("1", "-1", "1025", "4096", "not-a-number", ""):
            with self.subTest(invalid=invalid), self.assertRaises(AssertionError):
                PROBE.large_project_files_setting({
                    "TERARELAY_PERF_LARGE_PROJECT_FILES": invalid
                })

    def test_large_project_known_stats_reject_linked_parent_and_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            sandbox = Path(directory)
            root = sandbox / "owned"
            root.mkdir()
            PROBE.prepare_large_project_fixture(root)
            original = root / "large-tree" / "dir-00"
            outside = sandbox / "outside"
            original.rename(outside)
            original.symlink_to(outside, target_is_directory=True)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.stat_large_project_files(root)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.scan_large_project_metadata(root)
            original.unlink()
            (root / "large-tree").rename(sandbox / "saved-tree")
            (root / "large-tree").symlink_to(sandbox / "saved-tree",
                                            target_is_directory=True)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.stat_large_project_files(root)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.scan_large_project_metadata(root)

    def test_parent_swap_during_safe_open_cannot_escape_fixture(self):
        for operation in (PROBE.scan_large_project_metadata, PROBE.stat_large_project_files):
            with self.subTest(operation=operation.__name__):
                with tempfile.TemporaryDirectory() as directory:
                    sandbox = Path(directory)
                    root = sandbox / "owned"
                    root.mkdir()
                    PROBE.prepare_large_project_fixture(root)
                    parent = root / "large-tree" / "dir-00"
                    outside = sandbox / "outside"
                    real_open = os.open
                    swapped = []

                    def replace_at_open(path, flags, *args, **kwargs):
                        if path == "dir-00" and kwargs.get("dir_fd") is not None and not swapped:
                            parent.rename(outside)
                            parent.symlink_to(outside, target_is_directory=True)
                            swapped.append(True)
                        return real_open(path, flags, *args, **kwargs)

                    with mock.patch.object(PROBE.os, "open", side_effect=replace_at_open):
                        with self.assertRaises((AssertionError, OSError)):
                            operation(root)
                    self.assertTrue(swapped, "regression must hit the parent-open race")
                    self.assertEqual(len(list(outside.glob("file-*.bin"))), 32)

    def test_1024_file_metadata_cases_are_deterministic_and_detect_missing_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "private-project-fixture"
            root.mkdir()
            PROBE.prepare_large_project_fixture(root)
            scan = PROBE.scan_large_project_metadata(root)
            stats = PROBE.stat_large_project_files(root)
            self.assertEqual(scan["files"], 1024)
            self.assertEqual(stats["files"], 1024)
            self.assertEqual(scan["bytes"], 1024 * 256)
            self.assertEqual(stats["bytes"], 1024 * 256)
            self.assertEqual(scan["directories"], 32)
            self.assertEqual(len(list((root / "large-tree").glob("dir-*/*.bin"))), 1024)

            (root / "large-tree" / "dir-00" / "file-00.bin").unlink()
            with self.assertRaises(AssertionError):
                PROBE.scan_large_project_metadata(root)
            with self.assertRaises((AssertionError, FileNotFoundError)):
                PROBE.stat_large_project_files(root)


class OptionalKernelCacheDiagnosticTests(unittest.TestCase):
    def test_cache_probe_mode_must_survive_required_process_restart(self):
        saved = {"large_project_files": 0, "kernel_cache_diagnostics": False}
        PROBE.require_probe_modes(saved, 0, False)
        for changed_large, changed_cache in ((1024, False), (0, True), (1024, True)):
            with self.subTest(large=changed_large, cache=changed_cache):
                with self.assertRaises(AssertionError):
                    PROBE.require_probe_modes(saved, changed_large, changed_cache)

    def test_mode_is_strictly_disabled_by_default(self):
        self.assertFalse(PROBE.kernel_cache_diagnostics_setting({}))
        self.assertFalse(PROBE.kernel_cache_diagnostics_setting(
            {"TERARELAY_PERF_CACHE_DIAGNOSTICS": "0"}))
        self.assertTrue(PROBE.kernel_cache_diagnostics_setting(
            {"TERARELAY_PERF_CACHE_DIAGNOSTICS": "1"}))
        for invalid in ("", "true", "2", "-1"):
            with self.subTest(invalid=invalid), self.assertRaises(AssertionError):
                PROBE.kernel_cache_diagnostics_setting({
                    "TERARELAY_PERF_CACHE_DIAGNOSTICS": invalid})

    def test_unbuffered_same_handle_and_reopened_reads_match_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned"
            PROBE.prepare_fixture(root)
            handles = PROBE.open_known_files(root)
            try:
                with mock.patch.object(PROBE.os, "pread", wraps=os.pread) as pread:
                    first = PROBE.pread_open_handles(handles)
                    second = PROBE.pread_open_handles(handles)
                self.assertEqual(pread.call_count, 128)
            finally:
                for handle in handles:
                    handle.close()
            reopen = PROBE.pread_reopened_known_files(root)
            self.assertEqual(first, second)
            self.assertEqual(first, reopen)
            self.assertEqual(first["files"], 64)
            self.assertEqual(first["bytes"], 64 * 4096)

    def test_cache_preopened_handles_refuse_linked_source_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            PROBE.prepare_fixture(root)
            folder = root / "tree" / "dir-00"
            outside = base / "outside"
            folder.rename(outside)
            folder.symlink_to(outside, target_is_directory=True)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.open_known_files_for_cache_diagnostics(root)

    def test_reopened_read_rejects_linked_source_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            PROBE.prepare_fixture(root)
            original = root / "tree" / "dir-00"
            outside = base / "outside"
            original.rename(outside)
            original.symlink_to(outside, target_is_directory=True)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.pread_reopened_known_files(root)


class DefaultFixtureSymlinkSafetyTests(unittest.TestCase):
    def test_hardened_harness_has_distinct_methodology_version(self):
        self.assertEqual(PROBE.DEFAULT_BENCHMARK_METHODOLOGY_VERSION, "secure-fd-v2")
        self.assertIn("fs.openSync", PROBE.NODE_SOURCE)
        self.assertIn("TERARELAY_PERF_ROOT_FD", PROBE.NODE_SOURCE)

    def test_default_source_operations_refuse_linked_parent(self):
        actions = (
            PROBE.scan_tree, PROBE.read_tree, PROBE.stat_known_files,
            PROBE.read_known_files, PROBE.open_close_known_files,
            PROBE.open_known_files,
        )
        for action in actions:
            with self.subTest(action=action.__name__):
                with tempfile.TemporaryDirectory() as directory:
                    base = Path(directory)
                    root = base / "owned"
                    PROBE.prepare_fixture(root)
                    source = root / "tree" / "dir-00"
                    outside = base / "outside-source"
                    source.rename(outside)
                    source.symlink_to(outside, target_is_directory=True)
                    with self.assertRaises((AssertionError, OSError)):
                        value = action(root)
                        if isinstance(value, list):
                            for handle in value:
                                handle.close()

    def test_default_mutation_and_node_process_refuse_linked_parent(self):
        for name, action in (
            ("rename", lambda root: PROBE.rename_roundtrip(root, 7)),
            ("node", PROBE.node_read),
        ):
            with self.subTest(name=name):
                with tempfile.TemporaryDirectory() as directory:
                    base = Path(directory)
                    root = base / "owned"
                    PROBE.prepare_fixture(root)
                    source = root / name
                    outside = base / ("outside-" + name)
                    source.rename(outside)
                    source.symlink_to(outside, target_is_directory=True)
                    # The harmless existing Node fixture must be able to run
                    # if an unsafe implementation follows the linked cwd.
                    if name == "node":
                        (base / "editor.txt").write_bytes(PROBE.payload(999, 65536))
                    with self.assertRaises((AssertionError, OSError)):
                        action(root)

    def test_default_prepare_refuses_symlinked_archive_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            mount = base / "mock-mount"
            mount.mkdir()
            outside = base / "unowned-archive"
            outside.mkdir()
            (mount / "Archive QA").symlink_to(outside, target_is_directory=True)
            target = mount / "Archive QA" / "project-performance-fake"
            with self.assertRaises((AssertionError, OSError)):
                PROBE.prepare_fixture(target)
            self.assertFalse((outside / target.name).exists(),
                             "prepare must never create a fixture outside its owner")

    def test_default_cleanup_never_changes_linked_outside_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            owned = base / "owned"
            PROBE.prepare_fixture(owned)
            outside = base / "outside"
            outside.mkdir(mode=0o700)
            outside.chmod(0o700)
            observed_original = outside.stat().st_mode & 0o777
            original_chmod = os.chmod
            swapped = []

            def swap_before_unsafe_chmod(path, mode, *args, **kwargs):
                if (Path(path) == owned / "tree" / "dir-00"
                        and not swapped):
                    folder = owned / "tree" / "dir-00"
                    for child in folder.iterdir():
                        child.unlink()
                    folder.rmdir()
                    folder.symlink_to(outside, target_is_directory=True)
                    swapped.append(True)
                return original_chmod(path, mode, *args, **kwargs)

            with mock.patch.object(PROBE.os, "chmod",
                                   side_effect=swap_before_unsafe_chmod):
                PROBE.safe_cleanup(owned)
            self.assertFalse(owned.exists())
            self.assertEqual(outside.stat().st_mode & 0o777, observed_original,
                             "cleanup must not chmod unowned data")

    def test_node_source_opens_editor_inside_timed_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned"
            PROBE.prepare_fixture(root)
            actual_run = PROBE.subprocess.run
            observed = []

            def inspect_child(args, **kwargs):
                self.assertEqual(args[0], "node")
                self.assertIn("/proc/self/fd/", args[1])
                self.assertIn("TERARELAY_PERF_ROOT_FD", kwargs["env"])
                self.assertIn("TERARELAY_PERF_ROOT_DEV", kwargs["env"])
                self.assertIn(int(kwargs["env"]["TERARELAY_PERF_ROOT_FD"]),
                              kwargs["pass_fds"])
                self.assertIn("fs.openSync", PROBE.NODE_SOURCE)
                self.assertIn("fs.readFileSync", PROBE.NODE_SOURCE)
                observed.append(True)
                return actual_run(args, **kwargs)

            with mock.patch.object(PROBE.subprocess, "run", side_effect=inspect_child):
                self.assertEqual(PROBE.node_read(root), {"stdout": "perf-node-ok"})
            self.assertEqual(len(observed), 1)

    def test_node_child_uses_pinned_directory_after_path_swap(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            PROBE.prepare_fixture(root)
            node_dir = root / "node"
            other = base / "unowned-node"
            other.mkdir()
            (other / "index.js").write_text("console.log('outside-code')\n")
            actual_run = PROBE.subprocess.run
            swapped = []

            def swap_before_child(args, **kwargs):
                if not swapped:
                    node_dir.rename(root / "node-old")
                    node_dir.symlink_to(other, target_is_directory=True)
                    swapped.append(True)
                return actual_run(args, **kwargs)

            with mock.patch.object(PROBE.subprocess, "run",
                                   side_effect=swap_before_child):
                self.assertEqual(PROBE.node_read(root), {"stdout": "perf-node-ok"})
            self.assertTrue(swapped)
            self.assertEqual((other / "index.js").read_text(),
                             "console.log('outside-code')\n")

    def test_node_child_refuses_editor_symlink_swap(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            PROBE.prepare_fixture(root)
            outside = base / "unowned-editor.txt"
            outside.write_bytes(b"x" * 65536)
            original = root / "editor.txt"
            actual_run = PROBE.subprocess.run
            swapped = []

            def swap_before_child(args, **kwargs):
                if not swapped:
                    original.rename(root / "old-editor.txt")
                    original.symlink_to(outside)
                    swapped.append(True)
                return actual_run(args, **kwargs)

            with mock.patch.object(PROBE.subprocess, "run",
                                   side_effect=swap_before_child):
                with self.assertRaises(AssertionError):
                    PROBE.node_read(root)
            self.assertTrue(swapped)
            self.assertEqual(outside.read_bytes(), b"x" * 65536)

    def test_default_cleanup_refuses_a_simulated_nested_mount(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned"
            root.mkdir()
            nested = root / "nested"
            nested.mkdir()
            (nested / "sentinel.txt").write_text("private")
            original_mount_id = PROBE.owned_cleanup_mount_id
            calls = []

            def simulate_mount(fd):
                calls.append(fd)
                value = original_mount_id(fd)
                return value if len(calls) == 1 else value + 1

            with mock.patch.object(PROBE, "owned_cleanup_mount_id",
                                   side_effect=simulate_mount):
                with self.assertRaises((AssertionError, OSError)):
                    PROBE.safe_cleanup(root)
            self.assertGreaterEqual(len(calls), 2)
            self.assertEqual((nested / "sentinel.txt").read_text(), "private")

    def test_default_cleanup_does_not_follow_a_swapped_child_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            root.mkdir()
            original = root / "nested"
            original.mkdir()
            (original / "original.txt").write_text("private")
            outside = base / "unowned"
            outside.mkdir()
            (outside / "sentinel.txt").write_text("unowned")
            real_open = os.open
            swapped = []

            def swap_before_child_open(path, flags, *args, **kwargs):
                if (path == "nested" and kwargs.get("dir_fd") is not None
                        and not swapped):
                    original.rename(root / "moved-original")
                    original.symlink_to(outside, target_is_directory=True)
                    swapped.append(True)
                return real_open(path, flags, *args, **kwargs)

            with mock.patch.object(PROBE.os, "open", side_effect=swap_before_child_open):
                with self.assertRaises((AssertionError, OSError)):
                    PROBE.safe_cleanup(root)
            self.assertTrue(swapped)
            self.assertEqual((outside / "sentinel.txt").read_text(), "unowned")

    def test_default_cleanup_refuses_a_linked_root(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            outside = base / "unowned"
            outside.mkdir()
            (outside / "sentinel.txt").write_text("untouched")
            link = base / "project-performance-fake"
            link.symlink_to(outside, target_is_directory=True)
            with self.assertRaises((AssertionError, OSError)):
                PROBE.safe_cleanup(link)
            self.assertEqual((outside / "sentinel.txt").read_text(), "untouched")

    def test_default_known_stat_detects_directory_swap_at_open(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "owned"
            PROBE.prepare_fixture(root)
            source = root / "tree" / "dir-00"
            outside = base / "outside"
            swapped = []
            real_open = os.open

            def swap_at_open(path, flags, *args, **kwargs):
                if path == "dir-00" and kwargs.get("dir_fd") is not None and not swapped:
                    source.rename(outside)
                    source.symlink_to(outside, target_is_directory=True)
                    swapped.append(True)
                return real_open(path, flags, *args, **kwargs)

            with mock.patch.object(PROBE.os, "open", side_effect=swap_at_open):
                with self.assertRaises((AssertionError, OSError)):
                    PROBE.stat_known_files(root)
            self.assertTrue(swapped, "test must reach the secure parent-open boundary")


if __name__ == "__main__":
    unittest.main()
