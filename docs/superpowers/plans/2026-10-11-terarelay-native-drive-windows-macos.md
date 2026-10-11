# TeraRelay Native Drive — Windows + macOS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the existing TeraRelay Drive available as a native Windows Explorer cloud-files root and macOS Finder File Provider domain without duplicating storage, encryption, metadata, or pending-upload semantics.

**Architecture:** Extract platform-neutral filesystem/provider primitives from the existing Linux FUSE implementation, preserve Linux as a thin adapter, add a Windows CfAPI adapter in Rust, and add a macOS replicated File Provider extension that talks to the same Rust core over a loopback authenticated IPC boundary. Tauri lifecycle connects/disconnects the correct provider after continuity initialization, and release CI compiles/packages/verifies the real Windows/macOS provider components.

**Tech Stack:** Rust/Tauri 2.12.x, existing SQLite/Telegram/Drive crypto+streaming stack, `windows` Rust crate for CfAPI, Apple FileProvider/Foundation Swift APIs, Tauri macOS `bundle.macOS.files`, Node contract tests, Cargo tests, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-11-terarelay-native-drive-windows-macos-design.md`

## Global Constraints

- Preserve all unrelated uncommitted Device Continuity, Remote Import, Version Stack, updater, Android/iOS, and release work.
- Do not stage, commit, push, tag, reset, restore, clean, or publish; the user explicitly wants the live dirty workspace preserved.
- Linux FUSE behavior remains regression-clean.
- Windows uses Microsoft Cloud Files API; no WinFsp/runtime driver dependency and no literal drive-letter emulation in this feature.
- macOS uses `NSFileProviderReplicatedExtension` + `NSFileProviderEnumerating`; no macFUSE/kernel extension.
- Existing encrypted-file rules remain fail-closed; never persist decrypted encrypted-file blocks.
- Existing bounded write staging and crash-recovery semantics remain authoritative.
- Provider identities derive from existing Drive entry/directory IDs and are treated as untrusted when received from the OS.
- Platform code must remain behind target/platform boundaries so Linux builds do not acquire Windows/macOS link requirements.
- Windows native provider must compile in the normal Windows release job; macOS extension must be built, embedded under `Contents/PlugIns`, signed, and verified in macOS release jobs.
- No new user-facing Drive configuration model; Device Continuity remains the lifecycle owner.

## Review Focus

- Malformed/oversized provider identity bytes must be rejected before DB lookup or path use.
- Ranged hydration at EOF, zero-length, overflow, and cancellation boundaries must not over-read or mark partial data synced.
- A pending/dirty local file must never be overwritten by remote placeholder refresh/reconciliation.
- Provider reconnect after crash/restart must be idempotent and must not duplicate sync roots/domains or discard pending writes.
- Provider-supplied paths/names must not escape the registered root or staging tree; path traversal and symlink tricks must fail closed.

---

### Task 1: Extract platform-neutral Drive provider core

**Files:**
- Create: `app/src-tauri/src/drive_fs_core.rs`
- Modify: `app/src-tauri/src/drive_mount.rs`
- Modify: `app/src-tauri/src/lib.rs`
- Create/Test: `scripts/test-native-drive-platforms.cjs`

**Interfaces:**
- Produces: `DriveNodeId`, `DriveNodeKind`, `DriveNodeMetadata`, `DriveFsError`, `DriveFsCore` and stable provider identity encode/decode helpers.
- Produces provider operations: snapshot/list/lookup/metadata/read/create/write/resize/flush/rename/remove/recover.
- Consumes existing `DbConnection`, `TelegramState`, `DriveMountState`, Drive metadata helpers, crypto helpers, and streaming endpoint.

- [ ] **Step 1: Write RED architecture/identity tests** asserting `drive_fs_core.rs` exists, stable identity codec rejects malformed/oversized input, and the core is free of `fuser`, CfAPI, and FileProvider types.
- [ ] **Step 2: Run `node scripts/test-native-drive-platforms.cjs`** and confirm failure because the new core/module does not exist.
- [ ] **Step 3: Implement the smallest platform-neutral core** by moving reusable node/lookup/read/write/recovery semantics out of Linux-only scope and replacing OS errno/FUSE reply coupling with `DriveFsError`.
- [ ] **Step 4: Adapt `drive_mount.rs` Linux code to call the core** while keeping FUSE reply/error translation in the Linux module.
- [ ] **Step 5: Run focused Rust/core tests and `node scripts/test-native-drive-platforms.cjs`** until green.
- [ ] **Step 6: Run `cargo check` and the existing Drive-focused Rust tests**; expected zero regressions.

### Task 2: Preserve Linux FUSE behavior through the shared core

**Files:**
- Modify: `app/src-tauri/src/drive_mount.rs`
- Test: existing `app/src-tauri/src/drive_mount.rs` tests
- Test: `scripts/prebuild-qa.sh`

**Interfaces:**
- Consumes Task 1 `DriveFsCore` operations.
- Produces unchanged `cmd_drive_mount`, `cmd_drive_unmount`, `cmd_drive_status`, `cmd_drive_recover_pending`, and `unmount_best_effort` behavior on Linux.

- [ ] **Step 1: Add/adjust RED Linux adapter tests** for error mapping, lookup/list, ranged read, write/truncate, rename/move/delete, staging budget, encrypted locked reads, pending recovery, and provider status kind `fuse`.
- [ ] **Step 2: Run the focused Rust tests** and confirm the new adapter assertions fail before wiring.
- [ ] **Step 3: Wire FUSE callbacks to the core** with no storage-policy duplication.
- [ ] **Step 4: Run focused tests, full Rust library tests, `cargo check`, and `cargo fmt --check`**; expected all green.
- [ ] **Step 5: Run `scripts/prebuild-qa.sh`**; existing environment-only WebKit skip remains acceptable, no new failures.

### Task 3: Add Windows Cloud Files provider

**Files:**
- Create: `app/src-tauri/src/drive_windows.rs`
- Modify: `app/src-tauri/src/drive_mount.rs`
- Modify: `app/src-tauri/src/lib.rs`
- Modify: `app/src-tauri/Cargo.toml`
- Modify: `app/src-tauri/Cargo.lock`
- Test: `scripts/test-native-drive-platforms.cjs`
- Modify/Test: `.github/workflows/release.yml`

**Interfaces:**
- Consumes Task 1 `DriveFsCore`, stable identity codec, and `DriveMountState` lifecycle storage.
- Produces Windows `connect`, `disconnect`, placeholder projection, FETCH_DATA hydration, cancellation handling, local mutation mapping, and status kind `windows-cloud-files`.

- [ ] **Step 1: Add RED source-contract tests** requiring Windows-only `windows` + filesystem watcher dependencies, desktop-compatible `CfRegisterSyncRoot` registration, CfAPI connect/create-placeholder/execute/disconnect calls, recursive local-change monitoring, reconnect scan recovery, identity validation, cancellation/range guards, and no WinFsp dependency.
- [ ] **Step 2: Run `node scripts/test-native-drive-platforms.cjs`** and confirm the Windows contract fails.
- [ ] **Step 3: Implement idempotent Explorer sync-root registration and connection** using `CfRegisterSyncRoot` for the unpackaged MSI/NSIS desktop app plus `CfConnectSyncRoot` for Cloud Filter callbacks, stable provider identity, and an app-owned per-user sync root. Do not introduce a sparse/MSIX package identity.
- [ ] **Step 4: Implement placeholder projection and FETCH_DATA callbacks** backed only by shared-core enumeration/read operations.
- [ ] **Step 5: Implement recursive local create/content-write monitoring plus reconnect/full-scan recovery**, deduplicate rename/delete with CfAPI notifications, upload through shared-core pending operations, and convert successful local files back into in-sync placeholders without replacing dirty/pending nodes during remote refresh.
- [ ] **Step 6: Add Windows release compile/smoke verification** that compiles the provider and, where CfAPI is available, exercises temporary registration/placeholder/hydration/local-edit reconciliation/unregister without publishing anything.
- [ ] **Step 7: Run source contracts, Linux cargo check/tests, and workflow YAML parse**; expected green locally even though actual Windows linking executes in CI/Windows.

### Task 4: Add macOS replicated File Provider bridge and extension

**Files:**
- Create: `app/src-tauri/src/drive_provider_ipc.rs`
- Create: `app/src-tauri/macos-file-provider/Package.swift`
- Create: `app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderExtension.swift`
- Create: `app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderItem.swift`
- Create: `app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderEnumerator.swift`
- Create: `app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/BridgeClient.swift`
- Create: `app/src-tauri/macos-file-provider/Info.plist`
- Create: `app/src-tauri/macos-file-provider/TeraRelayFileProvider.entitlements`
- Create: `scripts/build-macos-file-provider.sh`
- Modify: `app/src-tauri/tauri.conf.json`
- Modify: `app/src-tauri/entitlements.plist`
- Test: `scripts/test-native-drive-platforms.cjs`

**Interfaces:**
- Consumes Task 1 `DriveFsCore` via a versioned local authenticated IPC service.
- Produces replicated File Provider enumerate/fetch/create/modify/delete operations and change-anchor signaling; produces a signed `.appex` staged for Tauri `Contents/PlugIns` embedding.

- [ ] **Step 1: Add RED IPC/extension contract tests** for protocol versioning, bounded request sizes, capability authentication, loopback-only binding, File Provider protocols, stable item identifiers, and app-extension metadata/entitlements.
- [ ] **Step 2: Run `node scripts/test-native-drive-platforms.cjs`** and confirm the macOS contract fails.
- [ ] **Step 3: Implement Rust loopback IPC service** with per-install capability, request/response types, provider identity validation, bounded content transfer, and Drive change anchors.
- [ ] **Step 4: Implement Swift replicated File Provider adapter** for enumeration, fetch/materialization, create/modify/delete, rename/move, and change enumeration; no Telegram/encryption logic in Swift.
- [ ] **Step 5: Implement macOS extension build script** using `swiftc`/`xcrun`/`codesign`, producing `TeraRelayFileProvider.appex` only on macOS and failing closed when required signing inputs are absent for release mode.
- [ ] **Step 6: Configure Tauri `bundle.macOS.files`** to embed the generated extension at `PlugIns/TeraRelayFileProvider.appex` and apply matching App Group entitlements.
- [ ] **Step 7: Run IPC Rust tests, source contracts, Linux cargo check/tests, and configuration parse**; expected green locally. Actual FileProvider compile/sign is verified on macOS CI.

### Task 5: Cross-platform provider lifecycle and UI/status

**Files:**
- Modify: `app/src-tauri/src/drive_mount.rs`
- Modify: `app/src-tauri/src/lib.rs`
- Modify: `app/src/hooks/useDeviceContinuity.ts`
- Modify: `app/src/components/desktop/dashboard/SettingsModal.tsx`
- Modify: locale JSON only if new visible strings are required
- Test: `scripts/test-focused-ui.cjs`
- Test: `scripts/test-native-drive-platforms.cjs`

**Interfaces:**
- Consumes Windows/macOS/Linux provider connect/disconnect implementations.
- Produces cross-platform `DriveMountStatus` fields `provider_kind`, `supported`, `mounted`, `mount_point`, `read_streaming`, `write_staging`, and actionable error/setup state.

- [ ] **Step 1: Add RED UI/lifecycle tests** proving initial continuity/reconciliation precedes provider connection, cleanup disconnects on sign-out/exit, provider status is surfaced without a second configuration model, and mobile never starts a desktop provider.
- [ ] **Step 2: Run focused UI/native-drive tests** and confirm failure for missing cross-platform status/lifecycle.
- [ ] **Step 3: Generalize native lifecycle** so Linux mounts, Windows connects CfAPI, macOS starts IPC/registers domain, and mobile remains unsupported.
- [ ] **Step 4: Update Settings status/reconnect presentation minimally** using existing Drive UI patterns and locale parity rules.
- [ ] **Step 5: Run focused UI tests, production frontend build, locale structure tests, and Rust tests**; expected green.

### Task 6: Release packaging, exhaustive verification, and audit

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `scripts/prebuild-qa.sh`
- Modify/Test: `scripts/test-native-drive-platforms.cjs`
- Modify: `THIRD_PARTY_NOTICES.md` only if required by new dependency license terms

**Interfaces:**
- Consumes all prior tasks.
- Produces fail-closed platform release gates for Windows provider compilation and macOS `.appex` build/embed/sign verification.

- [ ] **Step 1: Add RED release contract assertions** for Windows target compile, macOS extension build before Tauri bundle, `.app/Contents/PlugIns/TeraRelayFileProvider.appex` existence, extension entitlements, recursive code-sign verification, and preservation of existing updater/AuthentiCode/notarization logic.
- [ ] **Step 2: Run release/native-drive contracts** and confirm missing workflow gates fail.
- [ ] **Step 3: Implement workflow steps** with no changes to unrelated Android/iOS/updater publishing semantics.
- [ ] **Step 4: Run clean `npm ci`, native-drive contracts, focused UI tests, production build, locale parity, full Rust library tests, `cargo check`, `cargo fmt --check`, release YAML parse, `scripts/prebuild-qa.sh`, shell `git diff --check`, and RepoTunnel native `git_diff_check`.
- [ ] **Step 5: Fresh self-review against the spec and Review Focus** because no independent subagent is available in this harness; fix Critical/Important findings RED→GREEN once, then rerun affected/full gates.
- [ ] **Step 6: Record platform-verification boundary honestly**: Linux behavior can be locally executed; actual CfAPI and File Provider runtime/signing gates require their native OS runners and must not be claimed from Linux-only evidence.
