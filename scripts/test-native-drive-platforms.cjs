#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const assert = require('assert');

const root = path.resolve(__dirname, '..');
const read = (rel) => fs.readFileSync(path.join(root, rel), 'utf8').replace(/\r\n/g, '\n');
const exists = (rel) => fs.existsSync(path.join(root, rel));

function requireText(haystack, needle, message) {
  assert.ok(haystack.includes(needle), message || `Expected source to contain ${needle}`);
}

function rejectText(haystack, needle, message) {
  assert.ok(!haystack.includes(needle), message || `Expected source not to contain ${needle}`);
}

// Task 1 RED gate: the portable Drive core must exist independently from
// Linux FUSE / Windows CfAPI / macOS FileProvider adapter types.
assert.ok(
  exists('app/src-tauri/src/drive_fs_core.rs'),
  'portable Drive core app/src-tauri/src/drive_fs_core.rs must exist',
);

const core = read('app/src-tauri/src/drive_fs_core.rs');
requireText(core, 'pub enum DriveNodeId', 'Drive core must expose stable node identities');
requireText(core, 'pub enum DriveNodeKind', 'Drive core must expose platform-neutral node kinds');
requireText(core, 'pub enum DriveFsError', 'Drive core must expose platform-neutral errors');
requireText(core, 'pub struct DriveFsCore', 'Drive core must expose a reusable filesystem engine');
requireText(core, 'fn begin_write(', 'Drive core must expose copy-on-write editing for native providers');
requireText(core, 'fn abort_write(', 'Drive core must expose failed-write rollback for native providers');
requireText(core, 'encode_provider_identity', 'Drive core must encode provider identities');
requireText(core, 'decode_provider_identity', 'Drive core must validate/decode provider identities');
requireText(core, 'MAX_PROVIDER_IDENTITY_BYTES', 'provider identity input must be explicitly bounded');
requireText(core, 'pub fn read_versioned(', 'native providers must pin each multi-chunk read to one backing-content version');
requireText(core, 'content version changed during read', 'versioned reads must fail rather than mix two remote contents');
rejectText(core, 'fuser::', 'portable Drive core must not depend on FUSE types');
rejectText(core, 'CF_CALLBACK_', 'portable Drive core must not depend on Windows CfAPI types');
rejectText(core, 'NSFileProvider', 'portable Drive core must not depend on Apple FileProvider types');

const lib = read('app/src-tauri/src/lib.rs');
requireText(lib, 'pub mod drive_fs_core;', 'Rust crate must register the shared Drive core module');

const mount = read('app/src-tauri/src/drive_mount.rs');
requireText(
  mount,
  'impl DriveFsBackend for TeraRelayFs',
  'Linux Drive engine must implement the shared Drive backend contract',
);
requireText(
  mount,
  'DriveFsCore::new',
  'Linux FUSE adapter must be constructed through the shared Drive core',
);
requireText(
  mount,
  'DriveNodeId::Pending(id) => drive_pending_by_id(conn, id)',
  'portable Pending identities must resolve by their exact pending ID',
);
requireText(
  mount,
  'crate::drive_lookup::pending_for_file(conn, id)',
  'portable File identities must resolve their pending overlay before the remote file',
);
requireText(
  mount,
  'Node::Pending(pending) => DriveNodeMetadata {',
  'pending files must have explicit portable metadata mapping',
);
requireText(
  mount,
  '.unwrap_or_else(|| pending.id.clone())',
  'pending files must expose a stable base/new file id before and after finalization',
);

