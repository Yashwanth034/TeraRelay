# TeraRelay Drive Dedicated Storage Pool Design

Date: 2026-10-08
Status: Proposed for implementation
Scope: TeraRelay Drive backend storage, Drive mount capacity reporting, and Drive-specific verification only

## 1. Goal

TeraRelay Drive must behave like one seamless cloud drive while using Telegram only as an internal storage transport.

User-visible folders must remain virtual metadata. Creating folders must never create Telegram channels. Creating files must never create Telegram channels. All new Drive uploads must use a TeraRelay-managed private Drive storage pool that is separate from the user's normal TeraRelay channels.

The initial pool contains one dedicated private Telegram channel. TeraRelay reuses that channel for all Drive uploads. A later channel is created only when the current Drive storage channel is permanently unusable or a proven storage rollover condition requires it. FloodWait, transient network failures, rate limits, ordinary upload errors, and user folder creation must never trigger new channel creation.

Very large files remain one logical file to the user. TeraRelay may split the remote representation into Telegram-safe parts internally and reconstruct or stream them transparently.

## 2. Safety invariants

The implementation must preserve all of the following invariants:

1. Do not change authentication, login, Telegram session restore, API credentials, or TDLib authorization behavior.
2. Do not reuse user-created normal TeraRelay channels for new Drive uploads.
3. Do not delete or silently migrate existing Drive data stored in old backing channels.
4. Existing Drive files remain readable after the new storage-pool migration.
5. Existing normal TeraRelay channels, files, sharing, folder groups, and transfer behavior remain unchanged.
6. Creating or renaming virtual Drive folders creates zero Telegram channels.
7. Creating individual files creates zero Telegram channels beyond the one shared Drive storage channel that may be lazily provisioned for the pool.
8. Concurrent first-time Drive uploads must converge on exactly one newly created Drive storage channel.
9. FloodWait and transient Telegram failures must wait/retry or surface a retryable error; they must never create another storage channel.
10. No destructive database migration. New schema must be additive and idempotent.
11. New Drive data must be encrypted using the existing Drive encryption rules whenever Drive encryption is enabled.
12. Failed uploads must not publish a completed Drive entry.
13. A completed Drive entry must reference only fully uploaded and validated remote parts.
14. Local staging/cache limits remain finite and protected even when the Drive advertises effectively unlimited cloud capacity.
15. Stale legacy Drive layouts remain supported until an explicit future migration removes them.

## 3. User-visible behavior

The user sees one filesystem tree:

TeraRelay Drive/
- Projects/
- Movies/
- Backups/
- Photos/
- any number of nested folders

The user does not select a Telegram channel for Drive uploads and does not need to know which backend storage channel contains a file.

A 10 TB file is displayed as one 10 TB file. The remote representation may consist of thousands of Telegram-sized parts.

The TeraRelay Drive UI should describe capacity as "Unlimited" or "Unlimited cloud storage". The Linux FUSE filesystem cannot literally report an infinite integer capacity, so it will report a very large virtual capacity while separately enforcing real local staging limits.

## 4. Storage architecture

### 4.1 Separate Drive storage channels

Drive-managed Telegram channels are separate from the existing user-facing `folder_metadata` / normal `logical_channels` collection.

Introduce a Drive-specific registry, conceptually:

`drive_storage_channels`
- `id` / generation
- `backing_channel_id`
- `state` (`active`, `retired`, `read_only`)
- `created_at`
- `updated_at`
- optional retirement reason

The currently writable storage channel is the newest `active` Drive storage channel.

Drive storage channels must not appear as normal TeraRelay folders in the standard folder/channel UI.

### 4.2 Lazy provisioning

The first dedicated Drive channel is created only when a real Drive write needs remote storage and no active Drive storage channel exists.

Channel creation must use a Drive-specific title/about marker so TeraRelay can rediscover it after reinstall/restart without confusing it with user-created TeraRelay folders.

Suggested identity:
- title: `TeraRelay Drive Storage`
- later generations: `TeraRelay Drive Storage 002`, etc.
- about marker distinct from `[terarelay-folder]`, e.g. `[terarelay-drive-storage]`

Drive storage channels are private broadcast channels with history TTL disabled, matching the durable storage behavior already used by TeraRelay-owned storage channels.

### 4.3 Concurrency control

First-channel provisioning must be serialized in process and must re-check the database after acquiring the creation lock.

Flow:
1. caller requests active Drive storage channel
2. check DB for active channel
3. if absent, acquire Drive-storage provisioning mutex
4. re-check DB
5. if still absent, create exactly one Telegram channel
6. persist registry row transactionally
7. return the persisted channel

