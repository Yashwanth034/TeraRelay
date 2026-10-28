#!/usr/bin/env python3
"""Exercise installer backup safety with disposable profiles; never run apt."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]

class BackupSafety(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="reinstall-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.project = self.root / "project"
        scripts = self.project / "scripts"
        scripts.mkdir(parents=True)
        self.script = scripts / "reinstall-local-deb.sh"
        shutil.copyfile(ROOT / "scripts/reinstall-local-deb.sh", self.script)
        package = self.project / "app/src-tauri/target/release/bundle/deb/TeraRelay_0.2.0_amd64.deb"
        package.parent.mkdir(parents=True)
        tree = self.root / "deb"
        (tree / "DEBIAN").mkdir(parents=True)
        (tree / "DEBIAN/control").write_text("Package: tera-relay\nVersion: 0.2.0\nArchitecture: amd64\nMaintainer: Test <test@example.invalid>\nDescription: Backup safety fixture\n")
        subprocess.run(["dpkg-deb", "--build", str(tree), str(package)], check=True, capture_output=True)
        self.sha = hashlib.sha256(package.read_bytes()).hexdigest()
        self.home = self.root / "home"
        self.home.mkdir()
        self.data = self.home / ".local/share/io.terarelay.desktop"
        self.data.mkdir(parents=True, mode=0o700)
        (self.data / "session").write_bytes(b"disposable-session-fixture")
        (self.data / "session").chmod(0o600)
        (self.data / "session-link").symlink_to("session")
        for location in [".config", ".cache"]:
            p = self.home / location / "io.terarelay.desktop"
            p.mkdir(parents=True)
            (p / "sample").write_text(location)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.sudo_marker = self.root / "sudo-called"
        self.command("sudo", 'touch "$SUDO_MARKER"; exit 99')
        self.command("pgrep", 'exit 1')
        self.command("dpkg-query", "printf '0.2.0'")
        self.env = dict(os.environ, HOME=str(self.home), PATH=str(self.bin) + ":" + os.environ["PATH"], SUDO_MARKER=str(self.sudo_marker))
        for key in ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"]:
            self.env.pop(key, None)

    def command(self, name, body):
        p = self.bin / name
        p.write_text("#!/bin/sh\n" + body + "\n")
        p.chmod(0o755)

    def run_backup(self, sha=None):
        r = subprocess.run(["bash", str(self.script), sha or self.sha, "--backup-only"], env=self.env, capture_output=True, text=True)
        self.assertFalse(self.sudo_marker.exists(), "Backup-only or failure must never reach sudo/apt")
        return r

    def test_copies_profile_config_cache_and_preserves_session(self):
        r = self.run_backup()
        self.assertEqual(r.returncode, 0, r.stderr)
        saved = list((self.home / ".local/state/TeraRelay/backups").glob("backup-*"))
        self.assertEqual(len(saved), 1)
        backup = saved[0]
        self.assertEqual((backup / "app-data/session").read_bytes(), (self.data / "session").read_bytes())
        self.assertEqual((backup / "app-data/session").stat().st_mode & 0o777, 0o600)
        self.assertTrue((backup / "app-data/session-link").is_symlink())
        self.assertEqual((backup / "app-config/sample").read_text(), ".config")
        self.assertEqual((backup / "app-cache/sample").read_text(), ".cache")
        self.assertTrue((backup / "BACKUP_COMPLETE").is_file())

    def test_copy_failure_is_explicit_and_does_not_modify_source(self):
        self.command("cp", "exit 42")
        r = self.run_backup()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Backup failed", r.stderr)
        self.assertIn("installation was not attempted", r.stderr)
        self.assertEqual((self.data / "session").read_bytes(), b"disposable-session-fixture")
        self.assertFalse(list(self.home.rglob("BACKUP_COMPLETE")))

    def test_disappearing_destination_stops_with_specific_message(self):
        # Rename the test destination away mid-copy, reproducing ENOENT safely.
        real_cp = shutil.which("cp")
        self.command("cp", 'for target do :; done\nparent=$(dirname "$target")\nmv "$parent" "$parent.displaced"\nexec ' + real_cp + ' "$@"')
        r = self.run_backup()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Backup destination disappeared", r.stderr)
        self.assertIn("installation was not attempted", r.stderr)
        self.assertEqual((self.data / "session").read_bytes(), b"disposable-session-fixture")

    def test_checksum_mismatch_does_not_create_backup(self):
        r = self.run_backup("0" * 64)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("checksum mismatch", r.stderr)
        self.assertFalse((self.home / ".local/state/TeraRelay/backups").exists())

    def test_running_app_stops_before_backup(self):
        self.command("pgrep", "exit 0")
        r = self.run_backup()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Fully quit", r.stderr)
        self.assertFalse((self.home / ".local/state/TeraRelay/backups").exists())

if __name__ == "__main__":
    unittest.main()
