# TeraRelay Native Drive — Windows + macOS Design

## Goal

Make TeraRelay appear as a first-class cloud filesystem location in Windows Explorer and macOS Finder while reusing the existing Drive metadata, encrypted streaming, pending-write, upload, rename, move, delete, recovery, and Device Continuity logic.

This feature must not create a second storage model. Linux FUSE, Windows Cloud Files, and macOS File Provider must all project the same TeraRelay Drive state.

## User-visible result

### Windows

- TeraRelay appears as a registered cloud storage location in Explorer.
- Remote files appear immediately as placeholders without downloading their full content.
- Opening a file hydrates only the requested content through the existing TeraRelay stream path.
- Files copied into TeraRelay are staged through the existing bounded pending-write pipeline and uploaded normally.
- Rename, move, delete, create-folder, overwrite, and truncate update the existing Drive metadata model.
- Placeholder state reflects whether content is remote-only, hydrating, local, pending upload, or failed.
- TeraRelay can disconnect/reconnect the sync root without corrupting metadata or pending writes.

### macOS

- TeraRelay appears under Finder Locations through a native File Provider domain.
- Finder can enumerate remote folders/files without pre-downloading them.
- File materialization is on demand.
- Local writes, imports, moves, renames, and deletes flow into the same Drive mutation/pending-upload model used elsewhere.
- Remote metadata changes are signaled back to File Provider so Finder refreshes without recreating the domain.
- The provider fails safely when the TeraRelay background process is unavailable and resumes when it returns.

### Linux

- Existing FUSE behavior remains functional.
- No regression to current Linux read streaming, bounded write staging, encrypted read behavior, copy-on-write replacement, crash recovery, or unmount handling.

## Architecture decision

Use the operating systems' native cloud-storage integration instead of requiring third-party filesystem drivers.

### Windows: Cloud Files API (CfAPI)

Use Microsoft's Cloud Files platform through the `windows` Rust crate: register TeraRelay's unpackaged Win32/Tauri sync root with the desktop-supported `CfRegisterSyncRoot`, connect the runtime callback channel with `CfConnectSyncRoot`, and use Cloud Filter placeholders/fetch operations for hydration. Detect local creates/content edits with a recursive filesystem watcher (ReadDirectoryChangesW-backed) plus a deterministic reconnect/full-scan recovery pass, because CfAPI callbacks alone do not report every new/overwritten local file. Do not require MSIX/sparse-package identity merely to register the Drive; TeraRelay's existing MSI/NSIS distribution remains authoritative.

Reason:
- no WinFsp runtime installation;
- avoids adding GPL Rust bindings to TeraRelay's proprietary codebase;
- native Explorer cloud-file semantics;
- on-demand hydration and placeholder state are built into Windows;
- matches the behavior users expect from OneDrive-style providers.

The initial Windows result is a native Explorer sync root, not a synthetic block-device drive letter. A drive-letter alias is explicitly deferred because it adds little capability and would either depend on a third-party filesystem driver or introduce fragile path aliasing. The product requirement is first-class Explorer integration, not a fake disk abstraction.

### macOS: Replicated File Provider

Use Apple's `NSFileProviderReplicatedExtension` + `NSFileProviderEnumerating` model.

Reason:
- Apple documents this as the supported macOS cloud-storage integration;
- Finder owns local materialized copies and asks the provider for remote metadata/content;
- supports remote-change signaling and on-demand content;
- avoids macFUSE/kernel-extension installation and Apple Silicon reduced-security requirements.

The File Provider extension is a small Swift/Objective-C boundary adapter. TeraRelay storage rules remain in Rust.

## Rejected approaches

### WinFsp + macFUSE

Rejected as the default architecture.

- WinFsp Rust bindings currently use GPL licensing, which is a poor fit for TeraRelay's proprietary source license.
- macFUSE requires a separately installed system/kernel extension and is especially undesirable on Apple Silicon.
- It would create installer/runtime prerequisites that native cloud APIs avoid.

### Local WebDAV mount

Rejected.

- Windows WebDAV has client/service limitations that are unsuitable for TeraRelay's large-file goal.
- Semantics differ across Windows/macOS.
- It would be a compatibility transport, not a native cloud-files experience.

## Shared Rust filesystem core

The current `drive_mount.rs` contains both Linux/FUSE callback code and reusable Drive logic. Before adding new adapters, split reusable semantics from FUSE-specific response types.

Create an OS-independent module that owns:

- node identity and lookup;
- snapshots and directory enumeration;
- file attributes and sizes;
- remote ranged reads;
- encrypted chunk reads and authentication;
- cache handling;
- pending-file creation;
- copy-on-write replacement;
- random/scatter writes;
- truncate/resize;
- flush/finalization;
- mkdir;
- rename/move;
- unlink/delete;
- write recovery;
- stat/capacity values;
- error classification.

