# TeraRelay Drive Dedicated Storage Pool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all new TeraRelay Drive writes onto one TeraRelay-managed private Drive storage pool, keep Drive folders virtual, preserve legacy Drive data, aggregate many 16 MiB crypto blocks into larger Telegram documents, and expose effectively unlimited cloud capacity without weakening local staging safety.

**Architecture:** Add a Drive-only storage registry and provisioning state that never reuses normal TeraRelay channels. New-format Drive files use additive Drive-specific object/part metadata, with legacy `logical_files`-backed Drive entries still readable. Mounted writes remain bounded by the existing 256 MiB staging budget, so the first safe implementation groups consecutive 16 MiB crypto blocks into the largest crash-safe remote part that fits the staging/scratch budget; the metadata format is channel-per-part and therefore supports future ~2 GB parts and mid-file channel rollover without changing the user-visible file.

**Tech Stack:** Rust, Tauri 2, Grammers/MTProto, SQLite, FUSE (`fuser`), existing Actix localhost range streaming, XChaCha20-Poly1305 Drive encryption, React/TypeScript for capacity wording only if required.

**Spec:** `docs/superpowers/specs/2026-10-08-terarelay-drive-storage-pool-design.md`

## Global Constraints

- Do not change authentication, login, Telegram session restore, API credentials, TDLib authorization, or unrelated transfer code.
- Do not reuse user-created normal TeraRelay channels for new Drive uploads.
- Do not delete or silently migrate existing Drive data stored in old backing channels.
- Existing Drive files must remain readable after the new storage-pool migration.
- Existing normal TeraRelay channels, files, sharing, folder groups, and transfer behavior must remain unchanged.
- Creating/renaming/moving Drive folders creates zero Telegram channels.
- Creating individual Drive files creates no new channel after the one shared Drive storage channel exists.
- Concurrent first-time Drive writes must converge on one Drive storage channel.
- FloodWait, temporary RPC failures, timeouts, offline state, and ordinary upload errors never trigger channel rotation.
- Schema changes must be additive/idempotent; no destructive migration of existing Drive rows.
- Encrypted Drive data keeps the existing no-plaintext-disk-cache invariant.
- Keep the current 256 MiB mounted-write staging budget unless a test proves a safe bounded change is required.
- Do not stage, commit, or push any implementation changes unless the user explicitly asks later. Use diff/test checkpoints instead.

## Review Focus

- **Remote channel created but DB persistence fails:** the next attempt must rediscover the uniquely marked Drive channel rather than create a duplicate. Covered in Task 1 recovery tests.
- **Two first uploads start together:** only one provisioning action may create/register the channel. Covered in Task 1 concurrency tests.
- **Crash after a remote part is sent but before final file publication:** restart must reuse recorded complete parts and never expose a partial Drive file. Covered in Task 4 recovery tests.
- **Legacy file and new-format file coexist in one folder:** both must list/read/rename/trash correctly without confusing metadata backends. Covered in Task 5 compatibility tests.
- **Range read crosses remote-part and crypto-block boundaries:** returned plaintext must be byte-exact and encrypted cache must remain ciphertext-only. Covered in Task 5 read tests.

---

## File Structure

- **Create:** `app/src-tauri/src/drive_storage.rs` — Drive-only channel registry, channel identity/marker parsing, active-channel selection, serialized provisioning, rediscovery, conservative rotation API.
- **Modify:** `app/src-tauri/src/lib.rs` — register `drive_storage` module/state only; no auth changes.
- **Modify:** `app/src-tauri/src/commands/drive_metadata.rs` — additive Drive V2 tables/records, stop auto-importing normal channels for new Drive layout, legacy/new snapshot union, new pending/object/part helpers.
- **Modify:** `app/src-tauri/src/drive_stream.rs` — aggregate internal crypto blocks into Drive remote parts, upload/finalize/cancel/recovery against Drive storage channels.
- **Modify:** `app/src-tauri/src/drive_mount.rs` — allocate storage lazily, group completed blocks safely, fetch/read new-format parts, preserve legacy read path, virtual capacity reporting, stale-mount behavior verification.
- **Modify only if needed:** `app/src/App.tsx` or Drive settings UI — show `Unlimited cloud storage`; do not redesign unrelated UI.
- **Tests:** colocated Rust unit/integration tests in the above modules plus the existing `scripts/prebuild-qa.sh` and transfer-recovery suites.

### Task 1: Drive-only storage registry and safe single-channel provisioning

**Files:**
- Create: `app/src-tauri/src/drive_storage.rs`
- Modify: `app/src-tauri/src/lib.rs`
- Test: `app/src-tauri/src/drive_storage.rs` test module

