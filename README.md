# TeraRelay

TeraRelay is a private file-storage client built with Tauri, Rust, and React and backed by the user's own Telegram account.

## Current release line

Version 0.1.0 provides:

- Telegram phone/QR login and 2-step verification
- persistent Telegram sessions
- Personal Vault support
- logical TeraRelay channels with owner/member roles
- invite, join, leave, and revoke flows
- file and folder upload
- large-file split storage with one logical file in the UI
- resumable/reconstructed downloads with integrity validation
- search, rename, move, delete, preview, themes, proxy support, and local share links
- localization
- Linux packaging as .deb and AppImage

TeraRelay v0.1.0 has successful CI/build-package validation on Linux x86_64, Windows x64, macOS Apple Silicon and Intel, Android ARM64, and the iOS simulator. This validates the build/package paths and is not equivalent to full runtime/device testing on every platform. Linux x86_64 includes the additional TDLib/C++ acceleration path; Windows, macOS, Android, and iOS use the MTProto fallback. The iOS target is simulator-validated only; no signed device IPA is published in v0.1.0.

## Development

Requirements vary by operating system. Install the Tauri prerequisites for your platform, then:

```bash
git clone <repository-url>
cd TeraRelay/app
npm ci
npm run tauri dev
```

Do not copy Telegram sessions, API credentials, signing keys, or local application data into the repository.

## Verification

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

## Large files

Files above Telegram's normal per-document limit are stored internally as multiple Telegram documents. TeraRelay keeps this implementation detail hidden: users see and download one logical file with its original filename and total size.

## Security

See [SECURITY.md](SECURITY.md).

## Performance

See [PERFORMANCE.md](PERFORMANCE.md).

## Third-party software

TeraRelay uses open-source dependencies including Tauri, React, Grammers, Actix, and other libraries. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and the lockfiles for the resolved dependency set.

## License

TeraRelay v0.1.0 does not grant an open-source license for TeraRelay's original source code. Copyright and other rights remain with the project owner unless a project license is added later. Third-party dependencies remain subject to their own licenses.
