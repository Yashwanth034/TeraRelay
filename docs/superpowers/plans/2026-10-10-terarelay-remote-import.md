# TeraRelay Remote Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a complete paste-link Remote Import flow that acquires supported remote media/files/torrents and hands staged outputs into TeraRelay's existing resilient upload + Version Stack system.

**Architecture:** A new Rust `remote_import` command module owns source routing, engine discovery, metadata analysis, acquisition, cancellation, staging, and torrent sessions. A new frontend hook owns durable import-job orchestration and hands completed staged files into a small new programmatic entry point on the existing `useFileUpload` queue. The existing TeraRelay logical-file upload, transfer chooser, duplicate/version checks, manifests, and Version Stack commands remain authoritative.

**Tech Stack:** Rust/Tauri, Tokio, reqwest, librqbit 9.0.1, yt-dlp, Deno, FFmpeg/ffprobe, aria2c, gallery-dl, React/TypeScript, existing Tauri Store transfer persistence.

**Spec:** `docs/superpowers/specs/2026-10-10-terarelay-remote-import-design.md`

## Global Constraints

- Work only in the approved TeraRelay repository/workspace.
- Do not modify or inspect sensitive Telegram/TDLib profile/session contents.
- Preserve all existing uncommitted Device Continuity work.
- Do not modify `app/src-tauri/src/commands/drive_metadata.rs`, `app/src/App.tsx`, `scripts/test-focused-ui.cjs`, or Device Continuity files for this feature.
- Do not stage, commit, push, reset, restore, clean, tag, publish, or release without separate explicit Git authorization.
- Destination is the currently open writable TeraRelay channel; no channel dropdown in Remote Import.
- One selected video quality is a normal file; 2+ selected video qualities automatically become one existing Version Stack with highest quality primary.
- The new remote-import acquisition path must hand files to the normal logical upload path; it must not use the legacy direct `upload_stream` URL uploader.
- Remote Import must not inherit the legacy 2 GB URL-upload ceiling.
- External command arguments must never be constructed through a shell; source URLs and paths are passed as individual process arguments.
- DRM circumvention is out of scope and must return a clear unsupported error.
- No remote-source cookies, headers, signed URLs, or secrets are logged.

## Review Focus

- Malicious source/file names with `../`, absolute paths, shell characters, or duplicate names must stay inside the job staging directory and never become shell syntax.
- App restart during an acquisition must recover the durable job and reuse resumable engine state rather than silently creating a second job.
- Multi-quality partial success must keep valid uploads visible and must not publish a broken Version Stack referencing missing members.
- A direct URL or media extraction larger than 2 GB must successfully hand off to the normal logical multipart upload path rather than being rejected by the legacy URL limit.
- Cancelling an import must terminate only that import's engine/subprocess/torrent session and clean only that job's owned staging files.

---

### Task 1: Remote Import Domain, Source Router, and Quality Normalization

**Files:**
- Create: `app/src-tauri/src/commands/remote_import.rs`
- Modify: `app/src-tauri/src/commands/mod.rs`
- Modify: `app/src-tauri/src/lib.rs`
- Modify: `app/src-tauri/Cargo.toml`

**Interfaces:**
- Produces `RemoteImportSourceKind`, `RemoteImportQuality`, `RemoteImportAnalysis`, `RemoteImportItem`, `RemoteImportEngineStatus` serde models.
- Produces `classify_source(&str) -> Result<RemoteImportSourceKind, String>`.
- Produces quality normalization from yt-dlp JSON formats with stable engine format IDs and exactly one recommended choice preferring suitable 1080p.
- Produces Tauri commands `cmd_remote_import_engine_status` and `cmd_remote_import_analyze`.

- [ ] Write Rust tests first for magnet/direct-file/web classification, malicious/invalid input rejection, quality deduplication, 1080p recommendation, 4K/8K labels, FPS/HDR labels, and stable format IDs.
- [ ] Run focused tests and verify RED because the module/functions do not exist.
- [ ] Add the minimal module/types/router/normalizer and command registration; add `librqbit = "9.0.1"` but do not implement acquisition yet.
- [ ] Run focused Rust tests GREEN, then `cargo check`.

### Task 2: Engine Discovery and Safe yt-dlp / gallery-dl Analysis

**Files:**
- Modify: `app/src-tauri/src/commands/remote_import.rs`
- Reuse read-only: `app/src-tauri/src/transcode.rs`