**Interfaces:**
- Produces `DriveStorageState::new() -> DriveStorageState` with an internal async provisioning mutex.
- Produces `init_drive_storage_schema(conn: &sqlite::Connection) -> Result<(), String>`.
- Produces `active_storage_channel(conn: &sqlite::Connection) -> Result<Option<DriveStorageChannel>, String>`.
- Produces `ensure_active_storage_channel(app: &tauri::AppHandle, state: &TelegramState, db: &DbConnection, storage: &DriveStorageState) -> Result<DriveStorageChannel, String>`.
- Produces `is_drive_storage_channel(title: &str, about: &str) -> bool` and deterministic generation naming.
- Later tasks consume only these interfaces, not normal `logical_channels`.

- [ ] **Step 1: Write failing registry tests**
  - schema initialization is idempotent;
  - newest `active` Drive storage row is selected;
  - `retired`/`read_only` rows are never selected for writes;
  - the Drive marker is distinct from `[terarelay-folder]`.

- [ ] **Step 2: Run focused tests and verify RED**
  - Run: `cargo test --lib drive_storage::tests -- --nocapture`
  - Expected: FAIL because `drive_storage` does not exist.

- [ ] **Step 3: Implement the additive registry and identity helpers**
  - Table fields: generation, backing channel ID UNIQUE, state, title, created/updated timestamps, optional retirement reason.
  - No writes to `folder_metadata` or normal `logical_channels`.

- [ ] **Step 4: Write failing provisioning/concurrency tests around an injected fake creator/discoverer**
  - 10 concurrent callers perform one creation and all receive the same channel ID;
  - simulated “remote create succeeded, DB write failed” is recovered by rediscovery on retry;
  - transient/FloodWait-class errors return an error and do not invoke rotation/second creation.

- [ ] **Step 5: Verify RED, then implement serialized `ensure_active_storage_channel`**
  - Re-check DB after locking.
  - Rediscover uniquely marked owned Drive storage channel before any new create.
  - Create private broadcast channel with TTL disabled using the existing connected Grammers client and peer cache, without modifying auth/session code.

- [ ] **Step 6: Run focused tests and diff check**
  - `cargo test --lib drive_storage::tests -- --nocapture`
  - `cargo fmt -- --check`
  - `git diff --check`

### Task 2: Decouple Drive namespace from normal user channels while preserving legacy data

**Files:**
- Modify: `app/src-tauri/src/commands/drive_metadata.rs`
- Test: same module tests

**Interfaces:**
- Produces additive new-format records: `DriveObjectRecord`, `DriveObjectPartRecord`, `DriveEntryV2Record`, and pending block/part records.
- Produces `create_drive_pending_file_v2(...)` that does not select a normal logical channel and starts with storage unassigned until first remote upload.
- Adds an additive pending `format_version` discriminator (`1` legacy, `2` storage-pool) so startup/finalization never routes old unfinished work through the new path.
- Produces snapshot lookup helpers that union legacy `drive_file_entries` and new V2 entries.
- Extends the existing Drive metadata manifest/sync/merge/apply path with serde-defaulted Drive-storage registry/object/part/V2-entry records so another device can reconstruct new-format files; legacy schema versions remain accepted.

- [ ] **Step 1: Write failing namespace-separation tests**
  - creating 1,000 virtual directories adds zero rows to Drive storage channels and zero normal logical channels;
  - adding a normal user logical channel after Drive initialization does not auto-create a Drive root folder for new layouts;
  - already-existing legacy imported Drive directories remain readable/listable.

- [ ] **Step 2: Verify RED**
  - Run the three exact tests with `cargo test --lib commands::drive_metadata::tests::<name> -- --nocapture`.

- [ ] **Step 3: Add V2 tables and records additively**
  - `drive_objects`: object ID, plaintext size, encryption version, crypto ID, fingerprint, MIME, created time, status.
  - `drive_object_parts`: object ID, part index, backing channel ID, message ID, ciphertext start/size, first crypto block, block count, SHA-256.
  - `drive_entries_v2`: entry ID, object ID, directory ID, display name, timestamps, tombstone.
  - `drive_pending_blocks`: pending ID, block index, plaintext hash, encoded length, completion state.
  - `drive_pending_parts`: pending ID, part index, backing channel ID, message ID, first block, block count, ciphertext size/hash, upload timestamp.
  - Add `format_version` to `drive_pending_files` through an idempotent column check/ALTER; existing rows default to legacy format 1 and new storage-pool writes use 2.

