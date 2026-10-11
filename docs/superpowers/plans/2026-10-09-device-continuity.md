# TeraRelay Device Continuity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reuse TeraRelay's existing Telegram-backed Drive/logical-file/stack metadata so a newly authenticated device restores the same cloud-side TeraRelay state automatically, including encrypted-Drive unlock, without copying login/session data.

**Architecture:** Move the existing Drive startup/sync orchestration out of `App.tsx` into a focused continuity hook. The hook snapshots local Drive state, runs the existing remote metadata sync before desktop mount, classifies a true new-device restore, polls every 15 seconds on desktop/mobile, and exposes only the small UI state needed by a new unlock dialog. No new cloud backend, storage schema, or auth flow is introduced.

**Tech Stack:** React/TypeScript, Tauri invoke API, Sonner, existing Rust/Tauri Drive commands.

**Spec:** `docs/superpowers/plans/2026-10-09-device-continuity-design.md`

## Global Constraints

- Reuse `cmd_drive_sync_metadata`, `cmd_drive_get_snapshot`, `cmd_drive_crypto_status`, `cmd_drive_crypto_unlock`, `cmd_drive_recover_pending`, `cmd_drive_mount`, and `cmd_drive_unmount`.
- Do not copy or alter Telegram/TDLib session databases.
- Production auth remains unchanged; isolated QA uses the existing Feature A bypass.
- Linux Drive mount remains desktop-only; metadata sync runs for desktop and mobile.
- Do not bump version numbers.
- Do not stage, commit, push, tag, or release; the user will explicitly decide the next Git checkpoint.

## Review Focus

- New-device local Drive is empty while remote sync is temporarily offline: show a retry-safe warning and still allow desktop mount/cached use.
- Existing device with local Drive data: do not falsely announce a new-device restore.
- Encrypted remote Drive with no local key: ask only for existing passphrase; never call setup/create-key.
- User dismisses unlock dialog: leave Drive locked and usable elsewhere; do not re-prompt continuously in the same session.
- Mobile authenticated client: perform metadata reconciliation without attempting Linux mount/unmount.

---

### Task 1: Continuity classification and orchestration hook

**Files:**
- Create: `app/src/deviceContinuity.ts`
- Create: `app/src/hooks/useDeviceContinuity.ts`
- Modify: `scripts/test-focused-ui.cjs`

**Interfaces:**
- Produces `DriveSnapshotLike`, `DriveSyncResultLike`, `DriveCryptoStatusLike`, `classifyDeviceContinuity(...)`, and `useDeviceContinuity({ authenticated, isMobile })`.
- The hook returns `{ state, markDriveUnlocked }`, where state includes restore phase, restore counts, last sync source/error, and whether initial encrypted-Drive unlock is required.

- [ ] **Step 1: Write failing focused source-contract tests** asserting the continuity module/hook use the existing commands, detect pristine+remote restore only, sync before mount, keep 15-second polling on mobile/desktop, and do not call Drive crypto setup.
- [ ] **Step 2: Run `node scripts/test-focused-ui.cjs`** and verify the new continuity tests fail for missing implementation.
- [ ] **Step 3: Implement the pure classification helpers and continuity hook** with initial snapshot -> recover pending (desktop) -> remote sync -> post-sync snapshot -> crypto status -> desktop mount ordering. On failure, preserve local/mount behavior and let the interval retry.
- [ ] **Step 4: Run `node scripts/test-focused-ui.cjs`** and verify the focused suite passes.

### Task 2: Existing-passphrase unlock experience

**Files:**
- Create: `app/src/components/shared/DriveContinuityUnlock.tsx`
- Modify: `app/src/App.tsx`
- Modify: `app/src/App.css`
- Modify: `scripts/test-focused-ui.cjs`

**Interfaces:**
- Consumes `useDeviceContinuity` from Task 1.
- Produces a modal accepting `open`, restore counts, `onUnlocked`, and `onDismiss`; it invokes only `cmd_drive_crypto_unlock`.

- [ ] **Step 1: Write failing focused tests** proving `App.tsx` uses the continuity hook instead of the old inline sync/mount effect, the unlock UI contains `Later`, and unlock invokes `cmd_drive_crypto_unlock` but never `cmd_drive_crypto_setup`.
- [ ] **Step 2: Run `node scripts/test-focused-ui.cjs`** and verify RED.
- [ ] **Step 3: Implement the unlock dialog and App integration**, including one-time success/warning toasts from continuity state and passphrase clearing after submit/dismiss.
- [ ] **Step 4: Run `node scripts/test-focused-ui.cjs` and `npm run build` in `app/`** and verify GREEN/build success.

### Task 3: Isolated bypass QA and full regression

**Files:**
- No production-file changes expected unless QA exposes a real defect; any fix must follow RED -> GREEN.

**Interfaces:**
- Uses the existing debug-only `VITE_TERA_QA_FEATURE_A=1` / `TERARELAY_QA_FEATURE_A=1` login bypass.

- [ ] **Step 1: Launch/reuse isolated QA app without Telegram login**, configure Drive encryption with a test-only passphrase, lock it, reload, and verify the continuity unlock dialog appears.
- [ ] **Step 2: Unlock with the same test-only passphrase** and verify the dashboard remains usable and no production login/session is touched.
- [ ] **Step 3: Run `bash scripts/prebuild-qa.sh`** and verify final PASS, including Rust, recovery, frontend build, and source hygiene.
- [ ] **Step 4: Run `git diff --check`/RepoTunnel diff check and inspect final status**. Leave all implementation changes uncommitted and report the exact changed public paths separately from internal plan files.
