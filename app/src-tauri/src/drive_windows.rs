#![cfg(target_os = "windows")]

use crate::commands::drive_metadata::{
    load_drive_provider_projection, save_drive_provider_projection,
};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use crate::drive_fs_core::{
    align_provider_read_range, decode_provider_identity, encode_provider_identity,
    plan_offline_projection_recovery, provider_range_covers, windows_volume_target_path,
    DriveFsCore, DriveFsError, DriveLocalProjectionRecord, DriveNodeId, DriveNodeKind,
    DriveNodeMetadata, DriveOfflineProjectionAction, DriveProjectionRecord,
    MAX_PROVIDER_IDENTITY_BYTES,
};
use crate::drive_mount::{build_drive_fs_core, DriveMountState};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    STATUS_CLOUD_FILE_ACCESS_DENIED, STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS,
};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
};

const HYDRATION_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const LOCAL_IMPORT_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const PROVIDER_GUID: GUID = GUID::from_u128(0x7c8cc9b6_4f17_4bb4_918d_8a0d4b420301);
const PROVIDER_NAME: &str = "TeraRelay";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
const WINDOWS_PROJECTION_LEDGER_PROVIDER: &str = "windows-cfapi";
const WINDOWS_PROJECTION_LEDGER_VERSION: i64 = 1;
const WINDOWS_PROJECTION_MAX_ITEMS: usize = 100_000;
const WINDOWS_PROJECTION_MAX_DEPTH: usize = 512;

// `notify`'s Windows backend is ReadDirectoryChangesW. Keeping that detail
// explicit here also documents why no extra filesystem-driver runtime is needed.
const WINDOWS_WATCHER_BACKEND: &str = "ReadDirectoryChangesW";

#[derive(Clone, Copy)]
struct ActiveHydration {
    start: u64,
    length: u64,
    fully_cancelled: bool,
}

struct ActiveHydrationGuard {
    requests: Arc<Mutex<HashMap<i64, ActiveHydration>>>,
    request_key: i64,
}

impl Drop for ActiveHydrationGuard {
    fn drop(&mut self) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.remove(&self.request_key);
        }
    }
}

struct WindowsCallbackContext {
    core: DriveFsCore,
    db_pool: DbConnection,
    root: PathBuf,
    active_hydrations: Arc<Mutex<HashMap<i64, ActiveHydration>>>,
    provider_delete_suppression: Arc<Mutex<HashMap<DriveNodeId, Instant>>>,
}

struct WindowsDriveProvider {
    connection_key: CF_CONNECTION_KEY,
    _callback_context: Box<WindowsCallbackContext>,
    _watcher: RecommendedWatcher,
    watcher_running: Arc<AtomicBool>,
    watcher_thread: Option<JoinHandle<()>>,
    root: PathBuf,
}

impl WindowsDriveProvider {
    fn stop(mut self) {
        self.watcher_running.store(false, Ordering::SeqCst);
        drop(self._watcher);
        if let Some(worker) = self.watcher_thread.take() {
            let _ = worker.join();
        }
        // SAFETY: connection_key was returned by CfConnectSyncRoot in this
        // process and is disconnected exactly once while its callback context
        // is still alive.
        if let Err(error) = unsafe { CfDisconnectSyncRoot(self.connection_key) } {
            log::warn!("TeraRelay Windows Drive disconnect failed: {error}");
        }
    }
}

static PROVIDER: OnceLock<Mutex<Option<WindowsDriveProvider>>> = OnceLock::new();

fn provider_slot() -> &'static Mutex<Option<WindowsDriveProvider>> {
    PROVIDER.get_or_init(|| Mutex::new(None))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn aligned_query_buffer(byte_len: usize) -> Result<Vec<usize>, String> {
    let word = size_of::<usize>();
    let words = byte_len
        .checked_add(word.saturating_sub(1))
        .ok_or_else(|| "Windows Cloud Files query buffer size overflow".to_string())?
        / word;
    Ok(vec![0usize; words.max(1)])
}

fn query_buffer_byte_len(buffer: &[usize]) -> Result<u32, String> {
    let bytes = buffer
        .len()
        .checked_mul(size_of::<usize>())
        .ok_or_else(|| "Windows Cloud Files query buffer size overflow".to_string())?;
    u32::try_from(bytes).map_err(|_| "Windows Cloud Files query buffer is too large".to_string())
}

fn filetime_from_unix_ms(value: i64) -> i64 {
    // Windows FILETIME is 100ns since 1601-01-01; Unix epoch offset is
    // 11,644,473,600,000 milliseconds.
    value
        .saturating_add(11_644_473_600_000)
        .saturating_mul(10_000)
}

fn ensure_safe_relative(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Windows Drive path escaped the registered sync root".to_string())?;
    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err("Windows Drive path contains an unsafe component".to_string()),
        }
    }
    Ok(relative.to_path_buf())
}

fn root_path() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Some(value) = std::env::var_os("TERARELAY_DRIVE_WINDOWS_PATH") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "Could not resolve the Windows user profile for TeraRelay Drive".to_string()
        })?;
    Ok(home.join("TeraRelay Drive"))
}

fn query_sync_root_owner(root: &Path) -> Result<Option<bool>, String> {
    let root_wide = wide_path(root);
    let mut registration_buffer = aligned_query_buffer(
        size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + MAX_PROVIDER_IDENTITY_BYTES,
    )?;
    let registration_buffer_bytes = query_buffer_byte_len(&registration_buffer)?;
    let mut returned = 0u32;
    let query = unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(root_wide.as_ptr()),
            CF_SYNC_ROOT_INFO_STANDARD,
            registration_buffer.as_mut_ptr().cast(),
            registration_buffer_bytes,
            Some(&mut returned),
        )
    };
    if let Err(error) = query {
        // HRESULT_FROM_WIN32(ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT = 390).
        const NOT_UNDER_SYNC_ROOT_HRESULT: i32 = 0x8007_0186u32 as i32;
        if error.code().0 == NOT_UNDER_SYNC_ROOT_HRESULT {
            return Ok(None);
        }
        return Err(format!(
            "Could not inspect Windows Cloud Files sync-root ownership: {error}"
        ));
    }

    let identity_offset = offset_of!(CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity);
    if (returned as usize) < identity_offset {
        return Err("Windows returned truncated sync-root registration metadata".to_string());
    }
    let registration_bytes = registration_buffer.as_ptr().cast::<u8>();
    let info = unsafe { &*(registration_bytes.cast::<CF_SYNC_ROOT_STANDARD_INFO>()) };
    let provider_name_len = info
        .ProviderName
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(info.ProviderName.len());
    let provider_name = String::from_utf16(&info.ProviderName[..provider_name_len])
        .map_err(|_| "Windows sync-root provider name is invalid UTF-16".to_string())?;
    let identity_len = info.SyncRootIdentityLength as usize;
    if identity_len == 0
        || identity_len > MAX_PROVIDER_IDENTITY_BYTES
        || identity_offset.saturating_add(identity_len) > returned as usize
    {
        return Err("Windows returned an invalid sync-root identity".to_string());
    }
    let identity_bytes = unsafe {
        std::slice::from_raw_parts(registration_bytes.add(identity_offset), identity_len)
    };
    let identity = decode_provider_identity(identity_bytes).ok();
    Ok(Some(
        provider_name == PROVIDER_NAME && identity == Some(DriveNodeId::Root),
    ))
}