- [ ] **Step 4: Write failing metadata-sync compatibility tests, then extend manifest merge/apply**
  - a new-format manifest round-trips storage channel/object/part/V2-entry records;
  - legacy schema 1/2 manifests decode with empty V2 collections;
  - record-level merge cannot erase a known storage channel or completed object/part;
  - offline local mutation keeps Drive `dirty=1` until the existing sync path succeeds.

- [ ] **Step 5: Stop normal-channel auto-import for new Drive state**
  - Preserve existing legacy imported rows already present in Drive metadata.
  - Do not call normal-channel import as a mechanism for new Drive storage selection.
  - Keep legacy snapshot/read helpers intact.

- [ ] **Step 6: Implement V2 pending/object/part CRUD and union snapshot helpers**
  - New pending creation has no dependency on `default_backing_logical_channel`.
  - Name collision, rename, move, trash, restore continue to work across legacy and V2 entries.
  - Legacy pending rows (`format_version=1`) remain routed to the existing finalization/recovery path; new rows (`format_version=2`) use V2 only.

- [ ] **Step 7: Run drive_metadata tests and diff check**
  - `cargo test --lib commands::drive_metadata::tests -- --nocapture`
  - `cargo fmt -- --check`
  - `git diff --check`

### Task 3: Aggregate 16 MiB crypto blocks into bounded Drive remote parts

**Files:**
- Modify: `app/src-tauri/src/drive_stream.rs`
- Modify: `app/src-tauri/src/drive_mount.rs`
- Test: both modules

**Interfaces:**
- Produces `remote_part_block_plan(...)` mapping consecutive 16 MiB blocks into a bounded remote part.
- Produces `upload_pending_part(...) -> Result<DrivePendingPartRecord, String>` using `ensure_active_storage_channel`.
- Keeps `DRIVE_PLAINTEXT_CHUNK_SIZE = 16 MiB` unchanged.

- [ ] **Step 1: Write failing part-planning tests**
  - full internal blocks coalesce into one remote part;
  - the planned part never exceeds the safe staging/scratch bound;
  - a final short block is included only when the pending file is closed;
  - a 10 TB logical size produces a representable part plan without integer overflow or artificial file-size rejection.

- [ ] **Step 2: Verify RED**
  - `cargo test --lib drive_stream::tests -- --nocapture`

- [ ] **Step 3: Implement bounded aggregation**
  - Keep plaintext staging bounded.
  - Build one remote upload part from consecutive crypto blocks; do not create one Telegram message per 16 MiB block.
  - Use a safe mounted-write part size derived from the 256 MiB staging budget and scratch overhead; metadata format remains independent of part size so source-backed uploads can later use ~2,000,000,000-byte parts without schema changes.

- [ ] **Step 4: Write failing upload-location tests**
  - every pending part records `backing_channel_id + message_id + part_index`;
  - successive files reuse the same active storage channel;
  - changing virtual folders does not alter the selected Drive storage channel;
  - simulated permanent rollover can place later parts on another channel while keeping one logical object.

- [ ] **Step 5: Verify RED, implement upload/location persistence, then GREEN**
  - Persist a part only after Telegram send succeeds.
  - Clear local plaintext ranges/delete local block files only after the containing remote part is durably recorded.
  - Never rotate for FloodWait/transient errors.

- [ ] **Step 6: Run focused suites and diff check**
  - `cargo test --lib drive_stream::tests drive_mount::linux::staging_budget_tests -- --nocapture`
  - `cargo fmt -- --check`
  - `git diff --check`

### Task 4: Crash-safe finalization, dedup, cancellation, and restart recovery

**Files:**
- Modify: `app/src-tauri/src/drive_stream.rs`
- Modify: `app/src-tauri/src/commands/drive_metadata.rs`
- Modify: `app/src-tauri/src/drive_mount.rs`
- Test: colocated Rust tests

**Interfaces:**
- Produces `finalize_pending_v2(...) -> Result<String, String>`.
- Produces recovery helpers that classify recorded complete parts vs local/unuploaded blocks.

- [ ] **Step 1: Write failing recovery tests**
  - part recorded + app restart resumes without re-uploading that part;
  - Telegram send failure leaves the local block data/ranges intact;
  - incomplete pending object is never visible as a completed file;
  - cancellation deletes only messages owned by that pending write and leaves unrelated Drive objects untouched.

- [ ] **Step 2: Verify RED**

- [ ] **Step 3: Implement V2 finalization**
  - Validate complete ordered block coverage and ordered remote parts.
  - Compute existing content fingerprint from per-block plaintext hashes.
  - Reuse verified duplicate objects without uploading a second completed object.
  - Publish `drive_objects + drive_entries_v2` transactionally only after all parts validate.

