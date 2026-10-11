#!/usr/bin/env python3
"""Prepare verified standalone Remote Import helper binaries for a TeraRelay desktop bundle.

This script is release/build tooling only. It writes exclusively to
app/src-tauri/resources/remote-import, verifies every downloaded archive/binary
against a checksum pinned in this repository, and never installs anything into
the runner or user's PATH.
"""
from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUTPUT_DIR = ROOT / "app" / "src-tauri" / "resources" / "remote-import"
USER_AGENT = "TeraRelay-Remote-Import-Packager/1"

YT_DLP_TAG = "2026.08.19"
DENO_TAG = "v2.9.7"
FFMPEG_TAG = "n8.1.2-1"
ARIA_TAG = "release-2.0.3"
ARIA_LINUX_TAG = "1.37.0"
GALLERY_TAG = "2026.10.10"
GALLERY_LINUX_SOURCE_COMMIT = "ef30ea97c68d0d03ec49bd810ffe3fb4965559b7"
GALLERY_LINUX_SOURCE_SHA256 = "c2a4a83437d89d95018dd71281e0ead69dce3c02aa0cdccc3fcb4d0f220fc692"
GALLERY_LINUX_BUILD_REQUIREMENTS = """\
https://files.pythonhosted.org/packages/a9/ba/000a1996d4308bc65120167c21241a3b205464a2e0b58deda26ae8ac21d1/altgraph-0.17.5-py2.py3-none-any.whl#sha256=f3a22400bce1b0c701683820ac4f3b159cd301acab067c51c653e06961600597
https://files.pythonhosted.org/packages/0b/a7/71ac2cff56fec219ed242bb11b8efb69fcc4bec75db06fb7bfe35de520e6/certifi-2026.7.22-py3-none-any.whl#sha256=62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775
https://files.pythonhosted.org/packages/7f/c5/38806a25ab5e65fc178f39affeda20858efafede2fce1ffc2556cfc9fe73/charset_normalizer-3.5.2-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl#sha256=3d31298449090ab8d47b7b1b2a555ff73cac7ed438a08b7ac160980c7ebed649
https://files.pythonhosted.org/packages/58/a2/bb081bab032533a855d44de1d56f8e8426114ff1ba5d1f07a438a0a654f8/idna-3.20-py3-none-any.whl#sha256=ab7ae7122974553370f0bdb919e1a960b2cd1bc1ef0276416d896db81c14582c
https://files.pythonhosted.org/packages/63/34/ba1c580383c9eada3711951fef0795c80b829a078d72188184bcab9dd527/packaging-26.3-py3-none-any.whl#sha256=d7193f7c8e4e93f444fde0262bf90af30e16fa0ad0ad44cb553c87339b23cd1c
https://files.pythonhosted.org/packages/7b/b6/dd21d61fc5e25377be24eabf13e99e4e46548a5abe2bf7b15d8b3d9cc74a/pyinstaller-6.22.3-py3-none-manylinux2014_x86_64.whl#sha256=451a4ae14b719365bf1a2f0a99dae7b3463060061c3a394c70d5264cfb439528
https://files.pythonhosted.org/packages/8f/f1/3eee0263b487e571aed9204257b45bc2a0b6fec34d0a1adade4d8d8a968c/pyinstaller_hooks_contrib-2026.8-py3-none-any.whl#sha256=6d21d65323ea4467753afc21f52d7d0e69416ae8101dd88269bf273ad48db084
https://files.pythonhosted.org/packages/8d/59/b4572118e098ac8e46e399a1dd0f2d85403ce8bbaad9ec79373ed6badaf9/PySocks-1.7.1-py3-none-any.whl#sha256=2725bd0a9925919b9b51739eea5f9e2bae91e83288108a9ad338b2e3a4435ee5
https://files.pythonhosted.org/packages/a0/f4/c67b0b3f1b9245e8d266f0f112c500d50e5b4e83cb6f3b71b6528104182a/requests-2.34.2-py3-none-any.whl#sha256=2a0d60c172f83ac6ab31e4554906c0f3b3588d37b5cb939b1c061f4907e278e0
https://files.pythonhosted.org/packages/95/9c/c510029fc6ef33a6275cd2c5d3cecd6613dfd6aa401d57c54f1c18852ccf/setuptools-84.0.0-py3-none-any.whl#sha256=51a52592b3b99e102b609654876bd65f19f999935166d1352678931132b0c670
https://files.pythonhosted.org/packages/92/9d/c4e665119135114480843e7ab388fa94d8480650450e6f8e26b70d323a4c/urllib3-2.8.0-py3-none-any.whl#sha256=0cf3cae568d36aa9576b28dfb35f11328f1cb974ca7647d9475ebb86c75ac6e3
"""