fn register_sync_root(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root)
        .map_err(|e| format!("Could not create Windows TeraRelay Drive root: {e}"))?;
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|e| format!("Could not inspect Windows TeraRelay Drive root: {e}"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err("Windows TeraRelay Drive root is not a safe directory".to_string());
    }

    let provider_name = wide(PROVIDER_NAME);
    let provider_version = wide(PROVIDER_VERSION);
    let root_identity = encode_provider_identity(&DriveNodeId::Root).map_err(|e| e.to_string())?;
    let registration = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(provider_name.as_ptr()),
        ProviderVersion: PCWSTR(provider_version.as_ptr()),
        SyncRootIdentity: root_identity.as_ptr().cast(),
        SyncRootIdentityLength: root_identity.len() as u32,
        FileIdentity: std::ptr::null(),
        FileIdentityLength: 0,
        ProviderId: PROVIDER_GUID,
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PARTIAL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_STREAMING_ALLOWED,
        },
        Population: CF_POPULATION_POLICY {
            // TeraRelay projects the complete namespace itself. Do not ask the
            // provider for FETCH_PLACEHOLDERS callbacks that it intentionally
            // does not implement.
            Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME
            | CF_INSYNC_POLICY_TRACK_DIRECTORY_LAST_WRITE_TIME,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    let root_wide = wide_path(root);
    let mut registration_buffer = aligned_query_buffer(
        size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + MAX_PROVIDER_IDENTITY_BYTES,
    )?;
    let registration_buffer_bytes = query_buffer_byte_len(&registration_buffer)?;
    let mut returned = 0u32;
    let registration_query = unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(root_wide.as_ptr()),
            CF_SYNC_ROOT_INFO_STANDARD,
            registration_buffer.as_mut_ptr().cast(),
            registration_buffer_bytes,
            Some(&mut returned),
        )
    };
    let already_registered = registration_query.is_ok();
    if already_registered {
        let identity_offset = offset_of!(CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity);
        if (returned as usize) < identity_offset {
            return Err("Windows returned truncated sync-root registration metadata".to_string());
        }
        let registration_bytes = registration_buffer.as_ptr().cast::<u8>();
        let info = unsafe { &*(registration_bytes.cast::<CF_SYNC_ROOT_STANDARD_INFO>()) };
        let provider_name_len = info
            .ProviderName
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(info.ProviderName.len());
        let provider_name = String::from_utf16(&info.ProviderName[..provider_name_len])
            .map_err(|_| "Windows sync-root provider name is invalid UTF-16".to_string())?;
        let identity_len = info.SyncRootIdentityLength as usize;
        if identity_len == 0
            || identity_len > MAX_PROVIDER_IDENTITY_BYTES
            || identity_offset.saturating_add(identity_len) > returned as usize
        {
            return Err("Windows returned an invalid sync-root identity".to_string());
        }
        let identity_bytes = unsafe {
            std::slice::from_raw_parts(registration_bytes.add(identity_offset), identity_len)
        };
        let identity = decode_provider_identity(identity_bytes)
            .map_err(|_| "Existing Windows sync root is not owned by TeraRelay".to_string())?;
        if provider_name != PROVIDER_NAME || identity != DriveNodeId::Root {
            return Err(
                "Windows Drive path is already registered by another sync provider".to_string(),
            );
        }
    }
    let base_flags = CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT
        | CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT;
    let flags = if already_registered {
        CF_REGISTER_FLAG_UPDATE | base_flags
    } else {
        base_flags
    };
    // SAFETY: all registration pointers refer to stack/Vec storage that remains
    // alive for the duration of this synchronous call.
    unsafe { CfRegisterSyncRoot(PCWSTR(root_wide.as_ptr()), &registration, &policies, flags) }
        .map_err(|e| format!("Could not register TeraRelay with Windows Cloud Files: {e}"))
}

fn metadata_to_fs(item: &DriveNodeMetadata) -> CF_FS_METADATA {
    let created = filetime_from_unix_ms(item.created_at_ms);
    let updated = filetime_from_unix_ms(item.updated_at_ms);
    CF_FS_METADATA {
        BasicInfo: windows::Win32::Storage::FileSystem::FILE_BASIC_INFO {
            CreationTime: created,
            LastAccessTime: updated,
            LastWriteTime: updated,
            ChangeTime: updated,
            FileAttributes: match item.kind {
                DriveNodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY.0,
                DriveNodeKind::File => FILE_ATTRIBUTE_NORMAL.0,
            },
        },
        FileSize: item.size.min(i64::MAX as u64) as i64,
    }
}

fn create_placeholder(parent: &Path, item: &DriveNodeMetadata) -> Result<(), String> {
    let destination = parent.join(&item.name);
    if destination.exists() {
        // Never overwrite normal local data or dirty placeholders. For a clean
        // same-identity file placeholder, however, continuity may have learned
        // about a newer remote object at the same path. Refresh its metadata and
        // invalidate any hydrated bytes so the next read cannot serve stale data.
        if item.kind == DriveNodeKind::File {
            refresh_remote_file_placeholder(&destination, item)?;
        }
        return Ok(());
    }
    let name = wide(&item.name);
    let identity = encode_provider_identity(&item.id).map_err(|e| e.to_string())?;
    let mut info = CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name.as_ptr()),
        FsMetadata: metadata_to_fs(item),
        FileIdentity: identity.as_ptr().cast(),
        FileIdentityLength: identity.len() as u32,
        Flags: if item.kind == DriveNodeKind::Directory {
            CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
                | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION
        } else {
            CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
        },
        ..Default::default()
    };
    let parent_wide = wide_path(parent);
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(parent_wide.as_ptr()),
            std::slice::from_mut(&mut info),
            CF_CREATE_FLAG_NONE,
            None,
        )
    }
    .map_err(|e| {
        format!(
            "Could not create Windows Drive placeholder {}: {e}",
            item.name
        )
    })
}

fn project_remote_tree(
    core: &DriveFsCore,
    parent_id: &DriveNodeId,
    parent_path: &Path,
    depth: usize,
) -> Result<(), String> {
    if depth > 512 {
        return Err("Windows Drive metadata hierarchy is too deep".to_string());
    }
    for item in core.list_directory(parent_id).map_err(|e| e.to_string())? {
        create_placeholder(parent_path, &item)?;
        if item.kind == DriveNodeKind::Directory {
            project_remote_tree(core, &item.id, &parent_path.join(&item.name), depth + 1)?;
        }
    }
    Ok(())
}

fn is_reparse_point(path: &Path) -> Result<bool, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect Windows Drive item: {e}"))?;
    Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