// Task 3 RED gate: Windows must use native Cloud Files + shell sync-root
// registration and must also monitor local creates/content edits.
assert.ok(
  exists('app/src-tauri/src/drive_windows.rs'),
  'Windows Cloud Files provider app/src-tauri/src/drive_windows.rs must exist',
);
const windowsProvider = read('app/src-tauri/src/drive_windows.rs');
for (const needle of [
  'CfRegisterSyncRoot',
  'CfConnectSyncRoot',
  'CfCreatePlaceholders',
  'CfExecute',
  'CF_CALLBACK_TYPE_FETCH_DATA',
  'decode_provider_identity',
  'ReadDirectoryChangesW',
  'reconcile_local_tree',
  'placeholder_state',
  'prune_stale_placeholders',
  'placeholder_matches_remote_path',
  'provider_delete_suppression',
  'mark_local_item_in_sync',
  'reconcile_connected_tree',
]) {
  requireText(windowsProvider, needle, `Windows provider must contain ${needle}`);
}
rejectText(windowsProvider.toLowerCase(), 'winfsp', 'Windows provider must not depend on WinFsp');
requireText(windowsProvider, 'Windows Drive rename callback panicked', 'Windows rename callback must catch Rust panics before crossing FFI');
requireText(windowsProvider, 'Windows Drive delete callback panicked', 'Windows delete callback must catch Rust panics before crossing FFI');
requireText(windowsProvider, 'consume_provider_delete_suppression', 'provider-driven stale placeholder deletion must not echo into remote deletion');
requireText(windowsProvider, 'ModifiedDataSize', 'stale placeholder cleanup must preserve locally modified content');
requireText(windowsProvider, 'CF_IN_SYNC_STATE_IN_SYNC', 'Windows dirty detection must preserve zero-byte truncations and other out-of-sync placeholders');
requireText(windowsProvider, 'locally_dirty', 'Windows placeholder reconciliation must use explicit dirty state');
requireText(windowsProvider, 'Primary: CF_POPULATION_POLICY_ALWAYS_FULL', 'preprojected Windows namespace must not request unimplemented FETCH_PLACEHOLDERS callbacks');
requireText(windowsProvider, 'CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT', 'Windows sync root must be marked fully populated');
requireText(windowsProvider, 'CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION', 'projected Windows directories must be marked fully populated');
requireText(windowsProvider, 'CF_SYNC_ROOT_INFO_STANDARD', 'existing Windows sync-root ownership must be inspected before update');
requireText(windowsProvider, 'aligned_query_buffer', 'CfAPI variable-size query structs must use aligned storage');
rejectText(windowsProvider, 'vec![0u8; size_of::<CF_SYNC_ROOT_STANDARD_INFO>()', 'sync-root info must not cast byte-aligned storage to a C struct');
rejectText(windowsProvider, 'vec![0u8; size_of::<CF_PLACEHOLDER_STANDARD_INFO>()', 'placeholder info must not cast byte-aligned storage to a C struct');
requireText(windowsProvider, 'already registered by another sync provider', 'Windows must refuse to take over another provider sync root');
const registerStart = windowsProvider.indexOf('fn register_sync_root(');
const registerEnd = windowsProvider.indexOf('fn metadata_to_fs(', registerStart);
assert.ok(registerStart >= 0 && registerEnd > registerStart, 'Windows register_sync_root body must be inspectable');
const registerBody = windowsProvider.slice(registerStart, registerEnd);
requireText(registerBody, 'metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0', 'Windows sync root must reject junctions and other reparse-point roots');
requireText(windowsProvider, 'core.abort_write(&write_item.id)', 'Windows local import failures must roll back pending writes');
requireText(windowsProvider, 'walk_local(core, root, &path, depth + 1)?;', 'Windows reconnect scan must recurse through every Drive directory to recover nested offline edits');
rejectText(windowsProvider, 'if !was_placeholder {', 'Windows reconnect scan must not skip nested offline edits merely because their parent is already a placeholder');
rejectText(windowsProvider, 'CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION', 'Windows provider must allow its local-edit importer to hydrate untouched ranges of edited placeholders');
requireText(windowsProvider, 'provider_range_covers', 'Windows cancellation must distinguish partial from full fetch cancellation');
requireText(windowsProvider, 'CfUnregisterSyncRoot', 'Windows provider uninstall must unregister the Cloud Files sync root');
requireText(windowsProvider, 'unregister_known_sync_root', 'Windows provider must expose safe uninstall cleanup');
requireText(windowsProvider, 'fn callback_failure_range(', 'Windows hydration failure acknowledgement must not depend on decoding Drive identity');
const callbackFailureStart = windowsProvider.indexOf('fn callback_failure_range(');
const callbackFailureEnd = windowsProvider.indexOf('fn hydrate_request(', callbackFailureStart);
assert.ok(callbackFailureStart >= 0 && callbackFailureEnd > callbackFailureStart, 'Windows callback failure-range body must be inspectable');
const callbackFailureBody = windowsProvider.slice(callbackFailureStart, callbackFailureEnd);
requireText(callbackFailureBody, 'info.FileSize', 'Windows hydration failure acknowledgement must derive a safe range from CfAPI callback metadata');
requireText(callbackFailureBody, 'align_provider_read_range', 'Windows hydration failure acknowledgement must use the same 4 KiB alignment rules as success');
requireText(windowsProvider, 'let failure_range = callback_failure_range(', 'Windows fetch callback must prepare failure completion before Drive identity lookup');
requireText(windowsProvider, '.read_snapshot_versioned(&id, &metadata, offset, take)', 'Windows multi-chunk hydration must pin the original backing-content and pending-write metadata snapshot');
requireText(windowsProvider, 'VolumeDosName', 'Windows rename callback must normalize CfAPI volume-relative target paths');
requireText(windowsProvider, 'windows_volume_target_path', 'Windows rename callback must convert volume-relative targets before Drive path resolution');
requireText(core, 'pub fn windows_volume_target_path(', 'portable tests must cover CfAPI rename target normalization');
requireText(windowsProvider, '#[cfg(debug_assertions)]\n    if let Some(value) = std::env::var_os("TERARELAY_DRIVE_WINDOWS_PATH")', 'production Windows sync-root path must stay aligned with the Explorer installer target');
requireText(windowsProvider, 'STATUS_CLOUD_FILE_UNSUCCESSFUL', 'Windows hydration failures must report a Cloud Files status');
requireText(windowsProvider, 'use std::os::windows::ffi::OsStrExt;', 'Windows CfAPI paths must preserve native Windows UTF-16');
requireText(windowsProvider, '.encode_wide()', 'Windows CfAPI paths must be encoded losslessly from OsStr');
rejectText(windowsProvider, 'path.to_string_lossy()', 'Windows CfAPI paths must not use lossy UTF-8 conversion');
const reparseStart = windowsProvider.indexOf('fn is_reparse_point(');
const reparseEnd = windowsProvider.indexOf('#[derive(Debug)]\nstruct PlaceholderState', reparseStart);
assert.ok(reparseStart >= 0 && reparseEnd > reparseStart, 'Windows is_reparse_point body must be inspectable');
const reparseBody = windowsProvider.slice(reparseStart, reparseEnd);
requireText(reparseBody, 'std::fs::symlink_metadata(path)', 'Windows reparse-point inspection must examine the path itself without following links');
rejectText(reparseBody, 'std::fs::metadata(path)', 'Windows reparse-point inspection must not follow links before checking FILE_ATTRIBUTE_REPARSE_POINT');
requireText(windowsProvider, 'CfGetPlaceholderStateFromAttributeTag', 'Windows traversal must distinguish Cloud Files placeholders from foreign reparse points');
requireText(windowsProvider, 'CF_PLACEHOLDER_STATE_PLACEHOLDER', 'Windows traversal must require the Cloud Files placeholder state before descending through a reparse directory');
requireText(windowsProvider, 'fn is_cloud_placeholder(', 'Windows traversal must have an explicit Cloud Files placeholder classifier');
requireText(windowsProvider, 'if reparse && !is_cloud_placeholder(&path)?', 'Windows local traversal must skip foreign directory reparse points instead of following them');
const watcherEventStart = windowsProvider.indexOf('fn process_watcher_event(');
const watcherEventEnd = windowsProvider.indexOf('fn start_watcher(', watcherEventStart);
assert.ok(watcherEventStart >= 0 && watcherEventEnd > watcherEventStart, 'Windows watcher event body must be inspectable');
const watcherEventBody = windowsProvider.slice(watcherEventStart, watcherEventEnd);
requireText(watcherEventBody, 'FILE_ATTRIBUTE_REPARSE_POINT', 'Windows live watcher must inspect reparse points before ingestion');
requireText(watcherEventBody, '!is_cloud_placeholder(&path)', 'Windows live watcher must ignore foreign reparse points just like reconnect scanning');
const markSyncStart = windowsProvider.indexOf('fn mark_local_item_in_sync(');
const markSyncEnd = windowsProvider.indexOf('fn ingest_local_directory(', markSyncStart);
assert.ok(markSyncStart >= 0 && markSyncEnd > markSyncStart, 'Windows mark_local_item_in_sync body must be inspectable');
const markSyncBody = windowsProvider.slice(markSyncStart, markSyncEnd);
requireText(markSyncBody, 'CF_OPEN_FILE_FLAG_WRITE_ACCESS', 'Windows local placeholder conversion/update must request CfAPI write access');
requireText(windowsProvider, 'fn refresh_remote_file_placeholder(', 'Windows continuity refresh must update existing clean file placeholders after remote content changes');
const remoteRefreshStart = windowsProvider.indexOf('fn refresh_remote_file_placeholder(');
const remoteRefreshEnd = windowsProvider.indexOf('fn resolve_parent(', remoteRefreshStart);
assert.ok(remoteRefreshStart >= 0 && remoteRefreshEnd > remoteRefreshStart, 'Windows remote placeholder refresh body must be inspectable');
const remoteRefreshBody = windowsProvider.slice(remoteRefreshStart, remoteRefreshEnd);
requireText(remoteRefreshBody, 'CF_UPDATE_FLAG_VERIFY_IN_SYNC', 'Windows remote refresh must fail safely if a placeholder became locally dirty');
requireText(remoteRefreshBody, 'CF_UPDATE_FLAG_DEHYDRATE', 'Windows remote refresh must invalidate stale hydrated bytes');
requireText(remoteRefreshBody, 'CF_OPEN_FILE_FLAG_EXCLUSIVE', 'Windows remote dehydration must use an exclusive CfAPI oplock handle');
requireText(remoteRefreshBody, 'CF_OPEN_FILE_FLAG_WRITE_ACCESS', 'Windows remote placeholder updates must request write access');
requireText(windowsProvider, 'refresh_remote_file_placeholder(&destination, item)', 'Windows projection must refresh existing same-identity file placeholders instead of skipping them');
requireText(windowsProvider, 'WINDOWS_PROJECTION_LEDGER_VERSION', 'Windows reconnect recovery must persist a projection baseline outside the sync root');
requireText(windowsProvider, 'plan_offline_projection_recovery', 'Windows reconnect recovery must compare local placeholders against the last projected remote state');
requireText(windowsProvider, 'load_projection_ledger', 'Windows reconnect recovery must load the last successful projection baseline');
requireText(windowsProvider, 'save_projection_ledger', 'Windows reconciliation must persist the new baseline only after successful projection');
requireText(windowsProvider, 'load_drive_provider_projection', 'Windows projection recovery state must use TeraRelay local database state, not the sync root');
requireText(windowsProvider, 'apply_offline_projection_actions', 'Windows reconnect must replay provable offline rename/delete actions before stale placeholder pruning');
requireText(windowsProvider, 'DriveOfflineProjectionAction::Conflict', 'Windows reconnect must fail closed on ambiguous local/remote divergence');
rejectText(windowsProvider, 'HashSet<i64>', 'Windows cancellation must not collapse all cancel ranges to a request-key boolean');

