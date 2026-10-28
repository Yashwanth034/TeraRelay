<p align="center">
  <img src="app/public/logo.svg" width="112" alt="TeraRelay logo">
</p>

<h1 align="center">TeraRelay</h1>

<p align="center">
  <strong>Store files far larger than Telegram's single-file limit — TeraRelay chunks them automatically and gives you the original file back as one download.</strong>
</p>

<p align="center">
  <a href="https://github.com/Yashwanth034/TeraRelay/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/Yashwanth034/TeraRelay?display_name=tag"></a>
  <a href="https://github.com/Yashwanth034/TeraRelay/actions/workflows/ci.yml"><img alt="Cross-platform CI" src="https://github.com/Yashwanth034/TeraRelay/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/Yashwanth034/TeraRelay/releases/latest"><img alt="Downloads" src="https://img.shields.io/github/downloads/Yashwanth034/TeraRelay/total"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-proprietary-red"></a>
</p>

<p align="center">
  <a href="https://github.com/Yashwanth034/TeraRelay/releases/latest"><strong>Download the latest TeraRelay release</strong></a>
  ·
  <a href="SECURITY.md">Security</a>
  ·
  <a href="PERFORMANCE.md">Performance</a>
</p>

---

## The core idea

Telegram has a per-document upload limit. TeraRelay works around that limit at the **logical-file layer**.

You choose **one large file** — for example 10 GB, 100 GB, or another file that is far beyond Telegram's single-document limit. TeraRelay automatically splits it into Telegram-sized parts, uploads those parts to your Telegram-backed storage, records the manifest, and keeps the chunks hidden from the normal UI.

Later, you click **Download once**. TeraRelay fetches the required parts, verifies them, puts them back in the correct order, and reconstructs the **original file with its original name and size**.

## Performance

- **Upload:** ~18–25 MB/s
- **Download:** ~10–15 MB/s

Actual speeds depend on your connection and Telegram's server load.

```text
Example: one 100 GB archive

100 GB original file
        │
        ▼
TeraRelay splits it automatically
        │
        ├── part 001
        ├── part 002
        ├── part 003
        ├── ...
        └── manifest + integrity metadata
                │
                ▼
        stored through Telegram
                │
                ▼
      shown as ONE logical file
                │
          one Download action
                │
                ▼
      original 100 GB file restored
```

So the user does **not** have to manually name, upload, track, download, reorder, or join dozens of chunk files.

### Why use TeraRelay instead of splitting files manually?

| Task | Manual Telegram workflow | TeraRelay |
| --- | --- | --- |
| Upload a huge file | Split it yourself and upload many parts | Select the original file once |
| Keep track of parts | Manage part names/order yourself | Manifest and ordering are automatic |
| What you see | Many separate Telegram documents | One logical file |
| Download later | Download every part and join them manually | Click Download once |
| Reconstruct original | Manual tooling/scripts | Automatic reconstruction |
| Integrity | Verify parts yourself | Size/checksum validation |
| Interrupted transfer | Track/retry parts yourself | Resume/recovery logic built in |

> **Large-file scope:** TeraRelay is designed for logical files much larger than Telegram's single-document limit, including tens or hundreds of gigabytes. Practical limits still depend on Telegram/account behavior, available storage, network reliability, runtime, and local disk space. Release validation has included multi-gigabyte files; 100 GB is an explanatory example, not a claim that every environment has been benchmarked at that size.

## What is TeraRelay?

TeraRelay is a cross-platform Tauri + Rust + React application that turns **your own Telegram account into a managed file-storage layer**.

Its main job is to hide Telegram's storage mechanics from you: normal documents stay normal documents, and very large files are represented as one logical file even when Telegram stores them internally as many documents.

There is no separate TeraRelay cloud-storage account or remote file-storage backend. Your Telegram session and TeraRelay application state stay local to the client, while file data is stored through Telegram.

> **Security note:** Telegram cloud chats are **not end-to-end encrypted**. TeraRelay does not change that security model. If a file requires end-to-end confidentiality, encrypt it before uploading. See [SECURITY.md](SECURITY.md).

## Highlights

- **Huge logical files** — select one large file; TeraRelay handles Telegram-sized chunking and reconstruction automatically
- **One-file experience** — internal Telegram parts stay hidden; the UI shows the original logical file
- **Automatic reconstruction** — one Download action restores the original file from its stored parts
- **Integrity validation** — manifests, ordered chunks, exact-size reconstruction, and checksum validation
- **Resumable transfers** — upload/download recovery for interrupted or stale local transfer state
- **Your Telegram storage** — files are stored through your own Telegram account rather than a separate TeraRelay storage backend
- **Personal Vault** — private storage separated from shared TeraRelay channels
- **Logical channels** — owner/member roles with invite, join, rejoin, leave, and revoke flows
- **Persistent Telegram sessions** — phone login, QR login, 2-step verification, and session restore
- **File management** — search, rename, move, delete, preview, and folder upload
- **Desktop local sharing** — local share links for supported desktop files
- **Proxy support and localization**
- **Linux acceleration** — optional TDLib/C++ fast-transfer path on Linux x86_64

