# Changelog

All notable public changes to TeraRelay are documented here.

## [Unreleased]

## [0.2.3] - 2026-10-30

- Fix uploads to newly created Telegram channels when their per-channel message IDs overlap an older channel. Migrate the invalid global chunk-ID index without deleting existing Drive records or changing channel identities.
- Make local manifest indexing atomic: a failed reindex leaves the previous verified file/chunk mappings intact so upload retries can recover already-published Telegram parts.
- Keep existing TeraRelay channels discoverable after renaming their Telegram titles externally, using the stable backing ID instead of requiring the original `[TR]` title marker.
- Remove completed-download SQLite resume and lease sidecars while preserving stages and journals for interrupted downloads; never discard partially downloaded bytes just to tidy a folder.
- Recover from transient local HTTP 5xx video stream responses with FFmpeg 6.1-compatible bounded retry behavior. Show a concise actionable playback error and manual Retry instead of exposing verbose decoder logs.
- Isolate Drive test query counters per test thread, preventing nondeterministic failures during parallel test runs.

The patch does not auto-delete stale Drive folders or recreate Telegram content from a deleted channel. Users should verify orphaned channel data before removing any folder.

## [0.2.2] - 2026-10-29

- Replace the default blue/glass desktop treatment with neutral dark and light surfaces, clearer selected states, and readable primary controls across custom themes.
- Improve large-library scrolling and grid/list switching by reusing locale-aware filename sorting, enabling only the visible virtualizer, and attaching viewport measurement after asynchronous loading.
- Keep native scrolling, add lightweight sidebar overflow fades, respect reduced motion/performance mode, and prevent toolbar overlap in narrow desktop windows.
- Show the normal opening screen while restoring a saved Telegram session. Keep retry and optional phone sign-in available for real connection/credential errors and prolonged connections without clearing the saved login.

## [0.2.1] - 2026-10-28

- Preserve saved Telegram account sessions across normal launches, Linux background startup, and transient connection failures. Show recoverable connection status rather than incorrectly requesting phone sign-in; never create an empty session during automatic restoration.
- Improve desktop navigation, virtualized channel scrolling, transfer recovery, playback, accessibility, and local-first Drive operations.
- Add Drive organization, copy/mirror, backup, recovery, search, auditing, and diagnostic tools with focused unit and UI regression coverage.
- Strengthen Linux background startup and safe shutdown, and extend Windows/macOS native Drive platform validation.
- Security: patch the session-compatible Grammers transport to use Hickory 0.26.3 and uTP to use ringbuf 0.5.3, retaining SOCKS proxy and torrent support. Keep the strict, non-waived RustSec/npm release gates and add locked SOCKS and buffer regressions to CI.
- Run native-media unit fixtures serially, ensure FFmpeg/ffprobe are installed on test runners, and bound invalid-input HTTP cleanup so a failed startup cannot cause an indefinite test hang.
- Fix Windows journal, completed-download, and pending Drive staging flushes to use writable handles without truncating saved bytes; explicitly enable NTFS sparse staging for large logical downloads.
- Keep iOS SOCKS proxy hostname resolution on the native OS resolver, avoiding macOS-only SystemConfiguration APIs.
- Make the native stream-port collision fixture distinguish a live listener from TCP TIME_WAIT left by a prior QA run.
- Preserve progressive fragmented-MP4 playback during acquisition, then finalize completed media to seekable fast-start MP4 without re-encoding; harden WebKit playback recovery and bounded Windows FFmpeg cleanup.

## [0.2.0] - 2026-10-11

### Files, versions, and media

- Added explicit File Stacks / Versions with manual add-version, manage, rename, move, unstack, and delete behavior while preserving individual Telegram-backed files.
- Added byte-verified duplicate detection and on-demand rich media metadata without bloating the main file list.
- Added the premium desktop/mobile UI refresh while preserving existing file, transfer, sharing, and localization workflows.

### Transfers and Remote Import

- Added TeraRelay Boost as an explicit transfer-engine choice alongside the existing TDLib C++ path, with durable queue recovery and engine persistence across restarts.
- Added Remote Import for direct URLs, supported media pages, galleries, and torrents, including quality/file selection, resumable acquisition, multi-link batches, and optional manual Version Stack grouping.
- Added pinned, hash-verified helper preparation for yt-dlp, Deno, FFmpeg/ffprobe, aria2, and gallery-dl so release builds do not depend on user-installed helper tools.

### Drive and device continuity