- [ ] **Step 4: Implement restart/cancel cleanup**
  - Resume recorded parts.
  - Remove only temporary local state after successful publication/cancellation.
  - Keep orphan-remote cleanup best-effort and scoped to recorded pending message IDs.

- [ ] **Step 5: Run recovery and existing transfer-recovery tests**
  - focused Rust recovery tests;
  - `node scripts/test-transfer-recovery.cjs`.

### Task 5: New-format reads, copy-on-write edits, and legacy compatibility

**Files:**
- Modify: `app/src-tauri/src/drive_mount.rs`
- Modify: `app/src-tauri/src/commands/drive_metadata.rs`
- Test: colocated Rust tests

**Interfaces:**
- Produces `fetch_v2_cipher_range(...)` that maps object ciphertext offsets across `drive_object_parts` and their channel/message IDs.
- Existing legacy `fetch_stream_range(...)` remains for legacy logical-file-backed Drive entries.

- [ ] **Step 1: Write failing byte-range tests**
  - request entirely inside one part;
  - request crosses two remote parts;
  - encrypted 16 MiB block crossing a part boundary decrypts exactly;
  - cache files for encrypted V2 objects contain ciphertext, never returned plaintext.

- [ ] **Step 2: Verify RED**

- [ ] **Step 3: Implement V2 range resolver and read path**
  - Fetch only required channel/message ranges.
  - Authenticate each encrypted crypto block with the existing master key/crypto ID.
  - Keep legacy path unchanged.

- [ ] **Step 4: Write failing coexistence/edit tests**
  - legacy + V2 entries list together in one directory;
  - rename/move is metadata-only for V2;
  - copy-on-write edit of V2 preserves old object until replacement finalizes;
  - trash/restore collisions remain safe.

- [ ] **Step 5: Implement V2 lookup/edit/trash/restore integration and run tests**
  - `cargo test --lib drive_mount commands::drive_metadata::tests -- --nocapture`

### Task 6: Unlimited capacity UX, stale-mount recovery, and full regression verification

**Files:**
- Modify: `app/src-tauri/src/drive_mount.rs`
- Modify only if necessary: `app/src/App.tsx` or Drive settings component
- Test: existing Rust/UI/prebuild suites

**Interfaces:**
- FUSE `statfs` reports a huge virtual cloud capacity independent of local staging disk.
- Local writes still enforce the actual 256 MiB staging budget and return `ENOSPC` when local staging cannot accept more bytes.

- [ ] **Step 1: Write failing capacity tests**
  - virtual capacity is much larger than local staging;
  - staging budget remains unchanged/enforced;
  - inode/block calculations do not overflow 64-bit statfs fields.

- [ ] **Step 2: Verify RED and implement virtual capacity reporting**
  - Do not claim a numeric Telegram quota in UI; wording is `Unlimited cloud storage`.

- [ ] **Step 3: Add/adjust stale-mount regression test**
  - stale `ENOTCONN`/`EIO` mount is detected and safely detached/re-mounted by the existing mount lifecycle;
  - no duplicate healthy mount is created.

- [ ] **Step 4: Run complete Rust suite**
  - `cargo test --lib`
  - Expected: all tests pass; report exact count.

- [ ] **Step 5: Run formatting/source/frontend/full prebuild QA**
  - `cargo fmt -- --check`
  - `bash scripts/check-source.sh`
  - `npm run build` in `app`
  - `bash scripts/prebuild-qa.sh`
  - Preserve and report any environment-only SKIP exactly; do not call skipped real Telegram/WebKit behavior proven.

- [ ] **Step 6: Run cautious real integration QA only if the existing isolated QA app/session can be reused without duplicate instances**
  - safely recover the stale QA FUSE mount;
  - create nested virtual folders and verify no channel is created for them;
  - copy/read a file through Nemo;
  - verify first real Drive upload provisions at most one dedicated Drive channel and a second upload reuses it;
  - restart/reconnect the existing QA app and verify data/mount recovery;
  - verify the Drive storage channel does not appear in normal TeraRelay folder UI;
  - do not force a rotation test that would create a second real Telegram channel unless the user separately approves that destructive/external-account side effect.

- [ ] **Step 7: Final safety inspection without commit/push**
  - `git diff --check`
  - inspect `git status --short` and changed-file list;
  - verify no auth files/functions were modified beyond pre-existing unrelated work;
  - verify no staged changes were created by this implementation;
  - do not commit or push.
