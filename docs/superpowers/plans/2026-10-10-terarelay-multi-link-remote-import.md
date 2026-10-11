# TeraRelay Multi-Link Remote Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the finished single-link Remote Import flow so mixed multiple links can be analyzed/configured independently, executed safely in source order, and manually grouped across links into existing TeraRelay Version Stacks.

**Architecture:** Add a focused frontend batch model/orchestrator above the existing `RemoteImportJob` execution unit. Keep all source extraction, torrent acquisition, upload queue handoff, and Version Stack commands unchanged; the new batch layer owns ordered source entries, explicit multi-link selections, manual cross-link stack groups, durable recovery, and UI presentation.

**Tech Stack:** React + TypeScript, Tauri invoke/event APIs, `@tauri-apps/plugin-store`, existing TeraRelay Remote Import backend, existing upload queue, existing Version Stack commands, focused Node contract tests, existing Rust/prebuild regression suites.

**Spec:** `docs/superpowers/specs/2026-10-10-terarelay-multi-link-remote-import-design.md`

## Global Constraints

- Current open writable channel is the immutable batch destination; never add a channel dropdown.
- Multi-link mode means 2+ nonblank sources and has **no default selected qualities/files/items**.
- Single-link behavior remains unchanged, including its existing recommended default selection.
- Analysis and source execution are sequential in pasted source order.
- Cross-source Version Stacks are manual only; never infer groups from filenames, titles, hashes, dimensions, or media metadata.
- One output may belong to at most one cross-source group.
- If any output from a source belongs to a manual cross-source group, automatic same-source stacking is disabled for that source; ungrouped outputs remain standalone unless explicitly grouped.
- Reuse existing `RemoteImportJob`, acquisition commands, prepared-file upload queue handoff, durable upload outcomes, and Version Stack commands.
- Do not modify Device Continuity, Drive metadata/crypto/mount logic, sharing, TDLib auth/session handling, or unrelated dashboard/feed behavior.
- Do not stage, commit, push, tag, release, version-bump, reset, restore, or clean without explicit user authorization for that exact Git/destructive action.
- Never inspect, expose, copy, delete, or log out the preserved Telegram/TDLib QA session.

## Review Focus

1. **Duplicate pasted sources configured differently:** preserve duplicate source lines as distinct entry IDs and independent selections.
2. **Overlapping stack membership:** reject an output already owned by another manual group and suppress same-source auto-stack when a manual group touches that source.
3. **Restart after stack creation but before all members are added:** persist `stackId` and `stackedFileIds` after each successful mutation so retry is idempotent.
4. **Upload succeeds before batch state catches up:** resolve uploaded logical file IDs from existing durable `RemoteImportJob.uploadOutcomes` instead of reacquiring/reuploading.
5. **Later source failure after earlier success:** keep completed entries/results intact and expose a partial/paused batch state with Retry/Skip rather than rolling back earlier work.

---

## File Structure

- Create `app/src/remoteImportBatch.ts` — pure batch types, parsing, validation, selection/group planning, recovery normalization, and persistence helpers.
- Modify `app/src/remoteImport.ts` — expose only small generic helpers needed by the batch planner; do not move/duplicate engine logic.
- Modify `app/src/hooks/useRemoteImport.ts` — add explicit no-default analysis support and batch-safe single-job execution/finalization controls while preserving current single-link API behavior.
- Create `app/src/hooks/useRemoteImportBatch.ts` — durable sequential batch orchestration and manual cross-source stack finalization.
- Modify `app/src/components/desktop/dashboard/RemoteUploadModal.tsx` — switch between existing single-link UI and compact multi-link batch UI.
- Modify `app/src/components/desktop/DesktopDashboard.tsx` — wire the batch hook into the existing Remote Import modal using the current channel only.
- Modify `scripts/test-remote-import-ui.cjs` — preserve all existing single-link contracts and add focused multi-link contracts.
- Create `scripts/test-remote-import-batch.cjs` — executable pure-model/recovery tests for batch parsing, grouping, ordering, and idempotent planning.

### Task 1: Pure Batch Model and Parsing

**Files:**
- Create: `app/src/remoteImportBatch.ts`
- Test: `scripts/test-remote-import-batch.cjs`