The shared core must expose plain Rust request/response types and `Result<_, DriveFsError>`. It must not depend on FUSE, Windows CfAPI types, Swift, Finder, or Explorer.

Linux's `fuser::Filesystem` implementation becomes an adapter translating FUSE calls/errors to the shared core.

## Core interface

The exact implementation can evolve during TDD, but the boundary must provide these capabilities:

- `snapshot()`
- `lookup(parent, name)`
- `list_directory(parent)`
- `metadata(node)`
- `read(node, offset, length)`
- `create_file(parent, name)`
- `create_directory(parent, name)`
- `write(node, offset, bytes)`
- `resize(node, size)`
- `flush(node)` / `close_write(node)`
- `rename(node, new_parent, new_name)`
- `remove_file(node)`
- `remove_directory(node)`
- `recover_pending()`

Node identities must be stable across adapters and restarts. Existing Drive entry IDs/directory IDs remain the source of truth; platform-specific placeholder identities encode or map to those IDs rather than inventing another canonical ID system.

## Windows adapter

Create a Windows-only Rust adapter around the shared core.

### Registration

- Choose a deterministic per-user sync-root directory under the user's normal profile/app data location.
- Register it with `CfRegisterSyncRoot` using TeraRelay's stable provider GUID/name/version, root identity, hydration/population/in-sync policies, and the app-owned root path. This keeps the existing unpackaged MSI/NSIS desktop app compatible with Cloud Files without introducing a sparse/MSIX identity package.
- Use hydration/population policies that permit placeholders and on-demand content.
- Registration is idempotent and update-safe; reconnect checks the existing registered root before changing it.
- Connect with `CfConnectSyncRoot` only after TeraRelay authentication/Drive continuity is ready.
- Start recursive local-change monitoring for create/content-write events and run a deterministic full reconciliation scan on reconnect to cover provider downtime/watcher loss.
- Disconnect the CfAPI callback channel and watcher cleanly during application shutdown/sign-out; unregister the persistent Explorer root only for explicit account/provider removal, not every process exit.

### Placeholder enumeration

- Enumerate the shared core snapshot.
- Create/update placeholders with stable file identity bytes containing the TeraRelay node identity/version.
- Do not replace a dirty local/pending file with a remote placeholder.
- Remote metadata reconciliation updates placeholder metadata without forcing full hydration.

### Hydration

- Handle `FETCH_DATA` callbacks.
- Validate callback identity/ranges.
- Read only the requested ranges through shared-core `read`.
- Return bytes using `CfExecute` transfer operations.
- Handle cancellation without continuing unnecessary network reads.
- Encrypted Drive remains fail-closed when locked.

### Local mutations

- New/modified local files detected by the root watcher become shared-core pending writes; a reconnect/full-scan pass catches changes made while the provider was not running or if watcher events were lost.
- CfAPI rename/delete notifications and watcher events are deduplicated before calling shared-core mutations.
- Overwriting/replacing a placeholder as a normal file is treated as a local edit and converted back to a placeholder only after the upload/metadata commit succeeds.
- Upload completion marks the platform item in sync.
- Failed uploads remain recoverable and visibly out of sync.
- Platform callbacks and watcher imports must never bypass the existing bounded staging budget.

## macOS adapter

Add a macOS File Provider app extension target bundled with the desktop app.

### Extension boundary

The File Provider process must not reimplement Telegram, encryption, or Drive metadata.

Use a narrow local IPC bridge to the running TeraRelay desktop/background process. The transport must be local-only, authenticated with a per-install random capability stored in the app-controlled shared container/keychain, and versioned.

The extension sends filesystem-domain requests such as:

- enumerate container;
- fetch item metadata;
- materialize file/range;
- create/import file;
- modify contents;
- create folder;
- rename/move;
- delete;
- enumerate changes.

Rust handles them through the same shared core used by Linux/Windows.

If the Rust host is unavailable, the extension returns a retryable service-unavailable error. It must never fabricate success or silently discard writes.

### Change tracking

Add a monotonically increasing Drive change token derived from existing metadata revisions plus a small local mutation journal if needed by File Provider enumeration semantics.

- `enumerateChanges` returns deterministic changes since an anchor.
- remote reconciliation advances/signals the provider;
- local writes are not echoed as conflicting remote changes;
- deleted nodes produce tombstones long enough for provider reconciliation.

### Packaging

- The `.appex` uses the same Team ID/signing chain as TeraRelay.
- Required File Provider/App Group entitlements are isolated to macOS packaging.
- The release workflow verifies the extension exists inside the app bundle and is validly code-signed before notarization/upload.

## Lifecycle and UI

