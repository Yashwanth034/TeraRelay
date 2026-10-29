<p align="center">
  <img src="app/public/logo.svg" width="104" alt="TeraRelay logo">
</p>

<h1 align="center">TeraRelay</h1>

<p align="center">
  <strong>Your Telegram-backed file library, without the manual splitting and reassembly.</strong>
</p>

<p align="center">
  Store large files as one logical item, organize a personal library, import from supported links, and recover transfers across sessions.
</p>

<p align="center">
  <a href="https://github.com/Yashwanth034/TeraRelay/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/Yashwanth034/TeraRelay?display_name=tag"></a>
  <a href="https://github.com/Yashwanth034/TeraRelay/actions/workflows/ci.yml"><img alt="Cross-platform CI" src="https://github.com/Yashwanth034/TeraRelay/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: proprietary" src="https://img.shields.io/badge/license-proprietary-red"></a>
</p>

<p align="center">
  <a href="https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.1"><strong>Download v0.2.1</strong></a>
  ·
  <a href="#quick-start">Quick start</a>
  ·
  <a href="#features">Features</a>
  ·
  <a href="CHANGELOG.md">Changelog</a>
  ·
  <a href="SECURITY.md">Security</a>
</p>

---

## What TeraRelay does

Telegram limits the size of an individual uploaded document. TeraRelay handles a bigger **logical file** by splitting it into permitted parts behind the scenes, storing the parts with a manifest, and reconstructing the original file when you download it.

**You select one file. You see one file. You download one file.** You don't have to number, track, or rejoin the parts yourself.

```text
Your file                    Your Telegram-backed storage
   │                                   │
   └─ Upload ─► chunk + manifest ─────►│
                                       │
   ◄─ Download ◄─ verify + reassemble ◄┘
   │
Original filename, size, and bytes restored
```

TeraRelay uses your own Telegram account. File data is stored through Telegram; the client manages its session, metadata, and transfer state locally. There is no separate TeraRelay cloud-storage account.

**Important:** Telegram cloud chats are **not end-to-end encrypted**. Encrypt sensitive files yourself before uploading. TeraRelay does not change Telegram's security model. See [Security](SECURITY.md).

## Download v0.2.1

The current published release is **v0.2.1**. Choose the package that matches your device:

| Platform | Available release files | Notes |
| --- | --- | --- |
| **Linux x86_64** | `.deb`, `.rpm`, `.AppImage` | Choose the format for your distribution |
| **Windows x64** | `.exe`, `.msi` | Installers are not Authenticode-signed |
| **macOS Apple Silicon** | `.dmg`, `.app.tar.gz` | Not Apple-notarized |
| **macOS Intel** | `.dmg`, `.app.tar.gz` | Not Apple-notarized |
| **Android ARM64** | Signed `.apk`, `.aab` | Install the APK directly; AAB is for distribution workflows |
| **iOS** | No public installable package | Simulator CI passed; App Store/device distribution requires Apple credentials |

**[Get TeraRelay v0.2.1 from GitHub Releases](https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.1)**

The release includes `SHA256SUMS.txt` to verify downloaded files, signed desktop updater payloads, and a signed Android update manifest. **An updater signature is not the same as operating-system code signing.** Windows/macOS security prompts may appear.

Release builds and automated tests have passed; this does **not** mean the app has been exhaustively tested on every physical device or Telegram account.

## Quick start

1. Download the [appropriate v0.2.1 package](https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.1) and install or launch it.
2. Sign in using your Telegram account. Phone, QR, and two-step verification flows are supported.
3. Open **Personal Vault**, or create/join a TeraRelay channel.
4. Upload a document or folder. Large-file parts are handled automatically.
5. Browse, organize, preview, or download items. A multipart download is reconstructed and verified for you.

For other workflows, use **Remote Import** for supported URLs and **TeraRelay Drive** for desktop file-management integration.

## Features

| Area | What you can do |
| --- | --- |
| **Large files** | Automatic Telegram-sized parts, manifest ordering, exact-size reconstruction, and checksum validation |
| **Reliable transfers** | Persistent upload/download queues, resumption after interruptions, recovery from stale transfer state, and selectable transfer engines |
| **Library & sharing** | Personal Vault; logical channels with owner/member permissions and invite/join/leave flows; search, folders, rename, move, delete, and local desktop share links |
| **Versions & media** | File Stacks / Versions, byte-verified duplicate detection, metadata, document/media previews, and playback with seeking support |
| **Remote Import** | Supported direct URLs, media pages, galleries, and torrents; quality or file selection where available, multi-link queues, and resumable acquisition |
| **TeraRelay Drive** | Desktop file-manager integration, storage organization, metadata-based browsing, and recovery/backup tools |
| **Device Continuity** | Reconcile Drive metadata across devices with conflict protection; it is not a replacement for separate device authentication |
| **Desktop experience** | Background/tray mode, saved login recovery, themes and localization, plus signed in-app update payloads |