**Interfaces:**
- Produces sidecar/resource/PATH engine lookup for `yt-dlp`, `deno`, `aria2c`, `gallery-dl`, `ffmpeg`, `ffprobe`.
- `cmd_remote_import_engine_status` returns availability/version diagnostics without exposing host paths to normal UI.
- `cmd_remote_import_analyze` invokes yt-dlp with JSON metadata only, no download, and converts it to normalized qualities/items; gallery-dl metadata fallback is bounded and only used when applicable.

- [ ] Write tests using fake executable scripts in a temp directory: argv is passed literally (shell metacharacters do not execute), yt-dlp JSON is normalized, stderr errors are sanitized, and missing-engine errors are actionable.
- [ ] Verify RED.
- [ ] Implement engine lookup and subprocess helpers using `tokio::process::Command` directly; reuse existing FFmpeg discovery semantics rather than changing transcode code.
- [ ] Implement yt-dlp `--dump-single-json --skip-download` analysis with no playlist flattening loss; parse formats and playlist entries safely.
- [ ] Implement gallery-dl metadata fallback for gallery/carousel sources where a usable standalone executable exists.
- [ ] Run focused tests GREEN and `cargo check`.

### Task 3: Resumable Acquisition Engines and Owned Staging

**Files:**
- Modify: `app/src-tauri/src/commands/remote_import.rs`

**Interfaces:**
- Produces `RemoteImportAcquireRequest` and `RemoteImportAcquireResult`.
- Produces Tauri commands `cmd_remote_import_acquire`, `cmd_remote_import_cancel`, `cmd_remote_import_cleanup`.
- Emits `remote-import-progress` with job ID, phase, bytes, total, speed, and optional item/quality ID.
- Acquisition writes only under app-cache `remote-import/<validated-job-id>/`.

- [ ] Write tests first for job ID validation, staging path containment, filename sanitization, cancellation ownership, direct-download resume flags, and >2 GB result acceptance (using sparse/test metadata without downloading 2 GB).
- [ ] Verify RED.
- [ ] Implement job-owned staging helper and active-process cancellation registry.
- [ ] Implement yt-dlp acquisition for exact selected format IDs, with Deno available to yt-dlp, FFmpeg merge/remux, aria2 external downloader when available, and yt-dlp internal downloader fallback.
- [ ] Implement direct HTTP(S) acquisition with aria2 `--continue=true`/bounded connections and existing reqwest streaming fallback when aria2 is unavailable/fails before useful output.
- [ ] Implement gallery-dl acquisition for selected gallery items with path-safe output templates.
- [ ] Verify output exists/non-empty, return stable display/upload name separately from staging path, and never apply a 2 GB ceiling.
- [ ] Run focused tests GREEN and `cargo check`.

### Task 4: librqbit Magnet/Torrent Analysis and Selective Acquisition

**Files:**
- Modify: `app/src-tauri/src/commands/remote_import.rs`

**Interfaces:**
- Magnet and remote `.torrent` analysis returns torrent file items with stable file indices, safe display paths, and sizes.
- Acquisition accepts selected torrent file indices and downloads only those files with fastresume/resume behavior.
- Cancellation stops/forgets only the job's managed torrent while preserving completed staged outputs until cleanup.

- [ ] Write tests first for torrent path normalization, file-index selection translation, output containment, and cancellation lookup; use a tiny locally generated torrent fixture or librqbit list-only metadata where network is not required.
- [ ] Verify RED.
- [ ] Implement per-job librqbit session management with `Session`, `AddTorrentOptions { list_only, only_files, output_folder, overwrite, .. }`, DHT/PEX defaults, and selected-file completion waiting.
- [ ] Emit progress from librqbit stats and return one acquired output per selected real file, ignoring padding/unsafe paths.
- [ ] Run focused tests GREEN and `cargo check`.

### Task 5: Durable Frontend Remote Import Job Orchestration

**Files:**
- Create: `app/src/remoteImport.ts`
- Create: `app/src/hooks/useRemoteImport.ts`
- Modify: `app/src/types.ts`
- Modify: `app/src/hooks/useFileUpload.ts`
- Create: `scripts/test-remote-import-ui.cjs`

**Interfaces:**
- `useRemoteImport(activeFolderId, store, enqueuePreparedImports)` owns durable jobs and backend progress events.
- `useFileUpload` exposes `enqueuePreparedImports(inputs)` where each input supplies staging path, captured destination, display name, optional stack coordination metadata, and cleanup ownership.
- Staged imports enter normal `cmd_preflight_upload_candidate` + `cmd_upload_file`; no `cmd_upload_from_url` use.

