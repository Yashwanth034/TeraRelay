#!/usr/bin/env python3
"""Fail closed if narrow RustSec reachability assumptions change.

This is not a vulnerability waiver or replacement for cargo audit. It protects
the 2026-10-18 source review while compatible upstream fixes are outstanding.
Uses the fully resolved, target-inclusive Cargo feature graph and actual
dependency source instead of inferring features from Cargo.toml.
"""
import copy
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "app/src-tauri/Cargo.toml"


def version_is_below(actual: str, patched: tuple[int, int, int]) -> bool:
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)", actual)
    if match is None:
        raise AssertionError(f"Unrecognized Cargo version: {actual}")
    # Treat any prerelease at the patched triplet as potentially vulnerable,
    # not automatically equivalent to the final patched release.
    numbers = tuple(int(x) for x in match.groups())
    return numbers < patched or (numbers == patched and "-" in actual)


def verify_patched_webpki_versions(metadata: dict) -> list[str]:
    """Prevent a regression to the three fixed October 2026 TLS advisories.

    The legacy webpki-roots 0.26.x line is separately unaffected by the
    webpki-roots 1.x advisory. These floors are not a general RustSec audit.
    """
    floors = {
        "rustls-webpki": ((0, 103, 16), "RUSTSEC-2026-0382"),
        "webpki-root-certs": ((1, 0, 10), "RUSTSEC-2026-0383"),
        "webpki-roots": ((1, 0, 10), "RUSTSEC-2026-0381"),
    }
    checked = []
    for package in metadata["packages"]:
        name = package["name"]
        if name not in floors:
            continue
        version = package["version"]
        if name == "webpki-roots" and version.startswith("0.26."):
            continue  # This independent legacy line is unaffected.
        floor, advisory = floors[name]
        if version_is_below(version, floor):
            raise AssertionError(
                f"BLOCKED: previously patched {advisory} regressed: "
                f"{name} {version} below {'.'.join(map(str, floor))}"
            )
        checked.append(f"CHECKED patched {name} {version} ({advisory})")
    return checked


def verify_metadata(metadata: dict) -> list[str]:
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    matches = lambda name: [(id_, pkg) for id_, pkg in packages.items() if pkg["name"] == name]
    findings = []

    for package_id, package in matches("hickory-proto"):
        if not version_is_below(package["version"], (0, 26, 1)):
            continue
        features = nodes[package_id]["features"]
        if any("dnssec" in feature.lower() for feature in features):
            raise AssertionError(
                "BLOCKED: vulnerable hickory-proto has DNSSEC enabled; "
                "the prior NSEC3 reachability conclusion no longer applies"
            )
        findings.append(
            f"CHECKED hickory-proto {package['version']}: DNSSEC inactive; "
            "encoding/CPU advisory still unresolved"
        )

    for package_id, package in matches("ringbuf"):
        if not version_is_below(package["version"], (0, 5, 2)):
            continue
        # Inspect EVERY resolved direct user, not only a known root dependency.
        consumers = [
            packages[node_id] for node_id, node in nodes.items()
            if any(dep["pkg"] == package_id for dep in node["deps"])
        ]
        if len(consumers) != 1 or consumers[0]["name"] != "librqbit-utp":
            raise AssertionError(
                "BLOCKED: vulnerable ringbuf has new/unknown consumers: "
                + ", ".join(sorted(p["name"] for p in consumers))
            )
        source = Path(consumers[0]["manifest_path"]).parent / "src/stream_tx.rs"
        if not source.is_file():
            raise AssertionError(f"BLOCKED: uTP ring-buffer source unavailable: {source}")
        code = source.read_text(encoding="utf-8")
        if not re.search(r"type\s+RingBuf\s*=\s*SharedRb\s*<\s*Heap\s*<\s*u8\s*>\s*>\s*;", code):
            raise AssertionError(
                "BLOCKED: uTP ring buffer is no longer explicitly Heap<u8>; "
                "redo panic-in-Drop advisory analysis"
            )
        if not re.search(r"self\.consumer\.lock\(\)\.skip\(count\)", code):
            raise AssertionError(
                "BLOCKED: uTP ring-buffer consumption changed; redo reachability analysis"
            )
        # All references to the ringbuf crate currently live in these two
        # sources; a new user must be reviewed, not silently treated as bytes.
        refs = []
        for file in (source.parent).rglob("*.rs"):
            if re.search(r"\bringbuf\b", file.read_text(encoding="utf-8")):
                refs.append(file.name)
        if sorted(refs) != ["stream_dispatch.rs", "stream_tx.rs"]:
            raise AssertionError(f"BLOCKED: ringbuf usage locations changed: {sorted(refs)}")
        findings.append(
            f"CHECKED ringbuf {package['version']}: only librqbit-utp "
            "SharedRb<Heap<u8>> consumer observed; advisory still unresolved"
        )
    return findings