fn is_cloud_placeholder(path: &Path) -> Result<bool, String> {
    if !is_reparse_point(path)? {
        return Ok(false);
    }
    let path_wide = wide_path(path);
    let mut find_data = WIN32_FIND_DATAW::default();
    let find = unsafe { FindFirstFileW(PCWSTR(path_wide.as_ptr()), &mut find_data) }
        .map_err(|e| format!("Could not inspect Windows reparse tag: {e}"))?;
    let close_result = unsafe { FindClose(find) };
    close_result.map_err(|e| format!("Could not close Windows reparse inspection: {e}"))?;
    let state = unsafe {
        CfGetPlaceholderStateFromAttributeTag(find_data.dwFileAttributes, find_data.dwReserved0)
    };
    if state == CF_PLACEHOLDER_STATE_INVALID {
        return Err("Windows returned an invalid Cloud Files placeholder state".to_string());
    }
    Ok(state.contains(CF_PLACEHOLDER_STATE_PLACEHOLDER))
}

#[derive(Debug)]
struct PlaceholderState {
    identity: DriveNodeId,
    modified_data_size: i64,
    locally_dirty: bool,
}

fn placeholder_state(path: &Path) -> Result<Option<PlaceholderState>, String> {
    if !is_cloud_placeholder(path)? {
        return Ok(None);
    }
    let path_wide = wide_path(path);
    let handle =
        unsafe { CfOpenFileWithOplock(PCWSTR(path_wide.as_ptr()), CF_OPEN_FILE_FLAG_NONE) }
            .map_err(|e| format!("Could not open cloud placeholder state: {e}"))?;
    let mut buffer = aligned_query_buffer(
        size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + MAX_PROVIDER_IDENTITY_BYTES,
    )?;
    let buffer_bytes = query_buffer_byte_len(&buffer)?;
    let mut returned = 0u32;
    let result = unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_STANDARD,
            buffer.as_mut_ptr().cast(),
            buffer_bytes,
            Some(&mut returned),
        )
    };
    unsafe { CfCloseHandle(handle) };
    result.map_err(|e| format!("Could not inspect cloud placeholder metadata: {e}"))?;
    let identity_offset = offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
    if (returned as usize) < identity_offset {
        return Err("Windows returned truncated placeholder metadata".to_string());
    }
    let raw_bytes = buffer.as_ptr().cast::<u8>();
    let info = unsafe { &*(raw_bytes.cast::<CF_PLACEHOLDER_STANDARD_INFO>()) };
    let identity_len = info.FileIdentityLength as usize;
    if identity_len == 0
        || identity_len > MAX_PROVIDER_IDENTITY_BYTES
        || identity_offset.saturating_add(identity_len) > returned as usize
    {
        return Err("Windows returned an invalid placeholder identity".to_string());
    }
    let identity_bytes =
        unsafe { std::slice::from_raw_parts(raw_bytes.add(identity_offset), identity_len) };
    let identity = decode_provider_identity(identity_bytes).map_err(|e| e.to_string())?;
    let modified_data_size = info.ModifiedDataSize.max(0);
    Ok(Some(PlaceholderState {
        identity,
        modified_data_size,
        // A zero-byte truncate can have no modified byte range while still being
        // a genuine local edit. InSyncState is therefore authoritative for
        // clean-vs-dirty, with ModifiedDataSize retained as a conservative guard.
        locally_dirty: modified_data_size != 0 || info.InSyncState != CF_IN_SYNC_STATE_IN_SYNC,
    }))
}

fn refresh_remote_file_placeholder(path: &Path, item: &DriveNodeMetadata) -> Result<(), String> {
    if item.kind != DriveNodeKind::File {
        return Ok(());
    }
    let Some(state) = placeholder_state(path)? else {
        // An ordinary local file at the projected path belongs to the user.
        return Ok(());
    };
    if state.identity != item.id || state.locally_dirty {
        return Ok(());
    }

    let local = std::fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect existing Windows Drive placeholder: {e}"))?;
    if !local.is_file() {
        return Ok(());
    }
    let expected_last_write = filetime_from_unix_ms(item.updated_at_ms).max(0) as u64;
    if local.len() == item.size && local.last_write_time() == expected_last_write {
        return Ok(());
    }

    let path_wide = wide_path(path);
    // CfUpdatePlaceholder requires write access. Dehydrating stale bytes also
    // requires an exclusive oplock handle so a foreground writer cannot race the
    // remote refresh and have its data clobbered.
    let handle = unsafe {
        CfOpenFileWithOplock(
            PCWSTR(path_wide.as_ptr()),
            CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        )
    }
    .map_err(|e| format!("Could not open stale Windows Drive placeholder: {e}"))?;
    let metadata = metadata_to_fs(item);
    let result = unsafe {
        CfUpdatePlaceholder(
            handle,
            Some(&metadata),
            None,
            0,
            None,
            CF_UPDATE_FLAG_VERIFY_IN_SYNC
                | CF_UPDATE_FLAG_MARK_IN_SYNC
                | CF_UPDATE_FLAG_DEHYDRATE
                | CF_UPDATE_FLAG_ALLOW_PARTIAL,
            None,
            None,
        )
    };
    unsafe { CfCloseHandle(handle) };
    result.map_err(|e| format!("Could not refresh remote Windows Drive placeholder: {e}"))
}

fn resolve_parent(
    core: &DriveFsCore,
    root: &Path,
    path: &Path,
) -> Result<(DriveNodeId, String), String> {
    let relative = ensure_safe_relative(root, path)?;
    let mut components = relative.components().peekable();
    let mut parent = DriveNodeId::Root;
    let mut leaf = None;
    while let Some(component) = components.next() {
        let name = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| "Windows Drive path is not valid UTF-8".to_string())?
            .to_string();
        if components.peek().is_none() {
            leaf = Some(name);
            break;
        }
        let item = core.lookup(&parent, &name).map_err(|e| e.to_string())?;
        if item.kind != DriveNodeKind::Directory {
            return Err("Windows Drive parent path is not a directory".to_string());
        }
        parent = item.id;
    }
    Ok((
        parent,
        leaf.ok_or_else(|| "Windows Drive operation targeted the root".to_string())?,
    ))
}

fn ingest_local_file(core: &DriveFsCore, root: &Path, path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect local Windows Drive file: {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let reparse = is_reparse_point(path)?;
    if reparse && placeholder_state(path)?.is_some_and(|state| !state.locally_dirty) {
        return Ok(());
    }

    let (parent, name) = resolve_parent(core, root, path)?;
    let existing = core.lookup(&parent, &name).ok();
    if existing
        .as_ref()
        .is_some_and(|item| item.kind == DriveNodeKind::Directory)
    {
        return Err("A local file conflicts with a TeraRelay Drive directory".to_string());
    }
    let write_item = if let Some(item) = existing.as_ref() {
        core.begin_write(&item.id).map_err(|e| e.to_string())?
    } else {
        core.create_file(&parent, &name)
            .map_err(|e| e.to_string())?
    };
    let write_result = (|| -> Result<(), String> {
        core.resize(&write_item.id, 0).map_err(|e| e.to_string())?;

        let mut input = File::open(path)
            .map_err(|e| format!("Could not open local Windows Drive write: {e}"))?;
        let mut buffer = vec![0u8; LOCAL_IMPORT_CHUNK_BYTES];
        let mut offset = 0u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|e| format!("Could not read local Windows Drive write: {e}"))?;
            if read == 0 {
                break;
            }
            core.write(&write_item.id, offset, &buffer[..read])
                .map_err(|e| e.to_string())?;
            offset = offset
                .checked_add(read as u64)
                .ok_or_else(|| "Windows Drive local file size overflow".to_string())?;
        }
        core.resize(&write_item.id, offset)
            .map_err(|e| e.to_string())?;
        core.flush(&write_item.id).map_err(|e| e.to_string())?;
        core.close_write(&write_item.id).map_err(|e| e.to_string())
    })();
    if let Err(error) = write_result {
        if let Err(cleanup_error) = core.abort_write(&write_item.id) {
            log::warn!("Windows Drive failed-write rollback also failed: {cleanup_error}");
        }
        return Err(error);
    }

    let final_item = core.lookup(&parent, &name).map_err(|e| e.to_string())?;
    mark_local_item_in_sync(path, &final_item.id, reparse)
}