- [ ] Write the new focused UI/source-contract test first; assert durable job fields, immutable destination capture, no legacy URL uploader handoff, and cleanup only after upload success/cancel.
- [ ] Run it RED.
- [ ] Add TypeScript job/analysis/acquisition types and pure helpers for recommended selection, highest-quality ordering, phase normalization, and recovery normalization.
- [ ] Add `enqueuePreparedImports` to `useFileUpload` with optional explicit `uploadName` passed through to `cmd_upload_file`; preserve all existing local-file behavior unchanged.
- [ ] Implement `useRemoteImport` persistence/recovery, progress listener, analyze/start/cancel/retry, and staged-file handoff.
- [ ] Run focused script GREEN and `npm run build`.

### Task 6: Automatic Version Stack Finalization for Multi-Quality Imports

**Files:**
- Modify: `app/src/hooks/useFileUpload.ts`
- Modify: `app/src/hooks/useRemoteImport.ts`
- Modify: `app/src/types.ts`
- Modify: `scripts/test-remote-import-ui.cjs`

**Interfaces:**
- Upload queue reports successful imported logical file IDs back to the owning import job.
- One selected quality performs no stack command.
- 2+ successful selected video qualities: first two create the existing stack; later members use existing add command; all labels use `cmd_update_file_stack_label`; highest selected successful quality becomes primary.
- Partial failure never deletes successful files and never references failed members.

- [ ] Extend focused test RED for one-quality/no-stack, two-quality/create-stack, three-quality/add-member, quality labels, highest-quality primary, and partial failure.
- [ ] Add minimal import completion callback/result metadata to the existing upload queue without changing ordinary upload semantics.
- [ ] Implement stack orchestration entirely through existing Tauri Version Stack commands and invalidate file queries once finalization finishes.
- [ ] Run focused test GREEN and `npm run build`.

### Task 7: Clean Remote Import Modal in Current Channel

**Files:**
- Replace behavior in: `app/src/components/desktop/dashboard/RemoteUploadModal.tsx`
- Modify: `app/src/components/desktop/DesktopDashboard.tsx`
- Modify: `app/src/components/desktop/dashboard/TopBar.tsx` only if needed to disable action outside writable channel
- Modify: `app/src/i18n/locales/en.json`
- Modify other locale JSON only for required new keys using safe English fallback values if project convention requires key parity
- Modify: `scripts/test-remote-import-ui.cjs`

**Interfaces:**
- Existing globe action becomes Remote Import/Upload from Link for writable current channel only.
- Modal receives captured `activeFolderId`; it does not receive or render `folders`.
- First state is URL/source input + Analyze. Video analysis state renders multi-select qualities directly. Torrent/gallery/playlist analysis renders selectable items directly.
- Import button starts selected outputs; no separate version-stack toggle.

- [ ] Extend UI contract test RED: no `<select>` destination dropdown, no folders prop, current channel required, quality checkboxes allow multiple, recommended selected by default, one Import button.
- [ ] Implement modal states and wire `useRemoteImport` in DesktopDashboard while leaving unrelated dashboard behavior untouched.
- [ ] Keep UI compact and reuse existing premium/modal primitives/classes; no new channel list.
- [ ] Run focused test GREEN and `npm run build`.

### Task 8: End-to-End Regression and Real Engine Smoke Tests

**Files:**
- Modify only if a test exposes a Remote Import defect in files owned by Tasks 1–7.
- Do not change unrelated tests merely to make failures disappear.

**Interfaces:**
- Full feature must coexist with current Device Continuity uncommitted work and existing upload/download/version-stack behavior.

- [ ] Run Rust Remote Import tests and full `cargo test --lib`.
- [ ] Run `node scripts/test-remote-import-ui.cjs`.
- [ ] Run `npm run build`.
- [ ] Run `bash scripts/prebuild-qa.sh` without editing the existing modified `scripts/test-focused-ui.cjs`.
- [ ] Run `git diff --check` for unstaged changes through RepoTunnel.
- [ ] Smoke-test real local engines: engine status; yt-dlp metadata analysis of a small public test URL without downloading; direct HTTP acquisition against a local temporary HTTP server; FFmpeg/ffprobe availability; aria2 path; librqbit compile/runtime initialization. Do not download copyrighted media as a QA fixture.
- [ ] Inspect Git diff and verify no pre-existing unrelated modified files were changed by this feature.
- [ ] Perform final self-review because no independent subagent tool is available; Critical/Important findings get one TDD fix pass, minors are reported rather than silently expanded.