def main() -> None:
    # CI runners may have Linux build dependencies cached while still missing
    # target-only package sources for Windows/macOS/iOS. Keep the lockfile
    # immutable, but allow Cargo to fetch those exact locked packages so this
    # target-inclusive security check runs on a fresh runner.
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked",
         "--manifest-path", str(MANIFEST)],
        text=True, capture_output=True, check=True, timeout=120,
    )
    metadata = json.loads(result.stdout)
    findings = verify_metadata(metadata)
    for finding in findings:
        print(finding)

    for finding in verify_patched_webpki_versions(metadata):
        print(finding)
    # Independently test every fixed package's exact previous vulnerable version
    # and a second vulnerable copy. No lockfile or dependency bytes are changed.
    regressions = {
        "rustls-webpki": "0.103.15",
        "webpki-root-certs": "1.0.9",
        "webpki-roots": "1.0.7",
    }
    for name, old_version in regressions.items():
        current = next(
            (p for p in metadata["packages"]
             if p["name"] == name
             and (name != "webpki-roots" or p["version"].startswith("1."))),
            None,
        )
        if current is None:
            print(f"SKIP: {name} no longer present in the affected version line")
            continue
        for duplicate in (False, True):
            changed = copy.deepcopy(metadata)
            if duplicate:
                injected = copy.deepcopy(current)
                injected["id"] += "#synthetic-regression"
                injected["version"] = old_version
                changed["packages"].append(injected)
            else:
                next(p for p in changed["packages"] if p["id"] == current["id"])[
                    "version"
                ] = old_version
            try:
                verify_patched_webpki_versions(changed)
            except AssertionError as error:
                assert "previously patched" in str(error), error
            else:
                raise AssertionError(
                    f"Negative control missed {name} {old_version}"
                    + (" duplicate" if duplicate else "")
                )
        print(f"PASS negative controls: {name} old and duplicate versions blocked")

    # Negative controls prove this safeguard fails closed if a vulnerable
    # package's threat prerequisites change, without changing the real graph.
    vulnerable_hickory = [
        p for p in metadata["packages"]
        if p["name"] == "hickory-proto"
        and version_is_below(p["version"], (0, 26, 1))
    ]
    for package in vulnerable_hickory:
        changed = copy.deepcopy(metadata)
        node = next(n for n in changed["resolve"]["nodes"] if n["id"] == package["id"])
        node["features"].append("dnssec-ring")
        try:
            verify_metadata(changed)
        except AssertionError as error:
            assert "DNSSEC enabled" in str(error), error
        else:
            raise AssertionError(
                f"Negative control: vulnerable Hickory {package['id']} DNSSEC went undetected"
            )
        print(f"PASS negative control: Hickory {package['version']} DNSSEC blocks the guard")

    vulnerable_ringbuf = [
        p for p in metadata["packages"]
        if p["name"] == "ringbuf"
        and version_is_below(p["version"], (0, 5, 2))
    ]
    for ring in vulnerable_ringbuf:
        changed = copy.deepcopy(metadata)
        root_id = changed["resolve"]["root"]
        node = next(n for n in changed["resolve"]["nodes"] if n["id"] == root_id)
        node["deps"].append({"name": "ringbuf", "pkg": ring["id"], "dep_kinds": []})
        try:
            verify_metadata(changed)
        except AssertionError as error:
            assert "new/unknown consumers" in str(error), error
        else:
            raise AssertionError(
                f"Negative control: vulnerable ringbuf {ring['id']} consumer went undetected"
            )
        print(f"PASS negative control: ringbuf {ring['version']} unknown consumer is blocked")

    # Regression against silently choosing only the final package of a given
    # name in a graph containing multiple versions. All synthetic IDs exist
    # ONLY inside copied metadata, not Cargo.toml, Cargo.lock, or disk.
    if vulnerable_hickory:
        changed = copy.deepcopy(metadata)
        original = vulnerable_hickory[0]
        duplicate_id = original["id"] + "#synthetic-second-version"
        duplicate_package = copy.deepcopy(original)
        duplicate_package["id"] = duplicate_id
        changed["packages"].append(duplicate_package)
        duplicate_node = copy.deepcopy(next(
            n for n in changed["resolve"]["nodes"] if n["id"] == original["id"]
        ))
        duplicate_node["id"] = duplicate_id
        duplicate_node["features"].append("dnssec-ring")
        changed["resolve"]["nodes"].append(duplicate_node)
        try:
            verify_metadata(changed)
        except AssertionError as error:
            assert "DNSSEC enabled" in str(error), error
        else:
            raise AssertionError(
                "Negative control: second vulnerable Hickory version escaped detection"
            )
        print("PASS negative control: a second vulnerable Hickory version is checked")

    if vulnerable_ringbuf:
        changed = copy.deepcopy(metadata)
        original = vulnerable_ringbuf[0]
        duplicate_id = original["id"] + "#synthetic-second-version"
        duplicate_package = copy.deepcopy(original)
        duplicate_package["id"] = duplicate_id
        changed["packages"].append(duplicate_package)
        duplicate_node = copy.deepcopy(next(
            n for n in changed["resolve"]["nodes"] if n["id"] == original["id"]
        ))
        duplicate_node["id"] = duplicate_id
        changed["resolve"]["nodes"].append(duplicate_node)
        root_id = changed["resolve"]["root"]
        root = next(n for n in changed["resolve"]["nodes"] if n["id"] == root_id)
        root["deps"].append({
            "name": "ringbuf-second-version", "pkg": duplicate_id, "dep_kinds": []
        })
        try:
            verify_metadata(changed)
        except AssertionError as error:
            assert "new/unknown consumers" in str(error), error
        else:
            raise AssertionError(
                "Negative control: second vulnerable ringbuf version escaped detection"
            )
        print("PASS negative control: a second vulnerable ringbuf version is checked")

    # Exercise the future fixed-version path without touching Cargo.lock,
    # downloading packages or claiming that upstream migration is complete.
    patched_simulation = copy.deepcopy(metadata)
    for package in patched_simulation["packages"]:
        if package["name"] == "hickory-proto":
            package["version"] = "0.26.1"
        if package["name"] == "ringbuf":
            package["version"] = "0.5.2"
    assert not verify_metadata(patched_simulation), (
        "Positive control: simulated fixed dependencies must no longer need "
        "these *narrow* reachability guards"
    )
    print("PASS simulated patched-version control (metadata only; not a dependency fix)")
    print("PASS narrow RustSec reachability guards only; cargo audit must still run/fail on advisories")


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"FAIL {error}", file=sys.stderr)
        sys.exit(1)