## What's new in v0.2.1

- Keep saved Telegram authorization across restarts and temporary connection failures, with automatic reconnection and recoverable status.
- Improve Drive organization, backup/recovery, transfers, playback, navigation and accessibility.
- Upgrade vulnerable Hickory and ringbuf dependencies through narrow compatibility patches while preserving the Telegram session format, SOCKS proxies and torrent support.
- Extend cross-platform CI with locked proxy/buffer regressions and bounded, serial media fixtures.

See the [v0.2.1 changelog](CHANGELOG.md#021---2026-10-28). Published packages and signing details are listed on [GitHub Releases](https://github.com/Yashwanth034/TeraRelay/releases/latest).

## What's new in v0.2.0

Version 2 expands TeraRelay from file storage into version management, remote acquisition, and Drive workflows.

- **File Stacks / Versions:** group related files into a stack, add versions manually, and manage, rename, move, unstack, or delete them while preserving the individual Telegram-backed files.
- **Duplicate detection and media details:** identify byte-verified duplicates and load rich media metadata on demand.
- **Refreshed desktop and mobile interfaces:** updated layouts while retaining file management, transfers, sharing, and localization.
- **TeraRelay Boost:** choose a transfer engine explicitly, with saved engine preferences and durable queue recovery across restarts.
- **Remote Import:** acquire files from direct URLs, supported media pages, galleries, and torrents. Select quality or files, resume acquisition, import multiple links, and optionally group results into Version Stacks. Release builds prepare pinned, hash-verified helper tools.
- **TeraRelay Drive and Device Continuity:** organize a storage pool and reconcile Drive metadata across devices. Native Windows Explorer and macOS File Provider integration include portable-name checks, conflict protection, bounded staging, and reconnect recovery. macOS File Provider distribution requires Apple provisioning.
- **Desktop background mode:** closing the window keeps TeraRelay Drive connected. Reopen it from the tray menu or app launcher; the launcher restores the same running instance. Choose **Quit TeraRelay** in the sidebar or tray menu to stop the app and safely unmount Drive. Your saved login and transfer recovery data remain available.
- **Local storage:** file listings use metadata; browsing Drive does not download the whole library. Drive keeps a small read cache of about 32 MiB. Desktop idle cleanup releases TDLib file caches and retired transfer caches only after saved upload/download queues contain no unfinished work. Login databases, pending Drive writes, active imports and reinstall backups are preserved. Downloads, folder ZIPs, remote imports and transcoding still need local working space; the whole app is not limited to kilobytes.
- **Signed updates:** desktop updater packages and the mobile update manifest are signed. Android checks the downloaded APK hash again immediately before installation.
- **Release fixes:** corrected cross-platform Drive compilation, macOS 12.0 compatibility, and mobile update signature handling.

See the [v0.2.0 changelog](CHANGELOG.md#020---2026-10-11) for the detailed changes.

## Downloads

Use [GitHub Releases](https://github.com/Yashwanth034/TeraRelay/releases/latest) for the current published version, actual assets and signing details. The release workflow supports these targets:

| Platform | Packaging | Packages |
| --- | --- | --- |
| Linux x86_64 | Desktop release | DEB, RPM, AppImage |
| Windows x64 | Desktop release; Authenticode when configured | EXE, MSI |
| macOS Apple Silicon | Desktop release; Apple signing/notarization when configured | DMG, app archive |
| macOS Intel | Desktop release; Apple signing/notarization when configured | DMG, app archive |
| Android ARM64 | Production signing required | APK, AAB |
| iOS | Simulator CI; App Store distribution requires configured Apple credentials | See release notes |

**Download:** [GitHub Releases →](https://github.com/Yashwanth034/TeraRelay/releases/latest)

The Android **APK** is the directly installable package. The **AAB** is intended for Android distribution workflows.

Release/build validation confirms that the package paths compile and produce the expected artifacts. It is **not** a claim of exhaustive runtime/device testing across every OS, hardware model, network, or Telegram account type.

## How it works

```text
TeraRelay UI
    │
    ▼
Tauri / Rust core
    │
    ├── local session + TeraRelay metadata
    │
    ▼
Telegram API / MTProto
    │
    ├── Personal Vault
    └── TeraRelay logical channels
          │
          ├── normal Telegram documents
          └── large-file parts + manifest
                    │
                    ▼
             one logical file in TeraRelay
```

Internal Telegram document parts stay hidden from the normal file view. A multi-part file is presented, downloaded, and reconstructed as a single file with its original filename and total size.

## Large-file handling

TeraRelay supports logical files larger than Telegram's normal single-document limit by storing them as multiple Telegram documents plus TeraRelay metadata.

The client handles:

- split-part ordering
- logical filename and total size
- manifest metadata
- transfer resume state
- stale local-cache recovery
- exact reconstruction
- checksum verification

Users interact with **one file**, not with the internal parts.

## Transfer paths

| Platform | Transfer path |
| --- | --- |
| Linux x86_64 | TDLib/C++ acceleration where available, with the normal Telegram transfer path retained |
| Windows | Grammers / MTProto |
| macOS | Grammers / MTProto |
| Android | Grammers / MTProto |
| iOS | Grammers / MTProto |

Verified Linux release testing included multi-gigabyte logical files, upload throughput around **18–20 MB/s**, and native TDLib downloads around **16–17 MB/s** on the tested non-Premium account and network. These are measurements, **not guaranteed speeds**.

See [PERFORMANCE.md](PERFORMANCE.md) for the benchmark conditions and limitations.

## Quick start

1. Open the [latest release](https://github.com/Yashwanth034/TeraRelay/releases/latest).
2. Download the package for your platform.
3. Install or launch TeraRelay.
4. Sign in with your Telegram account.
5. Use Personal Vault or create/join a TeraRelay channel.
6. Upload and manage files normally; TeraRelay handles the Telegram storage details.

### v0.2.0 platform notes

- **Android:** ARM64 only.
- **iOS:** simulator build validation only; physical-device/App Store distribution requires Apple signing/provisioning.
- **Desktop signing:** Windows installers are unsigned for Authenticode. macOS packages are ad-hoc signed and not notarized; native File Provider distribution requires Apple provisioning. OS security prompts may appear.
- **Update integrity:** desktop updater payloads and the Android update manifest have verified signatures. The release includes `SHA256SUMS.txt` for download checksums.
- **Worldwide public sharing:** not enabled in v0.2.0. Desktop share links are local application features.

## Security model

TeraRelay handles authentication/session material locally. Never publish or commit Telegram sessions, API hashes, login codes, 2-step verification passwords, local databases, signing keys, provisioning profiles, or application caches.

The repository includes ignore rules and `scripts/check-source.sh` as release guardrails.

Current dependency advisories and their exact scope are documented in [SECURITY.md](SECURITY.md). Security issues involving sensitive information should use GitHub's private security-reporting flow rather than a public issue.

## Development

> Public source availability does **not** grant an open-source license for TeraRelay's original source code. See [License](#license).

### Prerequisites

Install the standard [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your operating system, plus Node.js and Rust.

```bash
git clone https://github.com/Yashwanth034/TeraRelay.git
cd TeraRelay/app
npm ci
npm run tauri dev
```

### Verification

Frontend:

```bash
cd app
npm ci
npm run build
npm audit --audit-level=high
node check-i18n.cjs
```

Rust/Tauri:

```bash
cd app/src-tauri
cargo fmt --all -- --check
cargo check --locked
cargo test --locked
```

Repository hygiene:

```bash
./scripts/check-source.sh
```

Cross-platform CI also validates Linux, Windows, macOS, Android ARM64, and the iOS simulator.

## Project status

The latest published stable version is listed on [GitHub Releases](https://github.com/Yashwanth034/TeraRelay/releases/latest). **v0.1.0** was the first public release.

All seven CI jobs and all required release jobs passed for v0.2.0. The iOS App Store Connect release job was skipped because production Apple credentials were unavailable; iOS simulator validation passed.

Current limitations:

- Telegram cloud storage is not end-to-end encrypted.
- Android 32-bit ABIs are not included.
- iOS physical-device/App Store distribution is not published.
- Worldwide public file sharing is not implemented.
- Cross-platform build/package validation is not the same as exhaustive device/runtime testing.

## Documentation

- [Project license](LICENSE)
- [Changelog](CHANGELOG.md)
- [Security model and advisories](SECURITY.md)
- [Support and reporting](.github/SUPPORT.md)
- [Transfer performance](PERFORMANCE.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)
- [Release downloads](https://github.com/Yashwanth034/TeraRelay/releases/latest)

## License

TeraRelay's original source code is **proprietary and All Rights Reserved**. The public repository is source-visible for inspection, evaluation, and security review; it is **not** an open-source license grant.

Official, unmodified TeraRelay binary releases may be downloaded, installed, and used for lawful personal or internal business use under the project terms. See [LICENSE](LICENSE) for the complete permissions and restrictions.

Third-party dependencies remain subject to their own licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
