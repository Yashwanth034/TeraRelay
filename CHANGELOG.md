# Changelog

All notable public changes to TeraRelay are documented here.

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

[0.1.0]: https://github.com/Yashwanth034/TeraRelay/releases/tag/v0.1.0