- Added the TeraRelay Drive storage pool and cross-device Device Continuity reconciliation using the existing encrypted Drive metadata model.
- Added native Windows Explorer and macOS File Provider integration with portable-name validation, crash/reconnect recovery, conflict fail-closed behavior, bounded staging, and provider-version checks.
- Added Unicode/case portable namespace collision protection for Drive manifests and native filesystem projections.

### Updates and release security

- Added the signed in-app updater for desktop plus a signed mobile update manifest and Android APK hash re-verification immediately before installation.
- Added release-time Android package/version/signature verification, Windows Authenticode signing gates, macOS Developer ID/notarization/File Provider provisioning gates, and a real App Store Connect/TestFlight pipeline for iOS when production Apple credentials are configured.
- Production signing steps run only when their required credentials are configured. The published v0.2.0 Windows installers are unsigned for Authenticode, and macOS packages are ad-hoc signed without notarization. Native macOS File Provider distribution requires Apple provisioning. iOS simulator validation passed; the App Store Connect release was skipped and no signed device IPA is included.
- Fixed cross-platform Drive compilation, macOS 12.0 File Provider compatibility, and mobile updater public-key/signature decoding. Desktop updater payloads and the mobile update manifest were cryptographically verified; the release includes SHA256SUMS.txt.

## [0.1.1] - 2026-10-06

### Transfers and playback

- Improved native upload/download progress and speed reporting.
- Persist transfer queues across restarts and reuse confirmed upload parts after checking the source file.
- Validate multipart ordering, sizes, checksums, and manifest capacity, including part counts beyond 999.
- Preserve existing destination files until downloads are complete and verified.
- Support ZIP64 folder archives with files over 4 GiB.
- Show full multipart movie duration and seek across parts while preserving audio selection and pause state.

### Release validation and security

- Patched the source-map-js build dependency to 1.2.2; generated frontend output is unchanged.
- Install FFmpeg for CI media tests, limit Rust test duration, and support source-only checks without rebuilding installers.
- Published Linux, Windows, macOS Apple Silicon/Intel, and signed Android ARM64 packages with SHA-256 checksums.
- iOS simulator validation passed; no signed physical-device IPA is included.
- Large-file recovery and reconstruction checks passed; no 1 TB/5 TB live Telegram transfer or guaranteed throughput is claimed.

## [0.1.0] - 2026-10-03

First public release.

### Storage and account model

- Telegram phone and QR login with 2-step verification support
- Persistent local Telegram sessions
- Personal Vault support
- Logical TeraRelay channels with owner/member roles
- Invite, join, rejoin, leave, and revoke flows
- Existing files remain visible to members after joining a logical channel
- Internal Telegram storage details remain hidden from the normal file view

### Files and transfers

- File and folder upload
- Large logical files stored as multiple Telegram documents plus TeraRelay metadata
- Manifest-based reconstruction with exact size and checksum validation
- Upload/download resume support
- Recovery from stale TDLib local-cache state
- Search, rename, move, delete, and preview
- Local desktop share links for supported files
- Proxy support
- Localization and theme support

### Performance

- Linux x86_64 TDLib/C++ acceleration path
- Verified multi-gigabyte logical-file reconstruction
- Observed Linux release-validation uploads around 18–20 MB/s
- Observed native TDLib downloads around 16–17 MB/s on the tested non-Premium account/network

These measurements are test results, not guaranteed speeds. See [PERFORMANCE.md](PERFORMANCE.md).

### Platform validation

- Linux x86_64: DEB, RPM, and AppImage
- Windows x64: EXE and MSI
- macOS Apple Silicon: DMG and app archive
- macOS Intel: DMG and app archive
- Android ARM64: signed APK and AAB, with signature verification in release CI
- iOS: simulator build validation only

### Security, licensing, and release hygiene

- Proprietary TeraRelay license with explicit permission to use official unmodified binaries
- Separate third-party license/notices inventory
- Public-source hygiene checks
- Session, credential, signing-key, local-data, cache, and QA-output exclusions
- npm high-severity audit check
- Rust dependency/advisory documentation
- GitHub private vulnerability reporting
- Cross-platform CI and release workflows

### Known v0.1.0 limitations

- Telegram cloud chats are not end-to-end encrypted
- Android 32-bit ABIs are not distributed
- iOS physical-device/App Store distribution is not included
- Worldwide public file sharing is not implemented
- Cross-platform build/package validation is not exhaustive runtime/device testing

[0.2.1]: https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.1
[0.2.0]: https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.2.0
[0.1.1]: https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.1.1
[0.1.0]: https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.1.0