**Interfaces:**
- Consumes: `RemoteImportAnalysis`, `RemoteImportJob`, `RemoteImportUploadOutcome` from `app/src/remoteImport.ts`.
- Produces:
  - `REMOTE_IMPORT_BATCH_STORE_KEY: string`
  - `MAX_REMOTE_IMPORT_BATCH_SOURCES: number` (set to `50`)
  - `RemoteImportBatch`, `RemoteImportBatchEntry`, `RemoteImportStackGroup`, `RemoteImportMemberRef` types
  - `parseRemoteImportSources(input: string): string[]`
  - `createRemoteImportBatch(folderId: number, rawInput: string): RemoteImportBatch`
  - `multiLinkDefaultSelectionIds(analysis: RemoteImportAnalysis): string[]`
  - `validateBatchReadyForImport(batch: RemoteImportBatch, jobsById: Map<string, RemoteImportJob>): string[]`
  - `normalizeRecoveredRemoteImportBatches(value: unknown): RemoteImportBatch[]`
  - serialized `loadPersistedRemoteImportBatches()` / `persistRemoteImportBatches()` helpers following the existing job-store pattern.

- [ ] **Step 1: Write failing pure-model tests**

Add assertions that:
- blank lines are removed;
- source order is preserved;
- duplicate links remain duplicated;
- 51 nonblank links fail with an explicit 50-source limit error;
- `createRemoteImportBatch(77, input)` captures `folderId=77` once and creates stable distinct entry IDs;
- `multiLinkDefaultSelectionIds()` returns `[]` for web qualities, torrent items, and one direct-file item;
- interrupted active batch phases recover to retryable/paused state without changing completed entries.

- [ ] **Step 2: Run the new test and verify RED**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: FAIL because `app/src/remoteImportBatch.ts` and its exported interfaces do not exist yet.

- [ ] **Step 3: Implement the pure batch model**

Use a versioned durable batch schema, unique entry/group IDs, exact source order, and serialized store writes. No backend/Tauri calls belong in this file.

- [ ] **Step 4: Run the focused batch model tests**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: PASS for parsing, no-default selection, 50-source limit, duplicate preservation, immutable destination, and recovery normalization.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 1 evidence to the existing `.superpowers/sdd/.../progress.md`; do not stage/commit.

### Task 2: Manual Cross-Link Group Planner

**Files:**
- Modify: `app/src/remoteImportBatch.ts`
- Test: `scripts/test-remote-import-batch.cjs`

**Interfaces:**
- Consumes: Task 1 batch/member types and analyzed `RemoteImportJob`s.
- Produces:
  - `groupSelectedMembers(batch, members, options): RemoteImportBatch`
  - `ungroupMembers(batch, groupId): RemoteImportBatch`
  - `validateStackGroups(batch, jobsById): string[]`
  - `sourceUsesManualStack(batch, entryId): boolean`
  - `planEntryFinalization(batch, entry, job): { automaticSameSourceStack: boolean; standaloneSelectionIds: string[] }`
  - `resolveGroupMembers(batch, group, jobsById): ResolvedBatchGroupMember[]`
  - `planCrossSourceStackFinalization(batch, group, jobsById): CrossSourceStackPlan`

- [ ] **Step 1: Add failing grouping tests**

Pin these cases:
- two selected outputs from different entries can form one group;
- grouping one output twice is rejected;
- missing/unselected member references are rejected;
- no cross-source group appears without an explicit grouping call;
- manual group involvement disables same-source auto-stack for the entire touched source;
- untouched source with 2+ selected qualities retains the existing automatic same-source stack rule;
- explicit primary member is persisted; without one, planner can *suggest* highest video height but must keep confirmation state separate;
- torrent-A 1080p + torrent-B 2160p resolves as two valid manual group members.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: FAIL only on missing group/planner behavior.

- [ ] **Step 3: Implement group validation/planning**

Represent members as `(entryId, selectionId)` references. Do not compare titles/filenames to infer relationships. Ensure a source touched by any manual group returns `automaticSameSourceStack=false` for all of that source's selected outputs.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: PASS including overlap rejection and torrent cross-link grouping.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 2 evidence to progress; no Git actions.

### Task 3: Batch-Safe Single-Job APIs

**Files:**
- Modify: `app/src/hooks/useRemoteImport.ts`
- Modify: `app/src/remoteImport.ts` only if a small pure helper is necessary
- Test: `scripts/test-remote-import-ui.cjs`
- Test: `scripts/test-remote-import-batch.cjs`

**Interfaces:**
- Consumes: existing `analyzeSource`, `startImport`, durable job store/outcomes, and `planRemoteImportFinalization`.
- Produces additive APIs without breaking existing callers:
  - `analyzeSource(sourceInput: string, options?: { defaultSelection?: boolean }): Promise<RemoteImportJob>` with default `true` so current single-link behavior remains unchanged.
  - `startImport(jobId, selectedIds?, options?: { deferAutomaticFinalization?: boolean }): Promise<RemoteImportJob>` or equivalent explicit batch-safe control.
  - a read-only job lookup/snapshot API suitable for batch orchestration, e.g. `getRemoteImportJob(jobId): RemoteImportJob | undefined`.
  - an explicit same-source finalization entry point that the batch hook can invoke only when the entry planner allows it.

