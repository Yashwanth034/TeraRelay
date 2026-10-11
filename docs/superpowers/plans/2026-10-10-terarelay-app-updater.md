# TeraRelay App Updater Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a secure, user-friendly in-app update path for TeraRelay desktop and Android, with an iOS store-managed fallback and GitHub Release automation.

**Architecture:** Desktop uses Tauri's signed updater and generated `latest.json`. Android uses a fixed GitHub release manifest, backend download/SHA-256 validation, and Android's package installer through a FileProvider. A shared React update context owns startup/manual checks and the global prompt.

**Tech Stack:** Tauri 2, Rust, reqwest, sha2, JNI/Android FileProvider, React 19, TypeScript, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-10-terarelay-app-updater-design.md`

## Global Constraints

- Do not stage, commit, push, tag, release, reset, restore, clean, or version-bump without separate explicit authorization.
- Preserve all existing Remote Import, Device Continuity, Version Stack, and Telegram-session work.
- Desktop updater private signing material must never be written to tracked source or logs.
- Android updates must retain package ID `io.terarelay.desktop` and the existing Android signing certificate.
- Draft GitHub Releases remain invisible to installed clients until manually published.
- Update failure must never prevent normal TeraRelay startup/use.

## Review Focus

- Malformed or malicious Android update manifest must be rejected without downloading/installing anything.
- Interrupted/failed downloads must leave the installed app untouched and remain retryable.
- Android install-source permission denial must not lose the already verified APK or dead-end the user.
- Desktop updater must not initialize on Android/iOS and mobile-only commands must not break desktop builds.
- A draft/prerelease or same/older version must not trigger an update prompt.

---

### Task 1: Updater contract and regression tests

**Files:**
- Create: `app/src/appUpdater.ts`
- Create: `scripts/test-app-updater.cjs`
- Modify: `scripts/prebuild-qa.sh`

**Interfaces:**
- Produces: `MobileUpdateManifest`, `AppUpdateRelease`, `compareVersions(a, b)`, manifest validation/normalization helpers, fixed endpoint constants.

- [ ] Write failing Node assertions covering semantic version ordering, invalid manifest/URL/hash rejection, updater config/workflow/native wiring expectations, and UI integration markers.
- [ ] Run `node scripts/test-app-updater.cjs` and confirm RED.
- [ ] Implement pure updater contract helpers.
- [ ] Re-run and keep infrastructure assertions RED until later tasks own them.

### Task 2: Desktop signed updater plumbing

**Files:**
- Modify: `app/package.json`
- Modify: `app/package-lock.json`
- Modify: `app/src-tauri/Cargo.toml`
- Modify: `app/src-tauri/Cargo.lock`
- Modify: `app/src-tauri/src/lib.rs`
- Modify: `app/src-tauri/tauri.conf.json`
- Modify: `app/src-tauri/capabilities/default.json`

**Interfaces:**
- Produces: desktop-only `@tauri-apps/plugin-updater` and process-relaunch runtime, configured GitHub `latest.json` endpoint, signed updater artifacts.

- [ ] Add updater/process dependencies with desktop Rust cfg for updater.
- [ ] Register updater only under `#[cfg(desktop)]` and process plugin cross-platform as needed.
- [ ] Enable updater artifacts, endpoint, permanent public key, passive Windows install mode, and capabilities.
- [ ] Run `cargo check`, frontend build, and updater regression assertions.

### Task 3: Android verified APK backend

**Files:**
- Create: `app/src-tauri/src/commands/app_update.rs`
- Modify: `app/src-tauri/src/commands/mod.rs`
- Modify: `app/src-tauri/src/lib.rs`

**Interfaces:**
- Produces Tauri commands:
  - `cmd_mobile_check_update() -> Option<MobileUpdateInfo>`
  - `cmd_android_download_update(app, version, url, sha256) -> String`
  - `cmd_android_can_install_packages() -> bool`
  - `cmd_android_open_install_permission()`
  - `cmd_android_install_update(apk_path)`
- Emits `app-update-download-progress` payload `{ downloaded, total }`.

- [ ] Add Rust unit tests for manifest normalization, host allow-list, SHA-256 syntax, and version comparison.
- [ ] Confirm focused Rust tests fail before implementation.
- [ ] Implement fixed-manifest fetch/validation, download-to-cache, checksum verification, progress events, Android permission/settings intent, and FileProvider install intent with Android cfg guards.
- [ ] Run focused Rust tests then full `cargo check`/`cargo test --lib`.

### Task 4: Android native FileProvider configuration