// Explorer shell integration must ship in both Windows installer formats.
const explorerClsid = '{3F1DA139-ED78-4BD2-9C96-EBF7981E5E4B}';
for (const rel of [
  'app/src-tauri/windows/nsis/explorer-drive.nsh',
  'app/src-tauri/windows/fragments/explorer-drive.wxs',
]) {
  assert.ok(exists(rel), `${rel} must exist for the Explorer navigation integration`);
}
const nsisExplorer = read('app/src-tauri/windows/nsis/explorer-drive.nsh');
const wixExplorer = read('app/src-tauri/windows/fragments/explorer-drive.wxs');
for (const source of [nsisExplorer, wixExplorer]) {
  requireText(source, explorerClsid, 'Windows installers must share one stable Explorer namespace CLSID');
  requireText(source, 'System.IsPinnedToNameSpaceTree', 'Explorer node must be pinned in the navigation pane');
  requireText(source, 'TargetFolderPath', 'Explorer node must point to the TeraRelay CfAPI sync root');
  requireText(source, 'TeraRelay Drive', 'Explorer node must target the TeraRelay Drive folder');
  requireText(source, 'HideDesktopIcons', 'Explorer namespace extension must stay off the desktop');
}
requireText(nsisExplorer, 'NSIS_HOOK_POSTINSTALL', 'NSIS installer must register the Explorer namespace after install');
requireText(nsisExplorer, 'NSIS_HOOK_PREUNINSTALL', 'NSIS uninstaller must remove Explorer namespace state');
requireText(nsisExplorer, '--unregister-drive-sync-root', 'NSIS full uninstall must invoke Cloud Files unregister cleanup');
requireText(nsisExplorer, '$UpdateMode', 'NSIS update flow must not unregister the active sync root');
requireText(nsisExplorer, 'DeleteRegKey', 'NSIS uninstaller must clean namespace registry keys');
requireText(wixExplorer, 'createAndRemoveOnUninstall', 'MSI registry component must clean itself on uninstall');
requireText(wixExplorer, 'TeraRelayExplorerNamespace', 'MSI Explorer registry component must have a stable referenced id');
requireText(wixExplorer, '--unregister-drive-sync-root', 'MSI full uninstall must invoke Cloud Files unregister cleanup');
requireText(wixExplorer, 'REMOVE=\"ALL\"', 'MSI Cloud Files cleanup must be uninstall-only');
requireText(wixExplorer, 'NOT UPGRADINGPRODUCTCODE', 'MSI upgrades must preserve the sync-root registration');