- [ ] **Step 1: Add failing compatibility tests**

Assert:
- ordinary `analyzeSource(source)` still applies `defaultRemoteImportSelectionIds()`;
- batch analysis requests no defaults and receives `selectedIds=[]`;
- batch-controlled jobs do not auto-create a same-source stack before the batch planner decides ownership;
- existing single-link finalizer still uses only `cmd_create_file_stack`, `cmd_add_file_to_stack`, `cmd_update_file_stack_label`, and `cmd_set_file_stack_primary`.

- [ ] **Step 2: Run existing/focused tests and verify RED for new APIs only**

Run: `node scripts/test-remote-import-ui.cjs && node scripts/test-remote-import-batch.cjs`

Expected: existing single-link tests stay green; newly added batch-safe API assertions fail.

- [ ] **Step 3: Add the minimal additive hook controls**

Do not change backend acquisition commands. Preserve current single-link defaults and existing public return fields; add only the controls needed for the batch hook to prevent overlapping stack finalization.

- [ ] **Step 4: Run focused tests and production type build**

Run: `node scripts/test-remote-import-ui.cjs && node scripts/test-remote-import-batch.cjs`

Then: `cd app && npm run build`

Expected: both focused suites PASS and TypeScript/Vite build exits 0.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 3 evidence; no Git actions.

### Task 4: Durable Sequential Batch Orchestrator

**Files:**
- Create: `app/src/hooks/useRemoteImportBatch.ts`
- Modify: `app/src/remoteImportBatch.ts`
- Test: `scripts/test-remote-import-batch.cjs`

**Interfaces:**
- Consumes: Task 1/2 batch helpers and Task 3 single-job APIs.
- Produces `useRemoteImportBatch(...)` with:
  - `batches`, `initialized`
  - `analyzeBatch(rawInput: string): Promise<RemoteImportBatch>`
  - `setEntrySelections(batchId, entryId, selectedIds): Promise<void>`
  - `skipEntry(batchId, entryId): Promise<void>`
  - `retryEntryAnalysis(batchId, entryId): Promise<void>`
  - `groupMembers(batchId, memberRefs, primaryRef?): Promise<void>`
  - `ungroup(batchId, groupId): Promise<void>`
  - `startBatch(batchId): Promise<void>`
  - `retryBatchEntry(batchId, entryId): Promise<void>`
  - `cancelBatch(batchId): Promise<void>`

- [ ] **Step 1: Add failing orchestration/recovery tests**

Use pure/fake single-job adapters to pin:
- analysis calls occur entry 1, then 2, then 3, never concurrently;
- execution starts source N+1 only after source N reaches required terminal upload state;
- earlier completed entries remain completed if a later source fails;
- retry reuses existing job ID when one exists;
- known logical file IDs prevent reacquisition/reupload;
- cancelling a batch calls cancellation only for its active Remote Import job, never unrelated jobs;
- restart at entry N resumes from durable batch/job state rather than source 1.

- [ ] **Step 2: Run batch tests and verify RED**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: orchestration tests fail because `useRemoteImportBatch`/runner does not exist.

- [ ] **Step 3: Implement serial orchestration**

Persist every state transition before starting the next network/upload action. Treat the existing `RemoteImportJob` as the authoritative execution/outcome record and the batch as the ordered orchestration record.

- [ ] **Step 4: Run batch tests and production build**

Run: `node scripts/test-remote-import-batch.cjs`

Then: `cd app && npm run build`

Expected: serial ordering/recovery tests PASS and build exits 0.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 4 evidence; no Git actions.

### Task 5: Idempotent Cross-Source Version Stack Finalization

**Files:**
- Modify: `app/src/hooks/useRemoteImportBatch.ts`
- Modify: `app/src/remoteImportBatch.ts`
- Test: `scripts/test-remote-import-batch.cjs`

**Interfaces:**
- Consumes: `planCrossSourceStackFinalization()` and existing Version Stack Tauri commands.
- Produces restart-safe group finalization using persisted `stackId` and `stackedFileIds`.

- [ ] **Step 1: Add failing finalization tests**

Pin:
- two successful manual members create exactly one stack;
- 3+ members add only missing files after base stack creation;
- retry with persisted `stackId` never calls `cmd_create_file_stack` again;
- retry with persisted `stackedFileIds` never adds the same member twice;
- labels come from the selected output metadata where available;
- explicitly confirmed primary member is set with `cmd_set_file_stack_primary`;
- a failed member leaves successful uploaded files safe and group state partial/retryable;
- unrelated ungrouped outputs remain standalone.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `node scripts/test-remote-import-batch.cjs`