**Files:**
- Create: `scripts/configure-android-updater.mjs`
- Create: `scripts/test-android-updater-config.cjs`
- Modify: `app/src-tauri/tauri.conf.json`
- Modify generated Android tree only for local verification (ignored, not relied on as source).

**Interfaces:**
- Consumes: Android backend's cache-path/FileProvider authority contract.
- Produces idempotent generated-native changes: `REQUEST_INSTALL_PACKAGES`, FileProvider entry, `res/xml/terarelay_update_paths.xml`.

- [ ] Write fixture-based RED test for configurator.
- [ ] Implement idempotent configurator.
- [ ] Wire it into Tauri `beforeBuildCommand` after frontend build.
- [ ] Run configurator test twice against fixture and assert identical second result.
- [ ] If Android SDK/NDK are available, run `tauri android init --ci`, configurator, and a native manifest/build validation.

### Task 5: Shared update coordinator and prompt

**Files:**
- Create: `app/src/context/AppUpdateContext.tsx`
- Create: `app/src/components/shared/AppUpdatePrompt.tsx`
- Modify: `app/src/App.tsx`

**Interfaces:**
- Produces `useAppUpdate()` with release/status/progress/check/install/dismiss state.
- Consumes desktop Tauri updater and Android commands.

- [ ] Add regression assertions for startup check, manual check, Later suppression, desktop dynamic imports, Android progress listener, iOS store path, and retry state.
- [ ] Confirm RED.
- [ ] Implement provider, startup check, platform adapters, global prompt, and failure-safe state machine.
- [ ] Run frontend build and updater regression test.

### Task 6: Settings/About update controls

**Files:**
- Modify: `app/src/components/desktop/dashboard/SettingsModal.tsx`
- Modify: `app/src/components/mobile/MobileDashboard.tsx`
- Modify: `app/src/i18n/locales/en.json`
- Modify other locale JSON files structurally with English fallback strings to preserve key parity.

**Interfaces:**
- Consumes: `useAppUpdate()`.
- Produces: current-version/update-status display and `Check for updates` action on desktop/mobile.

- [ ] Extend regression assertions for both About surfaces.
- [ ] Implement controls with loading/success/error/available status.
- [ ] Add locale keys across all locale files without removing existing translations.
- [ ] Run locale structural checks and frontend build.

### Task 7: GitHub Release updater artifacts and mobile manifest

**Files:**
- Modify: `.github/workflows/release.yml`
- Create: `scripts/build-mobile-update-manifest.mjs`
- Create: `scripts/test-release-updater.cjs`

**Interfaces:**
- Consumes: desktop updater signing secrets and existing Android signing secrets.
- Produces: desktop `.sig`/updater bundles + `latest.json`; tag-specific Android APK/AAB; `mobile-latest.json` with APK SHA-256.

- [ ] Write RED workflow/manifest-generator tests.
- [ ] Add desktop `TAURI_SIGNING_PRIVATE_KEY` and password env plus explicit updater JSON/signature upload settings.
- [ ] Normalize Android output names, compute SHA-256, generate manifest, and attach APK/AAB/manifest to the same draft release.
- [ ] Add strict validation that tag/package versions agree and required outputs exist.
- [ ] Run release workflow static tests.

### Task 8: Permanent updater signing key provisioning

**Files:**
- Modify: `app/src-tauri/tauri.conf.json` public key only.
- No tracked private-key file.

**Interfaces:**
- Produces: one permanent Tauri updater public key embedded in installed desktop clients.
- Requires: GitHub repository secret `TAURI_SIGNING_PRIVATE_KEY` (and password if generated encrypted).

- [ ] Generate the Tauri updater keypair into a RepoTunnel-owned temporary task, never tracked.
- [ ] Insert only the public key into `tauri.conf.json`.
- [ ] If connected GitHub secret mutation is safely available, set the repository secret without echoing secret content; otherwise leave an explicit release-blocking verification failure rather than a fake working configuration.
- [ ] Ensure no private-key bytes appear in `git diff`, project search, logs, or generated docs.

### Task 9: Full verification

**Files:** all updater files plus existing regression surface.

- [ ] Run `node scripts/test-app-updater.cjs`.
- [ ] Run `node scripts/test-android-updater-config.cjs`.
- [ ] Run `node scripts/test-release-updater.cjs`.
- [ ] Run `node scripts/test-focused-ui.cjs`.
- [ ] Run `cd app && npm run build`.
- [ ] Run `cd app/src-tauri && cargo test --lib && cargo check`.
- [ ] Run `bash scripts/prebuild-qa.sh`.
- [ ] Run native `git diff --check` and RepoTunnel `git_diff_check`.
- [ ] Re-read workspace status to prove no staging/commit/push and no unrelated work loss.