const rustMain = read('app/src-tauri/src/main.rs');
requireText(rustMain, '--unregister-drive-sync-root', 'Windows binary must expose uninstall cleanup before Tauri startup');
const cargo = read('app/src-tauri/Cargo.toml');
requireText(cargo, "target_os = \"windows\"", 'Cargo manifest must have Windows-only provider dependencies');
requireText(cargo, 'windows = {', 'Windows provider must use the windows crate');
rejectText(cargo.toLowerCase(), 'winfsp', 'Cargo manifest must not add WinFsp');
requireText(cargo, 'objc2-file-provider', 'macOS host must use native FileProvider bindings');
requireText(cargo, 'block2', 'macOS host FileProvider completion handlers must use Objective-C blocks');

// Task 4 RED gate: macOS must ship a native replicated File Provider extension
// with an authenticated local bridge, not macFUSE.
for (const rel of [
  'app/src-tauri/src/drive_provider_ipc.rs',
  'app/src-tauri/macos-file-provider/Package.swift',
  'app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderExtension.swift',
  'app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderEnumerator.swift',
  'app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderItem.swift',
  'app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/BridgeClient.swift',
  'app/src-tauri/macos-file-provider/Info.plist',
  'app/src-tauri/macos-file-provider/TeraRelayFileProvider.entitlements',
  'scripts/build-macos-file-provider.sh',
  'scripts/prepare-macos-file-provider.mjs',
]) {
  assert.ok(exists(rel), `${rel} must exist for the macOS File Provider`);
}
const macPrepare = read('scripts/prepare-macos-file-provider.mjs');
requireText(macPrepare, "process.platform !== 'darwin'", 'macOS bundle preparation must skip safely on Windows/Linux');
requireText(macPrepare, "spawnSync('/bin/bash'", 'macOS bundle preparation must invoke the real build only on macOS');
const macBuild = read('scripts/build-macos-file-provider.sh');
requireText(macBuild, 'TERARELAY_MACOS_SWIFT_TRIPLE', 'macOS File Provider build must support explicit target architecture');
requireText(macBuild, '--triple', 'SwiftPM File Provider build must cross-compile for the selected app target');
const ipc = read('app/src-tauri/src/drive_provider_ipc.rs');
requireText(ipc, 'PROTOCOL_VERSION', 'provider IPC must be versioned');
requireText(ipc, 'MAX_REQUEST_BYTES', 'provider IPC requests must be bounded');
requireText(ipc, 'pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;', 'macOS IPC request framing must fit one base64-encoded 4 MiB write chunk');
requireText(ipc, '127.0.0.1', 'provider IPC must bind to loopback only');
requireText(ipc, 'capability', 'provider IPC must authenticate with a capability');
requireText(ipc, 'NSFileProviderManager::addDomain_completionHandler', 'main macOS app must register the File Provider domain directly');
requireText(ipc, 'NSFileProviderWorkingSetContainerItemIdentifier', 'main macOS app must signal the replicated working set');
requireText(ipc, 'NSFileManager::defaultManager', 'macOS host must resolve the App Group container through Foundation');
requireText(ipc, 'containerURLForSecurityApplicationGroupIdentifier', 'macOS host must ask the system for the App Group container URL');
rejectText(ipc, '.join(\"Group Containers\")', 'macOS host must not manually construct the App Group container path');
requireText(ipc, 'PROVIDER_HOST_LOCK_FILE', 'macOS bridge must define a cross-process host lock in the App Group');
requireText(ipc, 'MAX_CONCURRENT_CLIENTS', 'macOS bridge must bound concurrent File Provider requests');
requireText(ipc, 'try_reserve_client', 'macOS bridge must reserve bounded client slots atomically');
requireText(ipc, 'MAX_LIST_PAGE_ITEMS', 'macOS bridge directory responses must be page bounded');
requireText(ipc, 'next_cursor', 'macOS bridge list responses must expose a continuation cursor');
requireText(ipc, 'terarelay-file-provider-client', 'macOS bridge must handle accepted File Provider requests concurrently');
requireText(ipc, 'if !thread_running.load(Ordering::SeqCst) {', 'macOS bridge shutdown wake connection must not spawn a 30-second client worker');
requireText(ipc, 'std::fs::remove_file(&endpoint_path)', 'macOS bridge startup failure must remove the endpoint it published before worker spawn');
const ipcStart = ipc.indexOf('pub(crate) fn start(');
const ipcFirstContainer = ipc.indexOf('    let container = group_container()?;', ipcStart);
assert.ok(ipcStart >= 0 && ipcFirstContainer > ipcStart, 'macOS IPC start reuse branch must be inspectable');
const ipcReuseBody = ipc.slice(ipcStart, ipcFirstContainer);
requireText(ipcReuseBody, 'if let Some(server) = slot.as_ref()', 'running macOS bridge reuse must read the in-memory server state');
requireText(ipcReuseBody, 'port: server.port', 'running macOS bridge reuse must reconstruct the endpoint from the live listener');
requireText(ipcReuseBody, 'private_write(&endpoint_path, &endpoint_bytes)?', 'running macOS bridge reuse must repair a missing/corrupt endpoint atomically');
rejectText(ipcReuseBody, 'std::fs::read(&endpoint_path)', 'running macOS bridge reuse must not trust stale endpoint-file contents');
requireText(ipc, 'libc::flock', 'macOS bridge must prevent a second host from replacing the shared endpoint');
requireText(ipc, 'LOCK_NB', 'macOS bridge host lock must fail fast instead of blocking app startup');
requireText(ipc, 'O_NOFOLLOW', 'macOS App Group state files must reject symlink targets');
requireText(ipc, 'create_new(true)', 'macOS private state replacement must use an exclusive temporary file');
requireText(ipc, 'std::fs::rename(&temporary_path, path)', 'macOS endpoint/capability writes must publish atomically in the App Group');
requireText(ipc, 'PermissionsExt', 'macOS private state files must repair strict Unix permissions');
requireText(ipc, '0o600', 'macOS endpoint/capability state must remain private to the user');
rejectText(ipc, '.truncate(true)', 'macOS File Provider state must never expose a partially truncated endpoint/capability file');
rejectText(ipc, 'Command::new', 'macOS File Provider control must not depend on a standalone helper process');
const swiftExtension = read('app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderExtension.swift');
requireText(swiftExtension, 'NSFileProviderReplicatedExtension', 'macOS must use replicated File Provider');
requireText(swiftExtension, 'NSFileProviderEnumerating', 'macOS File Provider must enumerate items/changes');
requireText(swiftExtension, '.mayAlreadyExist', 'macOS File Provider must make replayed creates idempotent');
rejectText(swiftExtension, '.versionOutOfDate', 'macOS File Provider must not use an unavailable Apple SDK error case');
requireText(swiftExtension, '.versionNoLongerAvailable', 'macOS File Provider must reject stale requested and mutation base versions using the supported SDK error');
requireText(swiftExtension, 'if #available(macOS 12.3, *)', 'macOS 12.0 compatibility must guard the versionNoLongerAvailable error introduced in 12.3');
requireText(swiftExtension, 'throw self.staleVersionError()', 'stale version failures must use the minimum-OS-compatible error path');
requireText(swiftExtension, 'BridgeClient.maxContentChunkBytes', 'macOS File Provider transfers must remain bounded');
requireText(swiftExtension, 'NSFileProviderManager(for:', 'File Provider downloads must resolve the active domain manager');
requireText(swiftExtension, 'temporaryDirectoryURL()', 'File Provider downloads must use the system same-volume temporary directory');
requireText(swiftExtension, 'contentVersion: wire.contentVersion', 'macOS multi-chunk fetch must pin reads to the enumerated backing-content version');
rejectText(swiftExtension, 'FileManager.default.temporaryDirectory', 'File Provider must not return a generic temporary-directory file to fetchContents');
rejectText(swiftExtension.toLowerCase(), 'macfuse', 'macOS provider must not use macFUSE');
const swiftEnumerator = read('app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderEnumerator.swift');
requireText(swiftEnumerator, 'isWorkingSet', 'macOS enumerator must distinguish the replicated working set from normal directories');
requireText(swiftEnumerator, 'maxWorkingSetDepth', 'working-set recursion must be depth bounded');
requireText(swiftEnumerator, 'maxWorkingSetItems', 'working-set recursion must be item bounded');
requireText(swiftEnumerator, 'expectedRevision', 'working-set enumeration must stay on one metadata revision');
requireText(swiftEnumerator, 'bridge.fileProviderIdentifier(wire.id)', 'working set must recursively enumerate nested directories');
requireText(swiftEnumerator, 'nextCursor', 'Finder enumeration must consume bounded bridge list pages');
requireText(swiftEnumerator, 'while cursor != nil', 'Finder enumeration must stream all bridge pages instead of requiring one oversized response');
rejectText(swiftEnumerator, '.load(as: UInt64.self)', 'sync-anchor decoding must not use alignment-sensitive Data.load');
const swiftBridge = read('app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/BridgeClient.swift');
const swiftItem = read('app/src-tauri/macos-file-provider/Sources/TeraRelayFileProvider/FileProviderItem.swift');
requireText(swiftBridge, 'maxRequestFrameBytes = 8 * 1024 * 1024', 'Swift IPC request framing must fit one base64-encoded 4 MiB write chunk');
rejectText(swiftBridge, 'private let decoder = JSONDecoder()', 'concurrent File Provider requests must not share mutable JSON decoder state');
requireText(swiftBridge, 'JSONDecoder().decode(BridgeEndpoint.self', 'endpoint decoding must use request-local decoder state');
requireText(swiftBridge, 'JSONDecoder().decode(BridgeResponse.self', 'response decoding must use request-local decoder state');
requireText(swiftBridge, 'payload.count <= Self.maxRequestFrameBytes', 'Swift bridge must enforce the shared bounded request frame');
requireText(swiftBridge, 'SO_NOSIGPIPE', 'macOS bridge sockets must suppress SIGPIPE when the host closes a connection');
requireText(swiftBridge, 'Darwin.setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE', 'macOS bridge must configure SO_NOSIGPIPE on every client socket');
rejectText(swiftBridge, 'SO_RCVTIMEO', 'macOS bridge must not impose a fixed receive deadline on real Telegram-backed File Provider writes');
requireText(swiftBridge, 'SO_SNDTIMEO', 'macOS bridge should still bound local request-frame sends');
requireText(swiftBridge, 'content_version', 'Swift bridge read requests must send the expected backing-content version');
requireText(swiftBridge, 'guard let written = response.written else', 'macOS bridge writes must fail closed when the Rust host omits the accepted byte count');
rejectText(swiftBridge, 'response.written ?? UInt64(data.count)', 'macOS bridge writes must never assume a missing host byte count means full success');
requireText(ipc, 'read_versioned', 'macOS Rust bridge must reject content changes during a multi-chunk fetch');
requireText(ipc, 'content_version', 'macOS bridge items must carry immutable backing-content identity');
requireText(swiftBridge, 'contentVersion', 'Swift bridge must decode backing-content identity');
requireText(swiftItem, 'wire.contentVersion', 'File Provider content version must bind to backing-content identity');
const contentVersionStart = swiftItem.indexOf('let content = versionData(');
const contentVersionEnd = swiftItem.indexOf('return NSFileProviderItemVersion', contentVersionStart);
assert.ok(contentVersionStart >= 0 && contentVersionEnd > contentVersionStart, 'File Provider content-version block must be inspectable');
const contentVersionBody = swiftItem.slice(contentVersionStart, contentVersionEnd);
rejectText(contentVersionBody, 'wire.updatedAtMs', 'metadata-only timestamp changes must not invalidate File Provider contentVersion');
requireText(swiftItem, 'wire.parent', 'File Provider metadata version must bind to parent identity');
requireText(swiftItem, 'wire.name', 'File Provider metadata version must bind to filename');
rejectText(swiftBridge, '.load(as: UInt32.self)', 'IPC frame decoding must not use alignment-sensitive Data.load');
requireText(ipc, '"lookup" =>', 'macOS bridge must support idempotent create lookup');
requireText(ipc, '"abortWrite" =>', 'macOS bridge must expose copy-on-write rollback');
requireText(swiftBridge, 'func abortWrite(', 'macOS Swift bridge must expose write rollback');
requireText(swiftExtension, 'bridge.abortWrite(identifier)', 'failed or cancelled Finder writes must roll back pending Drive state');
requireText(swiftExtension, 'options.contains(.recursive)', 'macOS File Provider must honor recursive Finder directory deletion');
requireText(swiftExtension, 'unappliedFields(', 'macOS File Provider must report metadata fields it does not persist');
requireText(swiftExtension, '.subtracting(applied)', 'macOS File Provider must not falsely acknowledge unsupported metadata fields');
requireText(swiftBridge, 'func removeRecursively(', 'Swift bridge must expose recursive directory removal');
requireText(ipc, '"removeRecursive" =>', 'Rust File Provider bridge must expose bounded recursive removal');
requireText(core, 'pub fn remove_recursive(', 'shared Drive core must provide bounded recursive removal without changing Linux rmdir semantics');
const modifyStart = swiftExtension.indexOf('func modifyItem(');
const modifyEnd = swiftExtension.indexOf('func deleteItem(', modifyStart);
assert.ok(modifyStart >= 0 && modifyEnd > modifyStart, 'macOS modifyItem body must be inspectable');
const modifyBody = swiftExtension.slice(modifyStart, modifyEnd);
assert.ok(
  modifyBody.indexOf('bridge.beginWrite(identifier)') >= 0 &&
  modifyBody.indexOf('bridge.beginWrite(identifier)') < modifyBody.indexOf('bridge.rename('),
  'multi-field Finder edits must start copy-on-write before rename/reparent',
);
requireText(modifyBody, 'pendingWriteAwaitingUpload', 'rename failure after beginWrite must roll back the pending replacement');
requireText(ipc, '"abortWrite" =>', 'macOS bridge must expose pending-write rollback');
requireText(swiftBridge, 'func abortWrite', 'Swift bridge must expose pending-write rollback');
requireText(swiftExtension, 'bridge.abortWrite(identifier)', 'File Provider failed/cancelled writes must roll back pending state');