fn mark_local_item_in_sync(
    path: &Path,
    id: &DriveNodeId,
    already_placeholder: bool,
) -> Result<(), String> {
    let identity = encode_provider_identity(id).map_err(|e| e.to_string())?;
    let path_wide = wide_path(path);
    let handle =
        unsafe { CfOpenFileWithOplock(PCWSTR(path_wide.as_ptr()), CF_OPEN_FILE_FLAG_WRITE_ACCESS) }
            .map_err(|e| format!("Could not open local item for cloud conversion: {e}"))?;
    let is_directory = std::fs::metadata(path)
        .map_err(|e| format!("Could not inspect local item for cloud conversion: {e}"))?
        .is_dir();
    let update_flags = if is_directory {
        CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_DISABLE_ON_DEMAND_POPULATION
    } else {
        CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_ALLOW_PARTIAL
    };
    let mut result = if already_placeholder {
        unsafe {
            CfUpdatePlaceholder(
                handle,
                None,
                Some(identity.as_ptr().cast()),
                identity.len() as u32,
                None,
                update_flags,
                None,
                None,
            )
        }
    } else {
        unsafe {
            CfConvertToPlaceholder(
                handle,
                Some(identity.as_ptr().cast()),
                identity.len() as u32,
                CF_CONVERT_FLAG_MARK_IN_SYNC,
                None,
                None,
            )
        }
    };
    if result.is_ok() && is_directory && !already_placeholder {
        result = unsafe {
            CfUpdatePlaceholder(
                handle,
                None,
                Some(identity.as_ptr().cast()),
                identity.len() as u32,
                None,
                CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
                None,
                None,
            )
        };
    }
    if result.is_ok() {
        result = unsafe {
            CfSetInSyncState(
                handle,
                CF_IN_SYNC_STATE_IN_SYNC,
                CF_SET_IN_SYNC_FLAG_NONE,
                None,
            )
        };
    }
    unsafe { CfCloseHandle(handle) };
    result.map_err(|e| format!("Could not mark local Windows Drive item in sync: {e}"))
}

fn ingest_local_directory(core: &DriveFsCore, root: &Path, path: &Path) -> Result<(), String> {
    let reparse = is_reparse_point(path)?;
    if reparse {
        return Ok(());
    }
    let (parent, name) = resolve_parent(core, root, path)?;
    let item = match core.lookup(&parent, &name) {
        Ok(item) if item.kind == DriveNodeKind::Directory => item,
        Ok(_) => return Err("A local directory conflicts with a TeraRelay Drive file".to_string()),
        Err(DriveFsError::NotFound) => core
            .create_directory(&parent, &name)
            .map_err(|e| e.to_string())?,
        Err(error) => return Err(error.to_string()),
    };
    // Convert locally created folders too. Without this, later Explorer rename/delete
    // operations on the folder bypass Cloud Files callbacks and never reach the core.
    mark_local_item_in_sync(path, &item.id, false)
}

fn placeholder_matches_remote_path(
    core: &DriveFsCore,
    root: &Path,
    path: &Path,
    identity: &DriveNodeId,
) -> bool {
    let Ok((parent, name)) = resolve_parent(core, root, path) else {
        return false;
    };
    core.lookup(&parent, &name)
        .map(|item| item.id == *identity)
        .unwrap_or(false)
}

fn suppress_provider_delete(
    suppression: &Arc<Mutex<HashMap<DriveNodeId, Instant>>>,
    identity: &DriveNodeId,
) {
    if let Ok(mut entries) = suppression.lock() {
        let now = Instant::now();
        entries.retain(|_, created| now.duration_since(*created) < Duration::from_secs(10));
        entries.insert(identity.clone(), now);
    }
}

fn remove_provider_delete_suppression(
    suppression: &Arc<Mutex<HashMap<DriveNodeId, Instant>>>,
    identity: &DriveNodeId,
) {
    if let Ok(mut entries) = suppression.lock() {
        entries.remove(identity);
    }
}

fn consume_provider_delete_suppression(
    suppression: &Arc<Mutex<HashMap<DriveNodeId, Instant>>>,
    identity: &DriveNodeId,
) -> bool {
    let Ok(mut entries) = suppression.lock() else {
        return false;
    };
    let now = Instant::now();
    entries.retain(|_, created| now.duration_since(*created) < Duration::from_secs(10));
    entries.remove(identity).is_some()
}

fn load_projection_ledger(
    db_pool: &DbConnection,
) -> Result<Option<Vec<DriveProjectionRecord>>, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Windows Drive projection database is poisoned".to_string())?;
    load_drive_provider_projection(
        &conn,
        WINDOWS_PROJECTION_LEDGER_PROVIDER,
        WINDOWS_PROJECTION_LEDGER_VERSION,
    )
}

fn save_projection_ledger(
    db_pool: &DbConnection,
    records: &[DriveProjectionRecord],
) -> Result<(), String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Windows Drive projection database is poisoned".to_string())?;
    save_drive_provider_projection(
        &conn,
        WINDOWS_PROJECTION_LEDGER_PROVIDER,
        WINDOWS_PROJECTION_LEDGER_VERSION,
        records,
    )
}

fn collect_remote_projection_inner(
    core: &DriveFsCore,
    parent: &DriveNodeId,
    parent_path: &[String],
    depth: usize,
    records: &mut Vec<DriveProjectionRecord>,
    identities: &mut HashSet<DriveNodeId>,
) -> Result<(), String> {
    if depth > WINDOWS_PROJECTION_MAX_DEPTH {
        return Err("Windows Drive remote projection hierarchy is too deep".to_string());
    }
    for item in core.list_directory(parent).map_err(|e| e.to_string())? {
        if records.len() >= WINDOWS_PROJECTION_MAX_ITEMS {
            return Err("Windows Drive remote projection exceeds the safe item limit".to_string());
        }
        if !identities.insert(item.id.clone()) {
            return Err(
                "Windows Drive remote projection contains a duplicate identity".to_string(),
            );
        }
        let mut path = parent_path.to_vec();
        path.push(item.name.clone());
        records.push(DriveProjectionRecord {
            id: item.id.clone(),
            path: path.clone(),
            kind: item.kind,
            content_version: item.content_version.clone(),
            size: item.size,
            updated_at_ms: item.updated_at_ms,
        });
        if item.kind == DriveNodeKind::Directory {
            collect_remote_projection_inner(core, &item.id, &path, depth + 1, records, identities)?;
        }
    }
    Ok(())
}