### Drive tools in the current app

Beyond basic folders, the desktop app includes tools for organizing and checking Drive data: folder copy/mirror operations, collections and smart views, duplicate exploration, backup timelines, integrity checks, pending-write recovery, and diagnostic views. These are part of the current product—not a separate download.

Native integration differs by OS. **Linux** supports an optional TDLib/C++ fast-transfer engine; **Windows** includes Explorer integration; **macOS** File Provider distribution depends on Apple provisioning. See [platform and release notes](https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.1) for distribution limits.

## What's improved in v0.2.1

- **Login and reconnection:** restore saved Telegram authorization after restart or temporary network loss, with recoverable connection status instead of unnecessarily returning to sign-in.
- **Playback and seeking:** improve WebKit media recovery and fragmented-MP4 handling. A completed progressive MP4 can be finalized as a seekable fast-start MP4 without re-encoding.
- **Drive and transfers:** improve local-first organization, backups, recovery, file handling, and background startup/shutdown behavior.
- **Dependency security and release checks:** update the affected Hickory/ringbuf dependency paths while retaining SOCKS proxy and torrent support; expand regression coverage and cross-platform validation.

**Older release notes belong in the [full changelog](CHANGELOG.md)** rather than repeating entire past versions in this README.

## Practical limits and security

- **Large isn't unlimited.** Tens or hundreds of gigabytes are logical-file use cases, but practical limits depend on Telegram/account behavior, network reliability, disk space, and the client environment. Multi-gigabyte files have been validated; a 100 GB example is not a universal benchmark or guarantee.
- **Performance varies.** Previous Linux test measurements are documented in [PERFORMANCE.md](PERFORMANCE.md), including the tested environment. They are not promised upload or download speeds.
- **Local disk is still used.** File downloads, folder archives, imports, and media processing may need temporary working space even when browsing uses metadata and a small read cache.
- **Sharing scope matters.** Desktop local share links are not a worldwide public file-hosting service. Worldwide public sharing is not implemented.
- **Protect account data.** Do not upload Telegram sessions, two-step verification secrets, signing keys, or local application databases to a public repository. See [SECURITY.md](SECURITY.md).

## Development

TeraRelay is built with **Tauri 2, Rust, React, and TypeScript**. The source is publicly viewable but [proprietary](LICENSE); public access is **not** an open-source license grant.

Install [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/), Node.js, and Rust for your platform.

```bash
git clone https://github.com/Yashwanth034/TeraRelay.git
cd TeraRelay/app
npm ci
npm run tauri dev
```

Check the frontend:

```bash
cd app                       # from repository root
npm ci
npm run build
npm audit --audit-level=high
node check-i18n.cjs
```

Check Rust and source hygiene:

```bash
cd app/src-tauri             # from repository root
cargo fmt --all -- --check
cargo check --locked
cargo test --locked

cd ../..
bash scripts/check-source.sh
```

The v0.2.1 release passed its cross-platform CI and required production release workflow. Automated validation covers Linux, Windows, macOS, Android ARM64, and the iOS simulator; it is not a substitute for testing every device. [See CI](https://github.com/Yashwanth034/TeraRelay/actions/workflows/ci.yml).

## Documentation and support

| Resource | Purpose |
| --- | --- |
| [Changelog](CHANGELOG.md) | Full release history, including earlier versions |
| [Security](SECURITY.md) | Data model, security guidance, and dependency advisory scope |
| [Performance](PERFORMANCE.md) | Transfer measurements and their limitations |
| [Support](.github/SUPPORT.md) | Help and reporting routes |
| [Third-party notices](THIRD_PARTY_NOTICES.md) | Dependency license information |
| [License](LICENSE) | Rights and restrictions for the source and official builds |

TeraRelay's original source is **All Rights Reserved**. Official, unmodified binaries may be downloaded and used under the [project license](LICENSE); redistribution and derivative-use restrictions still apply.