const providerPlist = read('app/src-tauri/macos-file-provider/Info.plist');
requireText(providerPlist, 'com.apple.fileprovider-nonui', 'File Provider extension point must be declared');
requireText(providerPlist, 'NSExtensionFileProviderDocumentGroup', 'File Provider document group must be declared');
requireText(providerPlist, 'group.io.terarelay.drive', 'File Provider must use the TeraRelay App Group');
const providerEntitlements = read('app/src-tauri/macos-file-provider/TeraRelayFileProvider.entitlements');
requireText(providerEntitlements, 'com.apple.security.app-sandbox', 'File Provider extension must be sandboxed');
requireText(providerEntitlements, 'com.apple.security.application-groups', 'File Provider must have App Group access');
requireText(providerEntitlements, 'com.apple.security.network.client', 'File Provider must be allowed to call the loopback bridge');
rejectText(providerEntitlements, 'com.apple.developer.fileprovider.testing-mode', 'production extension must not ship testing-mode entitlement');

const tauriConfig = JSON.parse(read('app/src-tauri/tauri.conf.json'));
assert.strictEqual(
  tauriConfig.bundle.windows.nsis.installerHooks,
  'windows/nsis/explorer-drive.nsh',
  'Tauri NSIS bundle must install the Explorer namespace hook',
);
assert.ok(
  tauriConfig.bundle.windows.wix.fragmentPaths.includes('windows/fragments/explorer-drive.wxs'),
  'Tauri WiX bundle must include the Explorer namespace fragment',
);
assert.ok(
  tauriConfig.bundle.windows.wix.componentRefs.includes('TeraRelayExplorerNamespace'),
  'Tauri WiX bundle must reference the Explorer namespace registry component',
);
assert.strictEqual(
  tauriConfig.build.beforeBundleCommand,
  'node ../scripts/prepare-macos-file-provider.mjs',
  'Tauri bundling must use a cross-platform macOS File Provider gate',
);
assert.strictEqual(
  tauriConfig.bundle.macOS.files['PlugIns/TeraRelayFileProvider.appex'],
  'macos-file-provider/build/TeraRelayFileProvider.appex',
  'Tauri must embed the File Provider extension under Contents/PlugIns',
);
const releaseWorkflow = read('.github/workflows/release.yml');
requireText(
  releaseWorkflow,
  'node scripts/test-native-drive-platforms.cjs',
  'desktop release must run the native-drive contract before bundling',
);

const appEntitlements = read('app/src-tauri/entitlements.plist');
requireText(appEntitlements, 'group.io.terarelay.drive', 'main macOS app must share the File Provider App Group');

requireText(lib, 'pub mod drive_windows;', 'Rust crate must register Windows native Drive provider');
requireText(lib, 'pub mod drive_provider_ipc;', 'Rust crate must register macOS File Provider bridge');
requireText(mount, '"windows-cloud-files"', 'Drive status must identify Windows Cloud Files provider');
requireText(mount, '"macos-file-provider"', 'Drive status must identify macOS File Provider');
requireText(mount, 'crate::drive_windows::connect(', 'Windows mount command must start the CfAPI provider');
requireText(mount, 'crate::drive_provider_ipc::start(', 'macOS mount command must start the File Provider bridge');

console.log('native-drive platform contract: PASS');