fn collect_remote_projection(core: &DriveFsCore) -> Result<Vec<DriveProjectionRecord>, String> {
    let mut records = Vec::new();
    let mut identities = HashSet::new();
    collect_remote_projection_inner(
        core,
        &DriveNodeId::Root,
        &[],
        0,
        &mut records,
        &mut identities,
    )?;
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}

fn relative_projection_components(root: &Path, path: &Path) -> Result<Vec<String>, String> {
    let relative = ensure_safe_relative(root, path)?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err("Windows Drive path contains an unsafe component".to_string());
        };
        let component = component.to_str().ok_or_else(|| {
            "Windows Drive placeholder path cannot be represented in the shared UTF-8 namespace"
                .to_string()
        })?;
        components.push(component.to_string());
    }
    Ok(components)
}

fn collect_local_projection_inner(
    root: &Path,
    directory: &Path,
    depth: usize,
    records: &mut Vec<DriveLocalProjectionRecord>,
    identities: &mut HashSet<DriveNodeId>,
) -> Result<(), String> {
    if depth > WINDOWS_PROJECTION_MAX_DEPTH {
        return Err("Windows Drive local projection hierarchy is too deep".to_string());
    }
    for entry in std::fs::read_dir(directory)
        .map_err(|e| format!("Could not enumerate Windows Drive projection: {e}"))?
    {
        let entry =
            entry.map_err(|e| format!("Could not enumerate Windows Drive projection item: {e}"))?;
        let path = entry.path();
        ensure_safe_relative(root, &path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Could not inspect Windows Drive projection item: {e}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        if reparse {
            if !is_cloud_placeholder(&path)? {
                continue;
            }
            let state = placeholder_state(&path)?.ok_or_else(|| {
                "Windows Cloud Files placeholder did not expose provider state".to_string()
            })?;
            if records.len() >= WINDOWS_PROJECTION_MAX_ITEMS {
                return Err(
                    "Windows Drive local projection exceeds the safe item limit".to_string()
                );
            }
            if !identities.insert(state.identity.clone()) {
                return Err(
                    "Windows Drive local projection contains a duplicate identity".to_string(),
                );
            }
            records.push(DriveLocalProjectionRecord {
                id: state.identity,
                path: relative_projection_components(root, &path)?,
                locally_dirty: state.locally_dirty,
            });
        }
        if metadata.is_dir() {
            // Normal local folders may contain moved cloud placeholders. Verified
            // Cloud Files directory placeholders are safe to recurse into; foreign
            // reparse points were rejected above.
            collect_local_projection_inner(root, &path, depth + 1, records, identities)?;
        }
    }
    Ok(())
}

fn collect_local_projection(root: &Path) -> Result<Vec<DriveLocalProjectionRecord>, String> {
    let mut records = Vec::new();
    let mut identities = HashSet::new();
    collect_local_projection_inner(root, root, 0, &mut records, &mut identities)?;
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}

fn projection_path(root: &Path, components: &[String]) -> Result<PathBuf, String> {
    if components.is_empty() || components.len() > WINDOWS_PROJECTION_MAX_DEPTH.saturating_add(1) {
        return Err("Windows Drive projection path depth is invalid".to_string());
    }
    let mut path = root.to_path_buf();
    for component in components {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains(['\\', '/'])
            || component.contains('\0')
        {
            return Err("Windows Drive projection path contains an unsafe component".to_string());
        }
        path.push(component);
    }
    ensure_safe_relative(root, &path)?;
    Ok(path)
}

fn ingest_local_directories_only(
    core: &DriveFsCore,
    root: &Path,
    directory: &Path,
    depth: usize,
) -> Result<(), String> {
    if depth > WINDOWS_PROJECTION_MAX_DEPTH {
        return Err("Windows Drive local hierarchy is too deep".to_string());
    }
    for entry in std::fs::read_dir(directory)
        .map_err(|e| format!("Could not enumerate Windows Drive root: {e}"))?
    {
        let entry = entry.map_err(|e| format!("Could not enumerate Windows Drive item: {e}"))?;
        let path = entry.path();
        ensure_safe_relative(root, &path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Could not inspect Windows Drive item: {e}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        if reparse {
            if !is_cloud_placeholder(&path)? {
                continue;
            }
        } else {
            ingest_local_directory(core, root, &path)?;
        }
        ingest_local_directories_only(core, root, &path, depth + 1)?;
    }
    Ok(())
}

fn apply_offline_projection_actions(
    core: &DriveFsCore,
    root: &Path,
    baseline: &[DriveProjectionRecord],
    actions: &[DriveOfflineProjectionAction],
) -> Result<(), String> {
    if actions
        .iter()
        .any(|action| matches!(action, DriveOfflineProjectionAction::Conflict { .. }))
    {
        return Err(
            "Windows Drive has ambiguous offline local/remote namespace changes; refusing to overwrite either side"
                .to_string(),
        );
    }

    let baseline_depth: HashMap<&DriveNodeId, usize> = baseline
        .iter()
        .map(|record| (&record.id, record.path.len()))
        .collect();
    let mut renames: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            DriveOfflineProjectionAction::Rename { id, path } => Some((id, path)),
            _ => None,
        })
        .collect();
    renames.sort_by_key(|(_, path)| path.len());
    for (id, components) in renames {
        let target = projection_path(root, components)?;
        let (parent, name) = resolve_parent(core, root, &target)?;
        let metadata = core.metadata(id).map_err(|e| e.to_string())?;
        if metadata.parent == parent && metadata.name == name {
            continue;
        }
        core.rename(id, &parent, &name).map_err(|e| e.to_string())?;
    }

    let mut deletes: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            DriveOfflineProjectionAction::Delete { id } => Some(id),
            _ => None,
        })
        .collect();
    deletes.sort_by_key(|id| std::cmp::Reverse(*baseline_depth.get(id).unwrap_or(&0)));
    for id in deletes {
        let metadata = match core.metadata(id) {
            Ok(metadata) => metadata,
            Err(DriveFsError::NotFound) => continue,
            Err(error) => return Err(error.to_string()),
        };
        match metadata.kind {
            DriveNodeKind::File => core.remove_file(id).map_err(|e| e.to_string())?,
            DriveNodeKind::Directory => core.remove_directory(id).map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}

fn prune_stale_placeholders(
    core: &DriveFsCore,
    root: &Path,
    directory: &Path,
    depth: usize,
    suppression: Option<&Arc<Mutex<HashMap<DriveNodeId, Instant>>>>,
) -> Result<(), String> {
    if depth > 512 {
        return Err("Windows Drive placeholder hierarchy is too deep".to_string());
    }
    for entry in std::fs::read_dir(directory)
        .map_err(|e| format!("Could not enumerate Windows Drive placeholders: {e}"))?
    {
        let entry =
            entry.map_err(|e| format!("Could not enumerate Windows Drive placeholder: {e}"))?;
        let path = entry.path();
        ensure_safe_relative(root, &path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Could not inspect Windows Drive placeholder: {e}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        if reparse && !is_cloud_placeholder(&path)? {
            continue;
        }
        if metadata.is_dir() {
            // Prune descendants first so a remotely removed/moved directory can
            // be removed without recursively deleting any untracked local data.
            prune_stale_placeholders(core, root, &path, depth + 1, suppression)?;
        }
        let Some(state) = placeholder_state(&path)? else {
            continue;
        };
        if state.locally_dirty
            || placeholder_matches_remote_path(core, root, &path, &state.identity)
        {
            continue;
        }

        if let Some(suppression) = suppression {
            suppress_provider_delete(suppression, &state.identity);
        }
        let removal = if metadata.is_dir() {
            std::fs::remove_dir(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = removal {
            if let Some(suppression) = suppression {
                remove_provider_delete_suppression(suppression, &state.identity);
            }
            // A non-empty directory means something local is still present.
            // Preserve it rather than recursively deleting user data.
            if metadata.is_dir() && error.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                continue;
            }
            return Err(format!(
                "Could not remove stale Windows Drive placeholder {}: {error}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn walk_local(
    core: &DriveFsCore,
    root: &Path,
    directory: &Path,
    depth: usize,
) -> Result<(), String> {
    if depth > 512 {
        return Err("Windows Drive local hierarchy is too deep".to_string());
    }
    for entry in std::fs::read_dir(directory)
        .map_err(|e| format!("Could not enumerate Windows Drive root: {e}"))?
    {
        let entry = entry.map_err(|e| format!("Could not enumerate Windows Drive item: {e}"))?;
        let path = entry.path();
        ensure_safe_relative(root, &path)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Could not inspect Windows Drive item: {e}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        if reparse && !is_cloud_placeholder(&path)? {
            continue;
        }
        if metadata.is_dir() {
            ingest_local_directory(core, root, &path)?;
            // Recurse through both existing provider placeholders and directories
            // that were created while TeraRelay was offline. A clean placeholder
            // directory can still contain a dirty descendant from an offline edit.
            walk_local(core, root, &path, depth + 1)?;
        } else if metadata.is_file() {
            ingest_local_file(core, root, &path)?;
        }
    }
    Ok(())
}

pub(crate) fn reconcile_local_tree(
    core: &DriveFsCore,
    root: &Path,
    db_pool: &DbConnection,
) -> Result<(), String> {
    let baseline = load_projection_ledger(db_pool)?;
    let remote_before = collect_remote_projection(core)?;
    let local_before = collect_local_projection(root)?;
    let actions =
        plan_offline_projection_recovery(baseline.as_deref(), &remote_before, &local_before);
    // Detect every ambiguous divergence before mutating Drive metadata. Ordinary
    // local folders are then created first so a safely proven moved placeholder
    // can target a folder that the user created while TeraRelay was closed.
    if actions
        .iter()
        .any(|action| matches!(action, DriveOfflineProjectionAction::Conflict { .. }))
    {
        return Err(
            "Windows Drive has ambiguous offline local/remote namespace changes; refusing to overwrite either side"
                .to_string(),
        );
    }
    ingest_local_directories_only(core, root, root, 0)?;
    apply_offline_projection_actions(core, root, baseline.as_deref().unwrap_or(&[]), &actions)?;
    walk_local(core, root, root, 0)?;
    // Before CfConnectSyncRoot there are no provider callbacks to suppress.
    prune_stale_placeholders(core, root, root, 0, None)?;
    project_remote_tree(core, &DriveNodeId::Root, root, 0)?;
    save_projection_ledger(db_pool, &collect_remote_projection(core)?)
}

fn reconcile_connected_tree(context: &WindowsCallbackContext) -> Result<(), String> {
    walk_local(&context.core, &context.root, &context.root, 0)?;
    prune_stale_placeholders(
        &context.core,
        &context.root,
        &context.root,
        0,
        Some(&context.provider_delete_suppression),
    )?;
    project_remote_tree(&context.core, &DriveNodeId::Root, &context.root, 0)?;
    save_projection_ledger(&context.db_pool, &collect_remote_projection(&context.core)?)
}

fn process_watcher_event(core: &DriveFsCore, root: &Path, event: Event) {
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    // Allow Windows to finish close/rename sequences before inspecting state.
    std::thread::sleep(Duration::from_millis(180));
    for path in event.paths {
        if ensure_safe_relative(root, &path).is_err() || !path.exists() {
            continue;
        }
        let result = (|| -> Result<(), String> {
            let meta = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if meta.file_type().is_symlink() {
                return Ok(());
            }
            let reparse = meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            if reparse && !is_cloud_placeholder(&path)? {
                return Ok(());
            }
            if meta.is_dir() {
                ingest_local_directory(core, root, &path)
            } else if meta.is_file() {
                ingest_local_file(core, root, &path)
            } else {
                Ok(())
            }
        })();
        if let Err(error) = result {
            log::warn!("Windows TeraRelay Drive local reconciliation deferred: {error}");
        }
    }
}

fn start_watcher(
    core: DriveFsCore,
    root: PathBuf,
) -> Result<(RecommendedWatcher, Arc<AtomicBool>, JoinHandle<()>), String> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .map_err(|e| format!("Could not initialize Windows Drive watcher: {e}"))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| format!("Could not watch Windows Drive root: {e}"))?;
    let running = Arc::new(AtomicBool::new(true));
    let thread_running = running.clone();
    let worker = std::thread::Builder::new()
        .name("terarelay-windows-drive-watch".to_string())
        .spawn(move || {
            let _backend = WINDOWS_WATCHER_BACKEND;
            while thread_running.load(Ordering::SeqCst) {
                match rx.recv_timeout(Duration::from_millis(400)) {
                    Ok(Ok(event)) => process_watcher_event(&core, &root, event),
                    Ok(Err(error)) => log::warn!("Windows Drive watcher error: {error}"),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|e| format!("Could not start Windows Drive watcher worker: {e}"))?;
    Ok((watcher, running, worker))
}

unsafe fn callback_context<'a>(
    info: *const CF_CALLBACK_INFO,
) -> Result<&'a WindowsCallbackContext, String> {
    if info.is_null() {
        return Err("Windows Cloud Files callback omitted callback info".to_string());
    }
    let pointer = unsafe { (*info).CallbackContext }.cast::<WindowsCallbackContext>();
    if pointer.is_null() {
        return Err("Windows Cloud Files callback omitted TeraRelay context".to_string());
    }
    Ok(unsafe { &*pointer })
}

unsafe fn callback_identity(info: *const CF_CALLBACK_INFO) -> Result<DriveNodeId, String> {
    if info.is_null() {
        return Err("Windows Cloud Files callback omitted callback info".to_string());
    }
    let len = unsafe { (*info).FileIdentityLength } as usize;
    let pointer = unsafe { (*info).FileIdentity }.cast::<u8>();
    if len == 0 || len > MAX_PROVIDER_IDENTITY_BYTES || pointer.is_null() {
        return Err("Windows Cloud Files callback supplied an invalid identity".to_string());
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer, len) };
    decode_provider_identity(bytes).map_err(|e| e.to_string())
}

fn transfer_param_size() -> u32 {
    (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_0>())
        as u32
}

unsafe fn transfer_data(
    info: &CF_CALLBACK_INFO,
    buffer: Option<&[u8]>,
    offset: i64,
    length: i64,
    status: windows::Win32::Foundation::NTSTATUS,
) -> Result<(), String> {
    let op_info = CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    let transfer = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: status,
        Buffer: buffer
            .map(|value| value.as_ptr().cast::<c_void>())
            .unwrap_or(std::ptr::null()),
        Offset: offset,
        Length: length,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: transfer_param_size(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: transfer,
        },
    };
    unsafe { CfExecute(&op_info, &mut params) }
        .map_err(|e| format!("Windows Cloud Files transfer completion failed: {e}"))
}

fn callback_failure_range(
    info: &CF_CALLBACK_INFO,
    parameters: &CF_CALLBACK_PARAMETERS,
) -> Option<(u64, u64)> {
    let fetch = unsafe { parameters.Anonymous.FetchData };
    if fetch.RequiredFileOffset < 0 || fetch.RequiredLength <= 0 || info.FileSize < 0 {
        return None;
    }
    align_provider_read_range(
        fetch.RequiredFileOffset as u64,
        fetch.RequiredLength as u64,
        info.FileSize as u64,
        4096,
    )
    .ok()
}

fn hydrate_request(
    info: &CF_CALLBACK_INFO,
    parameters: &CF_CALLBACK_PARAMETERS,
) -> Result<(), String> {
    let context = unsafe { callback_context(info) }?;
    let id = unsafe { callback_identity(info) }?;
    let fetch = unsafe { parameters.Anonymous.FetchData };
    if fetch.RequiredFileOffset < 0 || fetch.RequiredLength < 0 {
        return Err("Windows requested an invalid hydration range".to_string());
    }
    let requested_start = fetch.RequiredFileOffset as u64;
    let requested_length = fetch.RequiredLength as u64;
    requested_start
        .checked_add(requested_length)
        .ok_or_else(|| "Windows hydration request range overflow".to_string())?;
    {
        let mut active = context
            .active_hydrations
            .lock()
            .map_err(|_| "Windows hydration state is poisoned".to_string())?;
        active.insert(
            info.RequestKey,
            ActiveHydration {
                start: requested_start,
                length: requested_length,
                fully_cancelled: false,
            },
        );
    }
    let _active_guard = ActiveHydrationGuard {
        requests: context.active_hydrations.clone(),
        request_key: info.RequestKey,
    };
    // Pin the entire multi-chunk hydration to one immutable backing-content
    // version. A concurrent remote replacement must fail this request instead
    // of splicing bytes from two different Drive objects into one local file.
    let metadata = context.core.metadata(&id).map_err(|e| e.to_string())?;
    if metadata.kind != DriveNodeKind::File {
        return Err("Windows requested hydration for a Drive directory".to_string());
    }
    let expected_content_version = metadata.content_version.clone();

    // CfAPI requires TRANSFER_DATA offsets and lengths to be 4 KiB aligned
    // except for the final range ending at logical EOF.
    let (start, aligned_length) =
        align_provider_read_range(requested_start, requested_length, metadata.size, 4096)
            .map_err(|e| e.to_string())?;
    if aligned_length == 0 {
        return Ok(());
    }
    let end = start
        .checked_add(aligned_length)
        .ok_or_else(|| "Windows hydration range overflow".to_string())?;
    let mut offset = start;
    while offset < end {
        let cancelled = context
            .active_hydrations
            .lock()
            .map_err(|_| "Windows hydration state is poisoned".to_string())?
            .get(&info.RequestKey)
            .map(|request| request.fully_cancelled)
            .unwrap_or(false);
        if cancelled {
            // The Cloud Files filter already cancelled the outstanding IO before
            // issuing CANCEL_FETCH_DATA. Stop producing data without failing a
            // broader aligned range that could still satisfy another request.
            return Ok(());
        }
        let take = (end - offset).min(HYDRATION_CHUNK_BYTES);
        let bytes = context
            .core
            .read_versioned(&id, &expected_content_version, offset, take)
            .map_err(|e| e.to_string())?;
        if bytes.is_empty() && take != 0 {
            return Err("TeraRelay Drive hydration reached an unexpected end of file".to_string());
        }
        unsafe {
            transfer_data(
                info,
                Some(&bytes),
                offset as i64,
                bytes.len() as i64,
                STATUS_SUCCESS,
            )?
        };
        offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "Windows hydration progress overflow".to_string())?;
    }
    Ok(())
}

unsafe extern "system" fn fetch_data_callback(
    info: *const CF_CALLBACK_INFO,
    parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || parameters.is_null() {
        return;
    }
    let failure_range = callback_failure_range(unsafe { &*info }, unsafe { &*parameters });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hydrate_request(unsafe { &*info }, unsafe { &*parameters })
    }));
    if !matches!(result, Ok(Ok(()))) {
        match &result {
            Ok(Err(error)) => log::warn!("Windows Drive hydration failed: {error}"),
            Err(_) => log::error!("Windows Drive hydration callback panicked"),
            Ok(Ok(())) => {}
        }
        if let Some((offset, length)) = failure_range.filter(|(_, length)| *length > 0) {
            if let Err(error) = unsafe {
                transfer_data(
                    &*info,
                    None,
                    offset.min(i64::MAX as u64) as i64,
                    length.min(i64::MAX as u64) as i64,
                    STATUS_CLOUD_FILE_UNSUCCESSFUL,
                )
            } {
                log::error!("Windows Drive hydration failure acknowledgement failed: {error}");
            }
        }
    }
}