Expected: FAIL on missing durable group-finalization behavior.

- [ ] **Step 3: Implement group finalization**

Persist `stackId` immediately after creation and persist each `stackedFileId` immediately after successful addition. Use existing Version Stack commands only; do not add a backend stack system.

- [ ] **Step 4: Run focused tests and build**

Run: `node scripts/test-remote-import-batch.cjs && node scripts/test-remote-import-ui.cjs`

Then: `cd app && npm run build`

Expected: all focused tests PASS and build exits 0.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 5 evidence; no Git actions.

### Task 6: Multi-Link UI and Current-Channel Wiring

**Files:**
- Modify: `app/src/components/desktop/dashboard/RemoteUploadModal.tsx`
- Modify: `app/src/components/desktop/DesktopDashboard.tsx`
- Modify: `scripts/test-remote-import-ui.cjs`

**Interfaces:**
- Consumes: existing single-link hook plus `useRemoteImportBatch`.
- Produces one Remote Import modal with mode derived from parsed input count: 1 source = existing single-link path; 2+ sources = batch path.

- [ ] **Step 1: Add failing UI contract tests**

Assert:
- textarea/multiline input can hold multiple links;
- 2+ nonblank lines activate multi-link mode;
- one link still uses existing single-link path and default recommendation;
- multi-link cards render in original source order;
- no multi-link checkbox is checked from backend recommendation/default data until the user clicks it;
- each card owns independent `selectedIds`;
- direct-file rows are selectable in multi-link mode rather than implicitly selected;
- `Group as versions` is enabled only when 2+ selected eligible outputs are explicitly chosen for grouping;
- group/ungroup controls are visible but no automatic cross-link grouping text/behavior exists;
- no `<select>`/channel list/destination override appears;
- footer has one Import action for the batch;
- progress exposes current entry / total entries and per-entry status.

- [ ] **Step 2: Run UI tests and verify RED**

Run: `node scripts/test-remote-import-ui.cjs`

Expected: existing single-link assertions remain green; new multi-link UI assertions fail.

- [ ] **Step 3: Implement compact mode-switching UI**

Keep one modal. Reuse current visual language and existing current-channel subtitle. Do not create another dashboard page or channel picker.

- [ ] **Step 4: Run UI/model tests and production build**

Run: `node scripts/test-remote-import-ui.cjs && node scripts/test-remote-import-batch.cjs`

Then: `cd app && npm run build`

Expected: focused suites PASS and build exits 0.

- [ ] **Step 5: Record a non-Git checkpoint**

Append Task 6 evidence; no Git actions.

### Task 7: Full Regression and Scope Verification

**Files:**
- Modify only focused tests if a real regression exposes a missing contract.
- Do not expand product scope during this task.

**Interfaces:**
- Consumes: completed Tasks 1–6.
- Produces verification evidence only.

- [ ] **Step 1: Run focused Remote Import suites**

Run: `node scripts/test-remote-import-batch.cjs && node scripts/test-remote-import-ui.cjs && python3 scripts/test-remote-import-packaging.py`

Expected: all PASS.

- [ ] **Step 2: Run Remote Import Rust tests and compile check**

Run from `app/src-tauri`: `cargo test --lib remote_import -- --nocapture && cargo check`

Expected: all Remote Import Rust tests PASS; `cargo check` exits 0.

- [ ] **Step 3: Run full Rust regression**

Run from `app/src-tauri`: `cargo test --lib`

Expected: 0 failed.

- [ ] **Step 4: Run existing focused frontend regression**

Run: `node scripts/test-focused-ui.cjs`

Expected: 0 failures, including Device Continuity and Version Stack contracts.

- [ ] **Step 5: Run production frontend build**

Run from `app`: `npm run build`

Expected: TypeScript and Vite exit 0.

- [ ] **Step 6: Run native Git diff safety check**

Run RepoTunnel `git_diff_check(staged=false)`.

Expected: no whitespace/conflict-marker issues.

- [ ] **Step 7: Run full TeraRelay prebuild QA**

Run: `bash scripts/prebuild-qa.sh`

Expected: final line `PASS TeraRelay prebuild QA`. The known isolated-runtime WebKit playback/seek skip is acceptable only when it is the existing `/var/tmp`/audio-runtime limitation and decoder/range/UI checks still execute.

- [ ] **Step 8: Audit changed paths against scope**

Verify no new Multi-Link implementation edits landed in Device Continuity, Drive crypto/metadata/mount, TDLib auth/session, sharing, or unrelated dashboard/feed files. Distinguish pre-existing working-tree changes from this feature's changes.

- [ ] **Step 9: Stop without Git mutations**

Report implementation and test evidence to the user. Do not stage/commit/push/release unless the user separately authorizes that exact action.
