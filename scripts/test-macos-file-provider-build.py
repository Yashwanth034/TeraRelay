#!/usr/bin/env python3
"""Exercise File Provider packaging with isolated macOS tool fixtures."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class FileProviderBuildTests(unittest.TestCase):
    def build(self, identity, profiles=False):
        with tempfile.TemporaryDirectory(prefix="terarelay-provider-test-") as temporary:
            root = Path(temporary)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copy2(ROOT / "scripts/build-macos-file-provider.sh", scripts)
            provider = root / "app/src-tauri/macos-file-provider"
            provider.mkdir(parents=True)
            (provider / "Info.plist").write_text("<plist/>")
            (provider / "TeraRelayFileProvider.entitlements").write_text("<plist/>")
            (provider.parent / "Cargo.toml").write_text('[package]\nversion = "0.2.0"\n')
            tools = root / "tools"
            tools.mkdir()
            binary = root / "swift-output"
            binary.mkdir()
            (binary / "TeraRelayFileProvider").write_text("fixture")
            fixtures = {
                "uname": '#!/bin/sh\nprintf "Darwin\\n"\n',
                "swift": '#!/bin/sh\ncase "$*" in *--show-bin-path*) printf "%s\\n" "$TEST_BIN_DIR";; esac\n',
                "plutil": "#!/bin/sh\nexit 0\n",
                "codesign": '#!/bin/sh\nprintf "%s\\n" "$*" >> "$TEST_SIGN_LOG"\n',
            }
            for name, source in fixtures.items():
                tool = tools / name
                tool.write_text(source)
                tool.chmod(0o755)
            sign_log = root / "codesign.log"
            env = os.environ.copy()
            for name in (
                "APPLE_SIGNING_IDENTITY", "TERARELAY_MAIN_APP_PROFILE_PATH",
                "TERARELAY_FILE_PROVIDER_PROFILE_PATH", "TERARELAY_MACOS_SWIFT_TRIPLE",
                "TERARELAY_MACOS_BUILD_NUMBER",
            ):
                env.pop(name, None)
            env.update(
                PATH=str(tools) + os.pathsep + env["PATH"],
                APPLE_SIGNING_IDENTITY=identity,
                TEST_BIN_DIR=str(binary),
                TEST_SIGN_LOG=str(sign_log),
            )
            if profiles:
                for variable, filename in (
                    ("TERARELAY_MAIN_APP_PROFILE_PATH", "main.profile"),
                    ("TERARELAY_FILE_PROVIDER_PROFILE_PATH", "provider.profile"),
                ):
                    profile = root / filename
                    profile.write_text("fixture provisioning profile")
                    env[variable] = str(profile)
            result = subprocess.run(
                ["/bin/bash", str(scripts / "build-macos-file-provider.sh")],
                env=env, capture_output=True, text=True, timeout=30,
            )
            signatures = sign_log.read_text() if sign_log.exists() else ""
            bundled = (provider / "build/TeraRelayFileProvider.appex/Contents/MacOS/TeraRelayFileProvider").exists()
            return result, signatures, bundled

    def test_ad_hoc_build_needs_no_production_profiles(self):
        result, signatures, bundled = self.build("-")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(bundled)
        self.assertIn("--sign -", signatures)
        self.assertNotIn("--timestamp", signatures)
        self.assertIn("--verify", signatures)

    def test_production_identity_still_requires_profiles(self):
        result, signatures, _ = self.build("Developer ID Application: Fixture")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Production File Provider signing requires", result.stderr)
        self.assertEqual(signatures, "")

    def test_production_identity_signs_with_profiles(self):
        result, signatures, bundled = self.build("Developer ID Application: Fixture", profiles=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(bundled)
        self.assertIn("--timestamp", signatures)
        self.assertIn("--options runtime", signatures)
        self.assertIn("--verify", signatures)


if __name__ == "__main__":
    unittest.main()