unsafe extern "system" fn cancel_fetch_data_callback(
    info: *const CF_CALLBACK_INFO,
    parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || parameters.is_null() {
        return;
    }
    let Ok(context) = (unsafe { callback_context(info) }) else {
        return;
    };
    let cancel = unsafe { (*parameters).Anonymous.Cancel.Anonymous.FetchData };
    if cancel.FileOffset < 0 || cancel.Length < 0 {
        log::warn!("Windows Cloud Files supplied an invalid cancellation range");
        return;
    }
    let cancel_start = cancel.FileOffset as u64;
    let cancel_length = cancel.Length as u64;
    let Ok(mut active) = context.active_hydrations.lock() else {
        return;
    };
    let Some(request) = active.get_mut(&(unsafe { (*info).RequestKey })) else {
        return;
    };
    match provider_range_covers(cancel_start, cancel_length, request.start, request.length) {
        Ok(true) => request.fully_cancelled = true,
        Ok(false) => {
            // CfAPI explicitly permits cancellation of only a subset of the
            // original fetch. Keep hydrating so bytes outside that subset can
            // still satisfy later/outstanding IO on the same request key.
        }
        Err(error) => log::warn!("Windows Cloud Files cancellation range ignored: {error}"),
    }
}

fn ack_rename_or_delete(
    info: &CF_CALLBACK_INFO,
    operation_type: CF_OPERATION_TYPE,
    status: windows::Win32::Foundation::NTSTATUS,
) -> Result<(), String> {
    let op_info = CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: operation_type,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    let mut params = if operation_type == CF_OPERATION_TYPE_ACK_RENAME {
        CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + size_of::<CF_OPERATION_PARAMETERS_0_6>()) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                AckRename: CF_OPERATION_PARAMETERS_0_6 {
                    Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                    CompletionStatus: status,
                },
            },
        }
    } else {
        CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + size_of::<CF_OPERATION_PARAMETERS_0_7>()) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                    Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                    CompletionStatus: status,
                },
            },
        }
    };
    unsafe { CfExecute(&op_info, &mut params) }
        .map_err(|e| format!("Windows Cloud Files mutation acknowledgement failed: {e}"))
}