If channel creation succeeds remotely but local persistence fails, recovery must rediscover the uniquely marked Drive storage channel rather than creating another one blindly.

### 4.4 Channel rotation

Rotation is intentionally conservative.

A new Drive storage channel may be provisioned only when the current channel is known to be permanently unsuitable for writes or a separately defined, tested rollover threshold is reached.

Do not rotate on:
- FloodWait
- timeout
- offline state
- temporary RPC/server errors
- ordinary upload failure
- one failed chunk
- application restart

When rotation is needed, existing files remain where they are. New writes use the newly active channel. Old channels become `read_only`/`retired` but remain part of the pool for reads and recovery.

## 5. Large-file remote layout

### 5.1 Keep 16 MiB crypto blocks internal

The existing 16 MiB Drive plaintext chunk size remains useful for:
- bounded write staging
- authenticated encryption
- random-access read/decrypt
- crash recovery

It must no longer imply one Telegram message per 16 MiB block.

### 5.2 Aggregate remote Telegram parts

New Drive writes aggregate consecutive encrypted blocks into large Telegram-safe remote parts, targeting the existing TeraRelay split size of 2,000,000,000 bytes where practical.

The exact remote part size must stay below Telegram's accepted per-document limit after encryption/container overhead.

The implementation must not require materializing the full logical file locally. It may stream a sequence of staged encrypted blocks into one remote part and release local block data after the corresponding remote part is durably recorded.

### 5.3 Per-part location metadata

Drive must record the remote location of every large part so a logical file can be reconstructed even if future parts reside in another Drive storage channel.

Conceptual record:

`drive_file_parts`
- `file_id`
- `part_index`
- `backing_channel_id`
- `message_id`
- `ciphertext_size`
- `sha256`
- optional crypto-block range metadata

Pending equivalents may be stored in an additive `drive_pending_remote_parts` table until finalization.

A completed file is published only when all expected remote parts are present, ordered, and hash-validated in metadata.

### 5.4 Cross-channel logical files

The Drive layer, not the normal user-channel logical-file layer, owns cross-channel file reconstruction.

Do not force normal `logical_files` semantics to treat user channels as a Drive pool. Instead, keep Drive-specific part locations and use a Drive-specific read path that can fetch byte ranges from the required backing channel/message parts.

This isolates the new feature from user-facing logical channels and prevents unrelated behavior changes.

## 6. Read and streaming path

FUSE reads continue to expose normal byte ranges.

For a completed new-format Drive file:
1. map requested plaintext byte range to encrypted crypto-block range
2. map encrypted range to one or more Drive remote parts
3. fetch only required remote ranges from the correct Drive storage channel/message
4. authenticate/decrypt the necessary crypto blocks
5. return requested plaintext bytes
6. cache only ciphertext for encrypted files, preserving the existing no-plaintext-cache invariant

Legacy Drive files continue using the existing legacy logical-stream path.

The new read path must support a logical file whose remote parts cross storage-channel generations.

## 7. Metadata and backward compatibility

### 7.1 Additive schema only

The migration creates new Drive storage registry and remote-part tables with `CREATE TABLE IF NOT EXISTS` / idempotent schema checks.

Do not rewrite or delete existing `drive_file_entries`, `logical_files`, `logical_file_chunks`, or old backing-channel references during startup.

### 7.2 Legacy file support

Existing Drive entries that lack new Drive-part records continue to resolve through the current legacy backing-channel/logical-file implementation.

New files use the new storage-pool layout.

Edits to an old file use copy-on-write and may produce a new-format replacement while leaving the original bytes untouched until the replacement is fully finalized.

### 7.3 Existing pending writes

Startup recovery must distinguish legacy pending writes from new-format pending writes. Existing closed legacy pending work must remain recoverable by the current code path. New writes use the new pending remote-part path.

No migration may discard an unfinished pending write.

## 8. Hiding Drive storage channels from normal TeraRelay folders

The existing folder scanner currently recognizes `[TR]`, `[TD]`, and `[terarelay-folder]` channels as user-facing storage folders.

Drive-managed channels must use a distinct marker and must be explicitly excluded from normal folder discovery/import.

`ensure_imported_layout()` must not import Drive storage channels as top-level virtual Drive folders.

The user should see only the Drive namespace, not `TeraRelay Drive Storage 001` inside ordinary TeraRelay folders.

## 9. FUSE capacity reporting

The current `statfs()` reports local staging-disk capacity. That is misleading as the apparent cloud capacity.