`DriveMountStatus` becomes cross-platform rather than Linux-only.

It should report at minimum:

- `supported`
- `mounted` / `connected`
- `mount_point` or sync-root/provider location
- `read_streaming`
- `write_staging`
- `provider_kind` (`fuse`, `windows-cloud-files`, `macos-file-provider`)
- optional actionable setup/error state

Device Continuity should connect the platform provider only after initial metadata reconciliation/security classification, preserving the existing ordering guarantee.

Desktop Settings may show the location/provider state and a reconnect action, but this feature must not introduce a second Drive configuration model.

## Security requirements

- Existing encryption key rules remain unchanged.
- Never persist decrypted encrypted-file blocks merely to satisfy platform hydration.
- Platform placeholder identity is untrusted input and must be validated before mapping to Drive IDs.
- Windows callbacks and macOS IPC requests are range/path/size bounded.
- No arbitrary host path supplied by a platform callback may be used outside the registered root/staging directories.
- Pending write staging remains app-owned/private and bounded.
- macOS IPC must accept only local authenticated extension traffic.
- Sign-out/unmount must invalidate provider access without deleting user Drive metadata.

## Failure semantics

- Network loss: reads fail/retry cleanly; pending writes remain recoverable.
- Telegram unavailable: platform shows out-of-sync/error rather than losing local data.
- Drive locked: encrypted reads/writes that need the key fail with an actionable locked state.
- App crash: existing pending-write recovery restores unfinished uploads.
- Provider crash/restart: registration/domain remains reusable; reconnect is idempotent.
- Metadata conflict: shared Drive reconciliation remains authoritative; adapters never write directly around it.
- Cancellation: hydration or upload callbacks stop work when safe and must not partially mark an item in sync.

## Testing strategy

### Platform-neutral tests

Move the existing Drive filesystem behavior tests onto the shared core wherever possible so Linux/Windows/macOS semantics are proven once:

- lookup/list;
- ranged read;
- encrypted read/locked behavior;
- random write;
- copy-on-write replacement;
- truncate;
- rename/move;
- delete;
- directory operations;
- staging limits;
- recovery;
- conflict/error classification.

### Linux regression

Keep existing FUSE tests and QA mount test passing after extraction.

### Windows

Add Windows-target unit/contract tests for identity encoding, registration policy, callback mapping, range validation, cancellation, mutation mapping, and reconnect idempotence.

Release/CI must compile the actual Windows adapter on `windows-latest`. Where the runner supports CfAPI, add an integration smoke test that registers a temporary sync root, creates a placeholder, hydrates fixture bytes, and unregisters it.

### macOS

Add deterministic tests for IPC request/response contracts and change-anchor behavior in Rust.

On `macos-latest`, compile the File Provider extension, verify the embedded `.appex` metadata/entitlements/signature, and run host-side IPC contract tests. A full Finder interaction remains a platform integration test and must not be falsely claimed on a Linux development machine.

## Release requirements

- No new Windows third-party filesystem-driver installer dependency.
- No macFUSE/kernel-extension dependency.
- Windows native provider must be compiled in the normal Windows desktop release.
- macOS File Provider extension must be signed, embedded, and notarized with the desktop application.
- Existing updater signing, Windows Authenticode, macOS Developer ID/notarization, Android, and iOS release logic must remain intact.
- Any new third-party crate still receives its required notice/license treatment; existing `THIRD_PARTY_NOTICES.md` rules remain in force.

## Non-goals for this feature

- Android/iOS Files-app provider integration.
- Multi-account/multiple sync roots.
- Literal Windows drive-letter emulation.
- User-selectable local cache policy/"pin offline" UI beyond native provider state required for correct operation.
- Sharing/public-link changes.
- Replacing Device Continuity or Drive encryption.

## Acceptance criteria

1. Linux Drive behavior remains regression-clean after extracting the shared core.
2. Windows Explorer shows a registered TeraRelay cloud-files root and can enumerate placeholders from current Drive metadata.
3. Opening a Windows placeholder hydrates correct bytes on demand, including encrypted content when unlocked.
4. Windows create/write/rename/move/delete operations enter the existing TeraRelay mutation/upload pipeline and survive restart failures.
5. macOS Finder shows a TeraRelay File Provider domain and can enumerate/materialize items through the shared core bridge.
6. macOS create/write/rename/move/delete operations are durable/recoverable and do not bypass Drive metadata or encryption rules.
7. Both native providers reconnect idempotently after app/provider restart.
8. Provider callbacks cannot escape registered roots, bypass staging limits, or persist plaintext encrypted blocks.
9. Windows and macOS release builds compile/package their real provider components with required signing checks.
10. Existing Device Continuity, Remote Import, Version Stack, updater, Android/iOS, and unrelated work remain behaviorally unchanged.
