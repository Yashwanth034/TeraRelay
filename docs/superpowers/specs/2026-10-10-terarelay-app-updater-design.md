# TeraRelay App Updater Design

## Goal

TeraRelay users should not need to browse GitHub and manually replace the application for routine upgrades. A published TeraRelay release should be discoverable from the installed app, safely downloadable, and installable through the platform-supported path.

## Scope

- Windows x64, Linux x64, macOS Intel, and macOS Apple Silicon: signed Tauri in-app updater.
- Android arm64: TeraRelay-hosted release manifest, verified signed APK download, SHA-256 verification, and handoff to Android's package installer.
- iOS: store-managed update surface. TeraRelay may detect a newer release and open a configured App Store/TestFlight URL; it must never attempt self-installation.
- Startup update check plus manual `Check for updates` in desktop and mobile About/Settings.
- Global update prompt with release notes, download/install state, progress where the platform provides it, retryable errors, and a Later action.
- GitHub Releases remain the distribution origin. Draft releases are ignored by clients until the human publishes them.

## Security model

### Desktop

Use `tauri-plugin-updater` only on desktop. Enable `bundle.createUpdaterArtifacts`, configure the GitHub `latest.json` endpoint, embed one permanent Tauri updater public key, and sign updater bundles in CI with `TAURI_SIGNING_PRIVATE_KEY` / optional password from GitHub Secrets. Signature verification is mandatory.

The updater signing key is independent of the Android APK signing key. The public updater key may be committed. The private updater key must never be committed or logged.

### Android

Android accepts an application update only when package/application ID, signing certificate, and version code satisfy Android's update rules. TeraRelay's release workflow already signs Android output; the updater must preserve that signing path.

TeraRelay will fetch a fixed `mobile-latest.json` release asset over HTTPS from `github.com/Yashwanth034/TeraRelay/releases/latest/download/mobile-latest.json`. The manifest contains the semantic version, release notes, publish time, Android APK URL, APK SHA-256, and optional iOS store URL.

The backend must reject malformed manifests, non-HTTPS APK URLs, unexpected download hosts, invalid SHA-256 strings, and versions that are not newer than the installed app. APK bytes are downloaded to the app cache, SHA-256 verified before installation, and only then exposed to Android's package installer through a FileProvider content URI. Android retains final install/user-consent authority.

If Android has not granted `Install unknown apps` permission to TeraRelay, the app opens the OS permission screen and preserves the already-downloaded verified APK so the user can return and continue.

### iOS

No sideload/self-replacement path. If a configured store URL exists, TeraRelay opens it. Otherwise the UI explains that the current iOS build has no public update channel. The existing repository currently validates an iOS simulator build only.

## Release pipeline

A `vX.Y.Z` tag continues to create one draft GitHub Release. Desktop matrix builds attach normal installers, updater bundles/signatures, and a generated `latest.json`. The updater signing private key is supplied only through GitHub Actions secrets.

Android CI builds the existing signed APK/AAB, normalizes release asset names, computes the APK SHA-256, uploads the APK/AAB to the same GitHub Release, and uploads `mobile-latest.json` to that release. The manifest points to the tag-specific APK URL. Clients use GitHub's `/releases/latest/download/...` alias, so draft releases do not become visible until published.

The release workflow must fail before publishing usable update metadata when required signing material or Android outputs are missing.

## Application architecture

### `app/src/appUpdater.ts`

Pure shared types, release-manifest validation helpers, version comparison helpers, fixed release endpoints, and platform-neutral status model.

### `app/src/context/AppUpdateContext.tsx`

Single update coordinator for the running app. It checks once shortly after startup, supports manual checks, owns the available-release state, drives desktop or Android install flows, listens for Android download progress, and exposes status/actions to all UI surfaces. Network/check failures never make the main app unusable.

### `app/src/components/shared/AppUpdatePrompt.tsx`

Global modal shown when a newer version is found or when the user explicitly checks. Displays current/latest version, release notes, progress, errors, Update/Retry/Later, and the platform-specific final action.

### Desktop adapter

Dynamically import `@tauri-apps/plugin-updater` and `@tauri-apps/plugin-process` only on desktop. `check()` supplies signed metadata. `downloadAndInstall()` supplies progress. Windows exits/installer restarts; macOS/Linux relaunch after install.

### Android backend

`commands/app_update.rs` supplies commands to check the fixed mobile manifest, download+verify the APK, report install permission, open the Android install-source settings page, and launch the verified APK with a FileProvider URI. Non-Android builds return a platform-not-supported error for Android-only commands.

A generated-Android configurator script idempotently adds `REQUEST_INSTALL_PACKAGES`, the FileProvider declaration, and a cache-path XML resource after `tauri android init`. Tauri's `beforeBuildCommand` runs the configurator so CI and local Android builds receive the same native wiring.

## UX

- Startup: check once after the app becomes usable. No blocking splash and no repeated prompts in one session after Later.
- Manual check: desktop About and mobile About include `Check for updates`.
- Current: display `You're up to date` after a successful manual check with no release.
- Available: prompt shows version and release notes.
- Downloading: disable duplicate actions and display determinate progress when total bytes are known, otherwise indeterminate activity.
- Failure: keep current installation untouched and expose Retry plus a concise error.
- Android permission: after verified download, explain that Android requires install permission and open the exact OS settings screen; never ask the user to browse GitHub.
- iOS: open store/TestFlight when configured; no fake self-update claim.

## Testing

- Node regression tests for manifest validation, semantic-version comparison, platform routing, UI wiring, updater config, release workflow wiring, and Android configurator idempotence.
- Rust unit tests for Android manifest parsing/validation helpers and host allow-list/checksum validation where platform-independent.
- `npm run build` for TypeScript/Vite.
- `cargo test --lib` and `cargo check` for Rust.
- Android native configurator test against a fixture manifest and generated tree; if Android toolchain is available, initialize/build far enough to validate the native manifest.
- Existing focused UI/prebuild QA and `git diff --check` remain green.

## Non-goals

- Silent forced updates.
- Background binary replacement without user consent.
- Delta/patch updates.
- A new TeraRelay update server.
- Publishing a draft automatically.
- Committing, tagging, pushing, or releasing as part of this implementation without separate explicit Git authorization.
