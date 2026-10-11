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


@dataclasses.dataclass(frozen=True)
class Asset:
    name: str
    destination: str
    url: str
    sha256: str
    archive_member: str | None = None
    executable: bool = True
    source: str = ""


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


def assets_for_target(target: str) -> list[Asset]:
    if target == "linux-x64":
        return [
            _asset("yt-dlp", "yt-dlp", "yt-dlp/yt-dlp", YT_DLP_TAG, "yt-dlp_linux", "58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a"),
            _asset("deno", "deno", "denoland/deno", DENO_TAG, "deno-x86_64-unknown-linux-gnu.zip", "c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490", "deno"),
            _asset("ffmpeg", "ffmpeg", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffmpeg-linux-x64", "9eac5b2b5076db5ff853a6fa0dcd6b8de7d0cac8481eadda6c47cd935825f1ee"),
            _asset("ffprobe", "ffprobe", "shaka-project/static-ffmpeg-binaries", FFMPEG_TAG, "ffprobe-linux-x64", "065d3c56926052a76e884c4e4b51b7d95248da9391ab7effdcca6b94ceab98cf"),
            _asset("aria2c", "aria2c", "abcfy2/aria2-static-build", ARIA_LINUX_TAG, "aria2-x86_64-linux-musl_static.zip", "e0a09b12ef67f35f8a8e4fdddbec851d235b7c31da549d0578bff459032b499a", "aria2c"),
            _asset("gallery-dl", "gallery-dl", "gdl-org/builds", GALLERY_TAG, "gallery-dl_linux", "69901ef68ace81fb01377ac6e7ebca4894e66c74be8c243b8f414691d179062a"),
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
            _extract_or_copy(asset, downloaded, destination)
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