@dataclasses.dataclass(frozen=True)
class Asset:
    name: str
    destination: str
    url: str
    sha256: str
    archive_member: str | None = None
    executable: bool = True
    source: str = ""
    build_kind: str | None = None


def _asset(name: str, destination: str, repo: str, tag: str, filename: str, sha256: str,
           archive_member: str | None = None) -> Asset:
    return Asset(
        name=name,
        destination=destination,
        url=f"https://github.com/{repo}/releases/download/{tag}/{filename}",
        sha256=sha256,
        archive_member=archive_member,
        source=f"{repo}@{tag}:{filename}",
    )


def _gallery_linux_source_asset() -> Asset:
    commit = GALLERY_LINUX_SOURCE_COMMIT
    return Asset(
        name="gallery-dl",
        destination="gallery-dl",
        url=f"https://github.com/mikf/gallery-dl/archive/{commit}.tar.gz",
        sha256=GALLERY_LINUX_SOURCE_SHA256,
        source=f"mikf/gallery-dl@{commit}:source",
        build_kind="gallery-dl-pyinstaller",
    )


def assets_for_target(target: str) -> list[Asset]:
    if target == "linux-x64":
        return [
            _asset("yt-dlp", "yt-dlp", "yt-dlp/yt-dlp", YT_DLP_TAG, "yt-dlp_linux", "58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a"),
            _asset("deno", "deno", "denoland/deno", DENO_TAG, "deno-x86_64-unknown-linux-gnu.zip", "c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490", "deno"),
            _asset("ffmpeg", "ffmpeg", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffmpeg-linux-x64", "9eac5b2b5076db5ff853a6fa0dcd6b8de7d0cac8481eadda6c47cd935825f1ee"),
            _asset("ffprobe", "ffprobe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffprobe-linux-x64", "065d3c56926052a76e884c4e4b51b7d95248da9391ab7effdcca6b94ceab98cf"),
            _asset("aria2c", "aria2c", "abcfy2/aria2-static-build", ARIA_LINUX_TAG, "aria2-x86_64-linux-musl_static.zip", "e0a09b12ef67f35f8a8e4fdddbec851d235b7c31da549d0578bff459032b499a", "aria2c"),
            _gallery_linux_source_asset(),
        ]
    if target == "windows-x64":
        return [
            _asset("yt-dlp", "yt-dlp.exe", "yt-dlp/yt-dlp", YT_DLP_TAG, "yt-dlp.exe", "66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a"),
            _asset("deno", "deno.exe", "denoland/deno", DENO_TAG, "deno-x86_64-pc-windows-msvc.zip", "a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238", "deno.exe"),
            _asset("ffmpeg", "ffmpeg.exe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffmpeg-win-x64.exe", "4044b3924c977ad31229d504c5d5b8685f9553124fbaff6e9c99048b42830341"),
            _asset("ffprobe", "ffprobe.exe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffprobe-win-x64.exe", "fc37ca23d31ee08bb8f7e108edf3822f6ef3efc1a8d306bbe0b779190230710b"),
            _asset("aria2c", "aria2c.exe", "Kenshin9977/aria2", ARIA_TAG, "aria2c-windows-x86_64.exe", "bebce264e4192ecc01dcd519140730d0bd077ab9c20c9eed355b73c589b51696"),
            _asset("gallery-dl", "gallery-dl.exe", "gdl-org/builds", GALLERY_TAG, "gallery-dl_windows.exe", "c2ba089725a5bd9ea1a3b786505cdee7345d1fb43c173714b26befb156521c0e"),
        ]
    if target == "macos-arm64":
        return [
            _asset("yt-dlp", "yt-dlp", "yt-dlp/yt-dlp", YT_DLP_TAG, "yt-dlp_macos", "0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202"),
            _asset("deno", "deno", "denoland/deno", DENO_TAG, "deno-aarch64-apple-darwin.zip", "5cd46d6268f6f78f5d88bdc7159d20bd44cdaa4b3303474839f87ec6fe7ae25c", "deno"),
            _asset("ffmpeg", "ffmpeg", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffmpeg-osx-arm64", "e7b9fcd97f95f333512d6e8b8ac24d9dbc08f189f36047695499bd7b57214b22"),
            _asset("ffprobe", "ffprobe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffprobe-osx-arm64", "ded4c698b8ff38d0bc1fd30fcc5e768dc46f58bc15a8dfd61f98615ba49cde5c"),
            _asset("aria2c", "aria2c", "Kenshin9977/aria2", ARIA_TAG, "aria2c-macos-arm64", "baaaa52ee2539f664b595bd106311b792ded255fef5ab142cec1352db4c28d3a"),
            _asset("gallery-dl", "gallery-dl", "gdl-org/builds", GALLERY_TAG, "gallery-dl_macos", "8559943db808ac6fd84db2fd656845519930784f4e78bab9818d63a90695b23b"),
        ]
    if target == "macos-x64":
        return [
            _asset("yt-dlp", "yt-dlp", "yt-dlp/yt-dlp", YT_DLP_TAG, "yt-dlp_macos", "0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202"),
            _asset("deno", "deno", "denoland/deno", DENO_TAG, "deno-x86_64-apple-darwin.zip", "95daaff11c116a52ad54785e7914c8e9c9cdcaba793c5ed929c74ca2d8e6259a", "deno"),
            _asset("ffmpeg", "ffmpeg", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffmpeg-osx-x64", "62c87854d851f202fc4a29bdda0fe7b6ebcddd37b863482ce1bdc81151b03fe4"),
            _asset("ffprobe", "ffprobe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffprobe-osx-x64", "d530823f480a3c7eb6334f18a00197d1e9f1070e86172b9aa89c4bf4022bd879"),
            _asset("gallery-dl", "gallery-dl", "gdl-org/builds", GALLERY_TAG, "gallery-dl_macos", "8559943db808ac6fd84db2fd656845519930784f4e78bab9818d63a90695b23b"),
        ]
    raise ValueError(f"Unsupported Remote Import helper target: {target}")


def _sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _download(asset: Asset, destination: pathlib.Path) -> None:
    request = urllib.request.Request(asset.url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=120) as response, destination.open("wb") as output:
        shutil.copyfileobj(response, output)
    actual = _sha256(destination)
    if actual != asset.sha256:
        destination.unlink(missing_ok=True)
        raise RuntimeError(
            f"Checksum mismatch for {asset.source}: expected {asset.sha256}, got {actual}"
        )


def _extract_or_copy(asset: Asset, downloaded: pathlib.Path, destination: pathlib.Path) -> None:
    if asset.archive_member:
        with zipfile.ZipFile(downloaded) as archive:
            members = {name.replace("\\", "/"): name for name in archive.namelist()}
            wanted = asset.archive_member.replace("\\", "/")
            if wanted not in members or wanted.startswith("/") or ".." in pathlib.PurePosixPath(wanted).parts:
                raise RuntimeError(f"Verified archive does not contain safe member {asset.archive_member!r}")
            with archive.open(members[wanted]) as source, destination.open("wb") as output:
                shutil.copyfileobj(source, output)
    else:
        shutil.copy2(downloaded, destination)
    if asset.executable and os.name != "nt":
        destination.chmod(destination.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _safe_extract_tar(archive_path: pathlib.Path, destination: pathlib.Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        for member in members:
            parts = pathlib.PurePosixPath(member.name).parts
            if not parts or member.name.startswith("/") or ".." in parts:
                raise RuntimeError(f"Verified source archive contains unsafe path {member.name!r}")
            if member.issym() or member.islnk():
                raise RuntimeError(f"Verified source archive contains unsupported link {member.name!r}")
        archive.extractall(destination, members=members)


def _build_gallery_dl_from_source(
    asset: Asset, downloaded: pathlib.Path, destination: pathlib.Path
) -> None:
    if sys.platform != "linux":
        raise RuntimeError("Linux gallery-dl source build requested on a non-Linux host")

    build_root = downloaded.parent / "gallery-dl-build"
    source_root = build_root / "source"
    venv_root = build_root / "venv"
    _safe_extract_tar(downloaded, source_root)

    expected_source = source_root / f"gallery-dl-{GALLERY_LINUX_SOURCE_COMMIT}"
    if not (expected_source / "scripts" / "pyinstaller.py").is_file():
        raise RuntimeError("Pinned gallery-dl source archive is missing its PyInstaller build script")

    subprocess.run(
        [sys.executable, "-m", "venv", str(venv_root)],
        cwd=build_root,
        check=True,
        timeout=120,
    )
    venv_python = venv_root / "bin" / "python"
    requirements = build_root / "requirements.lock"
    requirements.write_text(GALLERY_LINUX_BUILD_REQUIREMENTS, encoding="utf-8")
    subprocess.run(
        [
            str(venv_python),
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--no-index",
            "--only-binary=:all:",
            "--require-hashes",
            "-r",
            str(requirements),
        ],
        cwd=build_root,
        check=True,
        timeout=300,
    )
    subprocess.run(
        [str(venv_python), "scripts/pyinstaller.py", "--label", "linux"],
        cwd=expected_source,
        check=True,
        timeout=300,
    )

    built = expected_source / "dist" / "gallery-dl_linux"
    if not built.is_file():
        raise RuntimeError("gallery-dl source build completed without producing gallery-dl_linux")
    shutil.copy2(built, destination)
    destination.chmod(destination.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _prepare_asset(asset: Asset, downloaded: pathlib.Path, destination: pathlib.Path) -> None:
    if asset.build_kind == "gallery-dl-pyinstaller":
        _build_gallery_dl_from_source(asset, downloaded, destination)
        return
    if asset.build_kind is not None:
        raise RuntimeError(f"Unsupported helper build kind {asset.build_kind!r}")
    _extract_or_copy(asset, downloaded, destination)


_PROBE_ARGS = {
    "yt-dlp": ("--version",),
    "deno": ("--version",),
    "ffmpeg": ("-version",),
    "ffprobe": ("-version",),
    "aria2c": ("--version",),
    "gallery-dl": ("--version",),
}


def _probe_prepared_asset(asset: Asset, destination: pathlib.Path) -> None:
    """Fail closed if a prepared helper cannot launch on its target runner."""
    if not asset.executable:
        return
    args = _PROBE_ARGS.get(asset.name)
    if args is None:
        raise RuntimeError(f"No runtime probe is defined for executable helper {asset.name!r}")
    try:
        result = subprocess.run(
            [str(destination), *args],
            cwd=destination.parent,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        destination.unlink(missing_ok=True)
        raise RuntimeError(f"Prepared helper {asset.name} could not launch: {error}") from error
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().replace("\n", " ")[:500]
        destination.unlink(missing_ok=True)
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(
            f"Prepared helper {asset.name} failed its runtime probe with exit {result.returncode}{suffix}"
        )


def prepare(target: str) -> None:
    assets = assets_for_target(target)
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    known = {name for name in [
        "yt-dlp", "yt-dlp.exe", "deno", "deno.exe", "ffmpeg", "ffmpeg.exe",
        "ffprobe", "ffprobe.exe", "aria2c", "aria2c.exe", "gallery-dl", "gallery-dl.exe",
        "manifest.json",
    ]}
    for name in known:
        path = OUTPUT_DIR / name
        if path.is_file() or path.is_symlink():
            path.unlink()

    manifest = {"schema": 1, "target": target, "assets": []}
    with tempfile.TemporaryDirectory(prefix="terarelay-remote-import-") as temp_dir:
        temp = pathlib.Path(temp_dir)
        for index, asset in enumerate(assets):
            downloaded = temp / f"{index}.download"
            print(f"Downloading {asset.name}: {asset.source}")
            _download(asset, downloaded)
            destination = OUTPUT_DIR / asset.destination
            _prepare_asset(asset, downloaded, destination)
            _probe_prepared_asset(asset, destination)
            manifest["assets"].append({
                "name": asset.name,
                "destination": asset.destination,
                "sha256_source": asset.sha256,
                "source": asset.source,
            })
            print(f"Prepared {destination.relative_to(ROOT)}")
    (OUTPUT_DIR / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def verify_downloads(target: str) -> None:
    assets = assets_for_target(target)
    with tempfile.TemporaryDirectory(prefix="terarelay-remote-import-verify-") as temp_dir:
        temp = pathlib.Path(temp_dir)
        for index, asset in enumerate(assets):
            downloaded = temp / f"{index}.download"
            _download(asset, downloaded)
            if asset.archive_member:
                with zipfile.ZipFile(downloaded) as archive:
                    names = {name.replace("\\", "/") for name in archive.namelist()}
                    wanted = asset.archive_member.replace("\\", "/")
                    if wanted not in names:
                        raise RuntimeError(f"Verified archive does not contain {asset.archive_member!r}")
            print(f"Verified {asset.name}: {asset.source}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=["linux-x64", "windows-x64", "macos-arm64", "macos-x64"])
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verify-downloads", action="store_true")
    args = parser.parse_args()
    assets = assets_for_target(args.target)
    if args.dry_run:
        for asset in assets:
            print(f"{asset.name:12} -> {asset.destination:16} {asset.source} sha256={asset.sha256}")
        if args.target == "macos-x64":
            print("aria2c      -> built-in downloader fallback (no pinned trusted macOS x64 sidecar)")
        return 0
    if args.verify_downloads:
        verify_downloads(args.target)
        return 0
    prepare(args.target)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