unsafe extern "system" fn rename_callback(
    info: *const CF_CALLBACK_INFO,
    parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || parameters.is_null() {
        return;
    }
    let info_ref = unsafe { &*info };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (|| -> Result<(), String> {
            let context = unsafe { callback_context(info) }?;
            let id = unsafe { callback_identity(info) }?;
            let rename = unsafe { (*parameters).Anonymous.Rename };
            let target = rename
                .TargetPath
                .to_string()
                .map_err(|e| format!("Invalid Windows rename target: {e}"))?;
            let volume = unsafe { (*info).VolumeDosName }
                .to_string()
                .map_err(|e| format!("Invalid Windows callback volume name: {e}"))?;
            let target = PathBuf::from(
                windows_volume_target_path(&volume, &target).map_err(|e| e.to_string())?,
            );
            let (parent, name) = resolve_parent(&context.core, &context.root, &target)?;
            context
                .core
                .rename(&id, &parent, &name)
                .map_err(|e| e.to_string())?;
            Ok(())
        })()
    }))
    .unwrap_or_else(|_| Err("Windows Drive rename callback panicked".to_string()));
    let status = if result.is_ok() {
        STATUS_SUCCESS
    } else {
        STATUS_CLOUD_FILE_ACCESS_DENIED
    };
    if let Err(error) = result {
        log::warn!("Windows Drive rename rejected/deferred: {error}");
    }
    if let Err(error) = ack_rename_or_delete(info_ref, CF_OPERATION_TYPE_ACK_RENAME, status) {
        log::error!("Windows Drive rename acknowledgement failed: {error}");
    }
}