For the mounted Drive:
- report a very large stable virtual filesystem capacity suitable for desktop file managers
- do not use that virtual number to decide whether local staging is safe
- keep the existing staging-budget checks authoritative
- return `ENOSPC` only when the real bounded local staging budget cannot accept additional pending bytes

TeraRelay UI wording should be "Unlimited" / "Unlimited cloud storage" rather than a fabricated precise Telegram quota.

## 10. Failure handling

### Channel creation fails
Return a clear retryable error. Do not fall back to a normal user channel.

### FloodWait / rate limit
Honor the wait/retry behavior already used by the Telegram layer. Never create another channel to bypass it.

### Upload of one remote part fails
Keep already completed remote parts and pending metadata for resume. Do not publish the Drive file as complete.

### App crashes after remote upload but before DB commit
Recovery reconciles remote marked data / pending metadata before starting a duplicate upload where possible.

### DB commit succeeds but metadata sync is offline
Keep local Drive state dirty and retry metadata sync later, following the existing Drive sync model.

### Current storage channel becomes permanently unwritable
Mark it non-active only after a classified permanent failure or explicit rollover decision. Provision a new active Drive storage channel. Existing files on the old channel remain readable.

### New channel creation is refused by Telegram/account policy
Stop safely and surface the error. Never delete old channels or move existing data destructively.

## 11. Implementation boundaries

Expected primary files:
- `app/src-tauri/src/commands/drive_metadata.rs`
- `app/src-tauri/src/drive_stream.rs`
- `app/src-tauri/src/drive_mount.rs`
- possibly one new focused Drive storage module, preferred over making existing large files significantly harder to maintain
- `app/src-tauri/src/lib.rs` / command registration only if needed for Drive-specific integration
- Drive-specific tests

Shared `commands/fs.rs` should only be touched when reusing an existing safe helper is necessary. Authentication files are out of scope.

Normal user-channel behavior, sharing, folder groups, and unrelated UI code are out of scope.

## 12. Verification requirements

Implementation is not complete until all applicable checks pass.

### Unit/regression tests

Add tests proving at minimum:
1. 1,000 virtual Drive folders create zero Drive storage channels.
2. Multiple files reuse one active Drive storage channel.
3. Concurrent first-use provisioning produces one channel registration, not duplicates.
4. Drive storage channels are excluded from ordinary folder discovery/import.
5. FloodWait/transient failure classification never rotates channels.
6. Channel rotation preserves old channel records for reads.
7. New large-file aggregation uses large remote parts rather than one Telegram message per 16 MiB crypto block.
8. Part ordering and hashes detect missing/reordered/corrupt metadata.
9. A logical file can span multiple Drive storage channels in metadata and still map ranges correctly.
10. Encrypted range reads return exact plaintext while disk cache contains ciphertext only.
11. Legacy Drive entries remain readable with no new part rows.
12. Legacy pending writes are not discarded by schema migration/recovery.
13. New file finalization is atomic from the Drive namespace perspective.
14. Virtual `statfs` capacity does not disable real staging-budget enforcement.

### Existing project checks

Run the existing TeraRelay verification flow after targeted tests:
- `cargo fmt -- --check`
- relevant Rust unit tests
- frontend build if frontend code changes
- transfer recovery tests if shared transfer code changes
- source hygiene check
- `git diff --check`
- full `scripts/prebuild-qa.sh`

The prebuild result must continue to distinguish simulated/loopback transport tests from real Telegram verification.

### Real integration QA when available

Using the existing isolated QA application/mount and without creating duplicate app instances:
- recover stale QA mount safely
- create nested folders
- copy a file through Nemo
- verify only one Drive storage channel exists
- verify repeated uploads reuse it
- read file back through the mount
- seek/read ranges on a larger file
- restart the app and verify mount/data recovery
- verify Drive storage channel is absent from normal user folder UI

Real Telegram channel creation/upload tests must be done cautiously and should create the minimum possible number of channels: one Drive storage channel unless a deliberate rotation test is being performed.

## 13. Completion criteria

This design is complete when:
- new Drive uploads never depend on existing user-created TeraRelay channels
- virtual folders never create Telegram channels
- files reuse one dedicated Drive storage channel
- large files use aggregated Telegram-sized remote parts while remaining one logical file
- cross-channel part locations are representable for safe future rollover
- old Drive files/pending writes remain intact and readable/recoverable
- Drive storage channels stay hidden from normal TeraRelay channel/folder UI
- FloodWait/transient failures cannot cause channel proliferation
- FUSE presents effectively unlimited cloud capacity without weakening local staging safeguards
- targeted Drive tests and the existing full TeraRelay QA pass
- authentication/session code remains untouched
