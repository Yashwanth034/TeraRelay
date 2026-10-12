#!/usr/bin/env python3
import importlib.util
import json
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "prepare-remote-import-engines.py"

failures = []

def check(condition, message):
    if not condition:
        failures.append(message)
        print("FAIL", message)
    else:
        print("PASS", message)

check(SCRIPT.exists(), "helper preparation script exists")

module = None
if SCRIPT.exists():
    spec = importlib.util.spec_from_file_location("remote_import_packaging", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)

if module:
    required = {"yt-dlp", "deno", "ffmpeg", "ffprobe", "gallery-dl"}
    for target in ["linux-x64", "windows-x64", "macos-arm64", "macos-x64"]:
        assets = module.assets_for_target(target)
        names = {asset.name for asset in assets}
        check(required <= names, f"{target} has all core Remote Import helpers")
        if target in {"linux-x64", "windows-x64"}:
            check("aria2c" in names, f"{target} bundles aria2 acceleration")
        else:
            check("aria2c" not in names, f"{target} uses the built-in downloader fallback instead of a brittle aria2 sidecar")
        for asset in assets:
            check(bool(re.fullmatch(r"[0-9a-f]{64}", asset.sha256)), f"{target}/{asset.name} has pinned sha256")
            check(asset.url.startswith("https://github.com/"), f"{target}/{asset.name} uses HTTPS GitHub release asset")
            check("/" not in asset.destination and "\\" not in asset.destination, f"{target}/{asset.name} destination is a basename")

    linux_aria2 = next(asset for asset in module.assets_for_target("linux-x64") if asset.name == "aria2c")
    check(
        "abcfy2/aria2-static-build@1.37.0" in linux_aria2.source
        and linux_aria2.archive_member == "aria2c",
        "linux-x64 aria2 uses the pinned self-contained musl static archive",
    )

    linux_gallery = next(asset for asset in module.assets_for_target("linux-x64") if asset.name == "gallery-dl")
    check(
        getattr(linux_gallery, "build_kind", None) == "gallery-dl-pyinstaller"
        and "mikf/gallery-dl" in linux_gallery.source
        and "ef30ea97c68d0d03ec49bd810ffe3fb4965559b7" in linux_gallery.url
        and linux_gallery.sha256 == "c2a4a83437d89d95018dd71281e0ead69dce3c02aa0cdccc3fcb4d0f220fc692",
        "linux-x64 gallery-dl is built from pinned source on the release runner instead of using the upstream glibc-sensitive executable",
    )

    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--target", "linux-x64", "--dry-run"],
        cwd=ROOT,
        text=True,
        capture_output=True,
    )
    check(result.returncode == 0, "packaging dry-run succeeds without downloading")
    check("yt-dlp" in result.stdout and "ffmpeg" in result.stdout and "gallery-dl" in result.stdout, "dry-run reports selected helpers")

    hashed_requirements = getattr(module, "GALLERY_LINUX_BUILD_REQUIREMENTS", "")
    requirement_lines = [line.strip() for line in hashed_requirements.splitlines() if line.strip()]
    check(
        bool(requirement_lines)
        and any("pyinstaller-6.22.3" in line for line in requirement_lines)
        and any("requests-2.34.2" in line for line in requirement_lines)
        and all(line.startswith("https://files.pythonhosted.org/") and "#sha256=" in line for line in requirement_lines),
        "linux gallery-dl build dependencies use direct hash-pinned wheel URLs without package-index resolution",
    )
    check(
        any("charset_normalizer-3.5.2-py3-none-any.whl" in line for line in requirement_lines)
        and not any("charset_normalizer-3.5.2-cp" in line for line in requirement_lines),
        "linux gallery-dl dependency lock avoids a CPython-minor-specific charset-normalizer wheel",
    )

    check(hasattr(module, "_prepare_asset"), "helper preparation dispatches source-built helpers explicitly")
    if hasattr(module, "_prepare_asset"):
        from unittest import mock
        with tempfile.TemporaryDirectory(prefix="terarelay-gallery-build-dispatch-") as temp_dir:
            temp = pathlib.Path(temp_dir)
            source = temp / "source.tar.gz"
            source.write_bytes(b"fixture")
            destination = temp / "gallery-dl"
            gallery_asset = module._gallery_linux_source_asset()
            with mock.patch.object(module, "_build_gallery_dl_from_source") as build_gallery, mock.patch.object(module, "_extract_or_copy") as extract_copy:
                module._prepare_asset(gallery_asset, source, destination)
                check(build_gallery.call_count == 1 and extract_copy.call_count == 0, "linux gallery-dl preparation uses the source-build path")

    check(hasattr(module, "_probe_prepared_asset"), "helper preparation runtime-probes extracted executables")
    if hasattr(module, "_probe_prepared_asset"):
        with tempfile.TemporaryDirectory(prefix="terarelay-helper-probe-test-") as temp_dir:
            temp = pathlib.Path(temp_dir)
            asset = module.Asset(
                name="aria2c",
                destination="aria2c",
                url="https://example.invalid/aria2c",
                sha256="0" * 64,
                source="test-fixture",
            )
            good = temp / "good-helper"
            good.write_text("#!/bin/sh\nexit 0\n")
            good.chmod(0o755)
            try:
                module._probe_prepared_asset(asset, good, "linux-x64")
                good_probe = True
            except Exception:
                good_probe = False
            check(good_probe, "runtime probe accepts a helper that launches successfully")

            bad = temp / "bad-helper"
            bad.write_text("#!/bin/sh\nexit 23\n")
            bad.chmod(0o755)
            try:
                module._probe_prepared_asset(asset, bad, "linux-x64")
                bad_probe_rejected = False
            except RuntimeError:
                bad_probe_rejected = True
            check(bad_probe_rejected, "runtime probe rejects a helper that exits nonzero")

        check(hasattr(module, "_runtime_probe_supported"), "helper probing knows when the runner CPU cannot execute the target architecture")
        if hasattr(module, "_runtime_probe_supported"):
            check(
                not module._runtime_probe_supported("macos-x64", "arm64"),
                "macOS Intel helpers are not executed on Apple Silicon release runners",
            )
            check(
                module._runtime_probe_supported("macos-arm64", "arm64"),
                "macOS Apple Silicon helpers remain runtime-probed on matching runners",
            )

config = json.loads((ROOT / "app/src-tauri/tauri.conf.json").read_text())
resources = config.get("bundle", {}).get("resources", [])
check(any("remote-import" in str(value) for value in resources), "Tauri bundle includes Remote Import helper resources")

release = (ROOT / ".github/workflows/release.yml").read_text()
check("helper_target" in release, "desktop release matrix declares helper target")
check("prepare-remote-import-engines.py" in release, "desktop release prepares verified Remote Import helpers")

backend = (ROOT / "app/src-tauri/src/commands/remote_import.rs").read_text()
lib = (ROOT / "app/src-tauri/src/lib.rs").read_text()
check("app_data_dir" in backend and "managed" in backend.lower(), "yt-dlp managed copy lives in TeraRelay app data")
check("cmd_remote_import_update_engines" in backend, "backend exposes independent yt-dlp refresh command")
check("commands::cmd_remote_import_update_engines" in lib, "yt-dlp refresh command is registered")

if failures:
    raise SystemExit(1)
print("Remote Import packaging checks passed")