unsafe extern "system" fn delete_callback(
    info: *const CF_CALLBACK_INFO,
    _parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info_ref = unsafe { &*info };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (|| -> Result<(), String> {
            let context = unsafe { callback_context(info) }?;
            let id = unsafe { callback_identity(info) }?;
            if consume_provider_delete_suppression(&context.provider_delete_suppression, &id) {
                return Ok(());
            }
            let metadata = context.core.metadata(&id).map_err(|e| e.to_string())?;
            match metadata.kind {
                DriveNodeKind::Directory => context
                    .core
                    .remove_directory(&id)
                    .map_err(|e| e.to_string()),
                DriveNodeKind::File => context.core.remove_file(&id).map_err(|e| e.to_string()),
            }
        })()
    }))
    .unwrap_or_else(|_| Err("Windows Drive delete callback panicked".to_string()));
    let status = if result.is_ok() {
        STATUS_SUCCESS
    } else {
        STATUS_CLOUD_FILE_ACCESS_DENIED
    };
    if let Err(error) = result {
        log::warn!("Windows Drive delete rejected/deferred: {error}");
    }
    if let Err(error) = ack_rename_or_delete(info_ref, CF_OPERATION_TYPE_ACK_DELETE, status) {
        log::error!("Windows Drive delete acknowledgement failed: {error}");
    }
}

pub(crate) fn connect(
    app_handle: &AppHandle,
    mount_state: &DriveMountState,
    db_pool: &DbConnection,
    telegram_state: &TelegramState,
) -> Result<PathBuf, String> {
    let mut slot = provider_slot()
        .lock()
        .map_err(|_| "Windows Drive provider state is poisoned".to_string())?;
    if let Some(provider) = slot.as_ref() {
        return Ok(provider.root.clone());
    }

    let root = root_path()?;
    register_sync_root(&root)?;
    let core = build_drive_fs_core(app_handle, mount_state, db_pool, telegram_state)?;
    // Recover local edits before any missing remote path is projected. This is
    // the reconnect/full-scan loss-recovery gate for ReadDirectoryChangesW.
    reconcile_local_tree(&core, &root, db_pool)?;

    let active_hydrations = Arc::new(Mutex::new(HashMap::new()));
    let provider_delete_suppression = Arc::new(Mutex::new(HashMap::new()));
    let context = Box::new(WindowsCallbackContext {
        core: core.clone(),
        db_pool: db_pool.clone(),
        root: root.clone(),
        active_hydrations,
        provider_delete_suppression,
    });
    let callbacks = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(fetch_data_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(cancel_fetch_data_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(rename_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(delete_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let root_wide = wide_path(&root);
    let context_ptr = (&*context as *const WindowsCallbackContext).cast::<c_void>();
    let connection_key = unsafe {
        CfConnectSyncRoot(
            PCWSTR(root_wide.as_ptr()),
            callbacks.as_ptr(),
            Some(context_ptr),
            // The local-edit importer intentionally reads edited placeholders
            // back through the sync root so untouched ranges can hydrate before
            // the copy-on-write object is finalized. Do not block provider-initiated
            // hydration here; the fetch callback reads from the remote Drive core,
            // not from the sync-root path, so this does not recurse.
            CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
    }
    .map_err(|e| format!("Could not connect TeraRelay Windows Cloud Files provider: {e}"))?;

    let (watcher, running, worker) = match start_watcher(core, root.clone()) {
        Ok(value) => value,
        Err(error) => {
            let _ = unsafe { CfDisconnectSyncRoot(connection_key) };
            return Err(error);
        }
    };
    *slot = Some(WindowsDriveProvider {
        connection_key,
        _callback_context: context,
        _watcher: watcher,
        watcher_running: running,
        watcher_thread: Some(worker),
        root: root.clone(),
    });
    Ok(root)
}

pub(crate) fn refresh() -> Result<(), String> {
    let slot = provider_slot()
        .lock()
        .map_err(|_| "Windows Drive provider state is poisoned".to_string())?;
    let Some(provider) = slot.as_ref() else {
        return Ok(());
    };
    reconcile_connected_tree(&provider._callback_context)
}

pub fn unregister_known_sync_root() -> Result<(), String> {
    // This cleanup mode is invoked by the Windows uninstallers before the
    // executable is removed. Disconnect this process first; if another
    // TeraRelay instance is still connected, CfUnregisterSyncRoot will fail
    // and the uninstaller intentionally stops instead of leaving stale state.
    disconnect();
    let root = root_path()?;
    if !root.exists() {
        return Ok(());
    }
    match query_sync_root_owner(&root)? {
        None => return Ok(()),
        Some(false) => {
            return Err(
                "Refusing to unregister a Windows sync root not owned by TeraRelay".to_string(),
            )
        }
        Some(true) => {}
    }
    let root_wide = wide_path(&root);
    unsafe { CfUnregisterSyncRoot(PCWSTR(root_wide.as_ptr())) }
        .map_err(|e| format!("Could not unregister the TeraRelay Windows sync root: {e}"))
}

pub(crate) fn disconnect() {
    let provider = provider_slot().lock().ok().and_then(|mut slot| slot.take());
    if let Some(provider) = provider {
        provider.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_guid_is_stable_and_nonzero() {
        assert_ne!(PROVIDER_GUID, GUID::zeroed());
    }

    #[test]
    fn filetime_epoch_conversion_is_stable() {
        assert_eq!(filetime_from_unix_ms(0), 116_444_736_000_000_000);
    }
}
