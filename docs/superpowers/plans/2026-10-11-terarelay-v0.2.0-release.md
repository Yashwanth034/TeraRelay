# TeraRelay v0.2.0 Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ship TeraRelay v0.2.0 from the current verified feature set without weakening signing/security gates or touching real Telegram/QA login state.

**Architecture:** Keep Linux and signed Android releasable immediately, keep Windows/macOS/iOS fail-closed until their real vendor credentials exist, and make CI select only credential-ready desktop targets. Use the existing Tauri updater flow with the new updater public key in source and its private key kept outside Git.

**Tech Stack:** GitHub Actions, Tauri 2, Rust, React/TypeScript, Node contract tests, Android signing.

**Spec:** Current release workflow plus the already-implemented updater/native-drive design docs.

## Global Constraints
- Do not modify or remove the real Telegram login/session path or QA bypass.
- Do not weaken Authenticode, Developer ID/notarization, App Store Connect, or updater-signature validation.
- Never commit private keys, passwords, certificates, provisioning profiles, sessions, or generated Remote Import helper binaries.
- Version this feature batch as 0.2.0.
- Push/tag/publish are explicitly authorized by the user for this release.

## Review Focus
- Missing Windows/macOS/iOS credentials must skip those release targets rather than produce unsigned artifacts.
- Linux and Android must still require the updater signing secret.
- The release tag must exactly match package version.
- The source updater public key must match the private key used by CI.
- The draft release must not be published until required available-platform jobs succeed.

---

### Task 1: Credential-aware release matrix
**Files:** Modify `.github/workflows/release.yml`, `scripts/test-release-updater.cjs`.
- [ ] Add failing contract checks for a release-capabilities job, dynamic desktop matrix, and conditional iOS job.
- [ ] Run the release contract test and observe RED.
- [ ] Implement capability detection without exposing secret values; Linux is always selected, Windows/macOS only when complete signing material exists, iOS only when all distribution credentials and URL exist.
- [ ] Run release contract test GREEN.

### Task 2: v0.2.0 updater/version metadata
**Files:** Modify `app/package.json`, `app/package-lock.json`, `app/src-tauri/Cargo.toml`, `app/src-tauri/Cargo.lock`, `app/src-tauri/tauri.conf.json`, `CHANGELOG.md`.
- [ ] Update updater public key from the protected local v0.2.0 signing keypair.
- [ ] Bump all product versions to 0.2.0.
- [ ] Add the v0.2.0 changelog entry covering Version Stacks, TeraRelay Boost, premium UI, Drive pool/continuity, Remote Import, updater, and native Windows/macOS Drive integration.
- [ ] Run updater/release contract tests and version consistency checks.

### Task 3: Release gate and source commit
- [ ] Run source hygiene, Remote Import tests, prebuild QA, cargo check, npm audit, release workflow YAML parse, and git diff checks.
- [ ] Stage only release-source changes and commit them.
- [ ] Push main.

### Task 4: Tag, CI release, and publish
- [ ] Create and push tag `v0.2.0` only after the clean release commit is on origin/main.
- [ ] Monitor the release workflow to terminal state.
- [ ] Verify Linux updater artifacts and signed Android APK/AAB/mobile manifest are present; verify skipped credential-unavailable targets are explicit.
- [ ] Publish the GitHub draft release only after all required available-platform jobs pass.
- [ ] Verify final release metadata and working tree state.
