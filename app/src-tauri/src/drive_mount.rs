use crate::commands::drive_metadata::{
    clear_drive_pending_ranges_for_span, create_drive_directory, create_drive_pending_file,
    create_drive_pending_replacement, delete_drive_pending, drive_file_by_entry_id,
    drive_object_block_location, drive_pending_by_id, drive_pending_chunks, drive_pending_parts,
    drive_pending_ranges, drive_snapshot, mark_drive_pending_closed, record_drive_pending_range,
    rename_drive_directory, rename_drive_file, rename_drive_pending,
    take_drive_pending_chunks_from, take_drive_pending_parts_starting_at_chunk,
    trash_drive_directory, trash_drive_file, update_drive_pending_size, DriveDirectoryRecord,
    DriveFileRecord, DrivePendingRecord,
};
use crate::db::DbConnection;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Debug, Clone, Serialize)]
pub struct DriveMountStatus {
    pub supported: bool,
    pub mounted: bool,
    pub mount_point: Option<String>,
    pub read_streaming: bool,
    pub write_staging: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveRecoveryResult {
    pub recovered: u32,
    pub remaining: u32,
    pub failures: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveStagedUpload {
    pub pending_id: String,
    pub path: String,
    pub folder_id: i64,
    pub file_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrivePendingCancelled {
    pub pending_id: String,
}

struct DriveMountInner {
    mounted: AtomicBool,
    mount_point: Mutex<Option<PathBuf>>,
    mounting: AtomicBool,
    active_pending: Mutex<std::collections::HashSet<String>>,
}

#[derive(Clone)]
pub struct DriveMountState {
    inner: Arc<DriveMountInner>,
    stream_port: u16,
    stream_token: String,
    cache_root: PathBuf,
    staging_root: PathBuf,
}

fn purge_legacy_plaintext_read_cache(cache_root: &Path) {
    // Early Drive prototypes persisted decrypted crypto-*.bin read blocks.
    // Never reuse these after encryption is enabled. Only visit TeraRelay's
    // own dedicated block-cache tree; never traverse other user directories.
    let Ok(file_directories) = std::fs::read_dir(cache_root) else {
        return;
    };
    for directory in file_directories.flatten() {
        if !directory
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(directory.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("crypto-")
                && (name.ends_with(".bin") || name.ends_with(".tmp"))
                && entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false)
            {
                if let Err(error) = std::fs::remove_file(entry.path()) {
                    log::warn!(
                        "Unable to remove obsolete plaintext Drive cache entry: {}",
                        error
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod read_cache_tests {
    use super::*;

    #[test]
    fn purges_only_legacy_plaintext_blocks_and_preserves_ciphertext() {
        let root =
            std::env::temp_dir().join(format!("tr-drive-cache-{:016x}", rand::random::<u64>()));
        let cache = root.join("blocks");
        let file = cache.join("file-fixture");
        std::fs::create_dir_all(&file).unwrap();
        let plaintext = file.join("crypto-0000000000000000.bin");
        let temporary_plaintext = file.join("crypto-0000000000000001.tmp");
        let ciphertext = file.join("cipher-0000000000000000.bin");
        let ordinary = file.join("0000000000000000.bin");
        std::fs::write(&plaintext, b"secret plaintext").unwrap();
        std::fs::write(&temporary_plaintext, b"plaintext temp").unwrap();
        std::fs::write(&ciphertext, b"authenticated ciphertext").unwrap();
        std::fs::write(&ordinary, b"unencrypted file cache").unwrap();

        purge_legacy_plaintext_read_cache(&cache);
        assert!(!plaintext.exists());
        assert!(!temporary_plaintext.exists());
        assert_eq!(
            std::fs::read(&ciphertext).unwrap(),
            b"authenticated ciphertext"
        );
        assert_eq!(std::fs::read(&ordinary).unwrap(), b"unencrypted file cache");

        std::fs::remove_dir_all(root).unwrap();
    }
}

impl DriveMountState {
    pub fn new(
        stream_port: u16,
        stream_token: String,
        cache_root: PathBuf,
    ) -> Result<Self, String> {
        let drive_root = cache_root.join("drive");
        let block_cache = drive_root.join("blocks");
        let staging_root = drive_root.join("staging");
        std::fs::create_dir_all(&block_cache)
            .map_err(|e| format!("Failed to create Drive cache directory: {e}"))?;
        std::fs::create_dir_all(&staging_root)
            .map_err(|e| format!("Failed to create Drive staging directory: {e}"))?;
        // Cloud write staging can contain plaintext before encryption and
        // pending upload. Never leave any of these app-owned directories
        // world-readable, including directories from earlier prototypes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&drive_root, &block_cache, &staging_root] {
                let metadata = std::fs::symlink_metadata(path)
                    .map_err(|e| format!("Failed to inspect private Drive cache: {e}"))?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err("Drive cache directory is not a regular directory".to_string());
                }
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| format!("Failed to secure private Drive cache: {e}"))?;
            }
        }
        purge_legacy_plaintext_read_cache(&block_cache);
        Ok(Self {
            inner: Arc::new(DriveMountInner {
                mounted: AtomicBool::new(false),
                mount_point: Mutex::new(None),
                mounting: AtomicBool::new(false),
                active_pending: Mutex::new(std::collections::HashSet::new()),
            }),
            stream_port,
            stream_token,
            cache_root: block_cache,
            staging_root,
        })
    }

    pub(crate) fn is_pending_active(&self, pending_id: &str) -> bool {
        self.inner
            .active_pending
            .lock()
            .map(|pending| pending.contains(pending_id))
            .unwrap_or(false)
    }

    pub(crate) fn set_pending_active(&self, pending_id: &str, active: bool) {
        if let Ok(mut pending) = self.inner.active_pending.lock() {
            if active {
                pending.insert(pending_id.to_string());
            } else {
                pending.remove(pending_id);
            }
        }
    }

    fn status(&self) -> DriveMountStatus {
        DriveMountStatus {
            supported: cfg!(target_os = "linux"),
            mounted: self.inner.mounted.load(Ordering::SeqCst),
            mount_point: self.inner.mount_point.lock().ok().and_then(|path| {
                path.as_ref()
                    .map(|value| value.to_string_lossy().into_owned())
            }),
            read_streaming: cfg!(target_os = "linux"),
            write_staging: cfg!(target_os = "linux"),
        }
    }
}

#[tauri::command]
pub fn cmd_drive_status(state: State<'_, DriveMountState>) -> DriveMountStatus {
    state.status()
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub fn cmd_drive_mount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    let _ = state;
    Err("TeraRelay Drive V1 is currently available on Linux only".to_string())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    Ok(state.status())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub fn cmd_drive_recover_pending(
    _app_handle: AppHandle,
    _state: State<'_, DriveMountState>,
    _db_pool: State<'_, DbConnection>,
    _telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveRecoveryResult, String> {
    Ok(DriveRecoveryResult {
        recovered: 0,
        remaining: 0,
        failures: 0,
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::commands::drive_metadata::DriveSnapshot;
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
        TimeOrNow,
    };
    use libc::{EACCES, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, EROFS};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const ROOT_INODE: u64 = 1;
    const TTL: Duration = Duration::from_secs(1);
    const CACHE_BLOCK_SIZE: u64 = 4 * 1024 * 1024;
    const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    // Only bounded chunks may reside locally. Scatter/random writes must not
    // silently consume the entire local disk while waiting to be completed.
    const STAGING_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

    fn ensure_private_staging_directory(path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Could not create private Drive staging directory: {e}"))?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("Could not inspect private Drive staging directory: {e}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Drive staging path is not a private directory".to_string());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Could not secure Drive staging directory: {e}"))
    }

    fn sync_local_staging_for_pending(pending: &DrivePendingRecord) -> Result<(), String> {
        // fsync must make the still-local, not-yet-uploaded chunk data durable.
        // Uploading completed chunks alone is insufficient: a crash after
        // acknowledging fsync must not lose the unfinished final chunk.
        let dir = Path::new(&pending.staging_path)
            .parent()
            .ok_or_else(|| "Drive staging location has no parent directory".to_string())?;
        for entry in std::fs::read_dir(dir)
            .map_err(|e| format!("Cannot inspect pending Drive staging files: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Cannot inspect Drive staging entry: {e}"))?;
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|e| format!("Cannot inspect Drive staging file: {e}"))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err("Drive staging contains an unsafe non-file entry".to_string());
            }
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(entry.path())
                .and_then(|file| file.sync_all())
                .map_err(|e| format!("Could not sync local Drive staged bytes: {e}"))?;
        }
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("Could not sync Drive staging directory: {e}"))
    }

    fn allocated_stage_bytes(root: &Path) -> Result<u64, String> {
        let mut pending = vec![root.to_path_buf()];
        let mut total = 0u64;
        let mut scanned = 0usize;
        while let Some(dir) = pending.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|e| format!("Cannot inspect Drive write-staging budget: {e}"))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| format!("Cannot inspect Drive staging entry: {e}"))?;
                scanned += 1;
                if scanned > 100_000 {
                    return Err("Drive staging has too many entries to inspect safely".to_string());
                }
                let meta = entry
                    .path()
                    .symlink_metadata()
                    .map_err(|e| format!("Cannot stat Drive staging entry: {e}"))?;
                if meta.is_dir() {
                    pending.push(entry.path());
                } else {
                    total = total.saturating_add(meta.blocks().saturating_mul(512));
                }
            }
        }
        Ok(total)
    }

    fn staging_can_accept(used: u64, extra: u64) -> Result<(), String> {
        if used.saturating_add(extra) > STAGING_BUDGET_BYTES {
            return Err(
                "TeraRelay Drive local write-staging limit reached; finish or resume pending uploads before adding more writes"
                    .to_string(),
            );
        }
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    struct VirtualStatfs {
        blocks: u64,
        blocks_free: u64,
        blocks_available: u64,
        files: u64,
        files_free: u64,
        block_size: u32,
        name_length: u32,
        fragment_size: u32,
    }

    fn virtual_statfs_values() -> VirtualStatfs {
        // FUSE/statfs has no textual "unlimited" sentinel. Report the largest
        // signed-64-bit-safe virtual byte capacity so desktop file managers see
        // an effectively unlimited cloud filesystem, while write staging remains
        // independently bounded by STAGING_BUDGET_BYTES.
        const BLOCK_SIZE: u64 = 4096;
        let blocks = (i64::MAX as u64) / BLOCK_SIZE;
        let files = 1u64 << 50;
        VirtualStatfs {
            blocks,
            blocks_free: blocks,
            blocks_available: blocks,
            files,
            files_free: files,
            block_size: BLOCK_SIZE as u32,
            name_length: 255,
            fragment_size: BLOCK_SIZE as u32,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PendingStorageFormat {
        LegacyV1,
        PoolV2,
    }

    fn pending_storage_format(format_version: u32) -> Result<PendingStorageFormat, String> {
        match format_version {
            1 => Ok(PendingStorageFormat::LegacyV1),
            2 => Ok(PendingStorageFormat::PoolV2),
            version => Err(format!(
                "Unsupported TeraRelay Drive pending format version {version}"
            )),
        }
    }

    fn recorded_part_needs_reupload(
        recorded_first_chunk: u64,
        recorded_chunk_count: u64,
        expected_first_chunk: u64,
        expected_end_chunk: u64,
        dirty_ranges: &[(u64, u64)],
    ) -> bool {
        if recorded_first_chunk != expected_first_chunk
            || recorded_chunk_count != expected_end_chunk.saturating_sub(expected_first_chunk)
        {
            return true;
        }
        let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
        let start = expected_first_chunk.saturating_mul(chunk_size);
        let end = expected_end_chunk.saturating_mul(chunk_size);
        dirty_ranges
            .iter()
            .any(|(range_start, range_end)| *range_end > start && *range_start < end)
    }

    #[derive(Clone)]
    enum Node {
        Root,
        Directory(DriveDirectoryRecord),
        Remote(DriveFileRecord),
        Pending(DrivePendingRecord),
    }

    impl Node {
        fn inode(&self) -> u64 {
            match self {
                Node::Root => ROOT_INODE,
                Node::Directory(dir) => inode_for("dir", &dir.id),
                Node::Remote(file) => inode_for("file", &file.entry_id),
                Node::Pending(file) => file
                    .base_entry_id
                    .as_deref()
                    .map(|entry_id| inode_for("file", entry_id))
                    .unwrap_or_else(|| inode_for("pending", &file.id)),
            }
        }
    }

    fn inode_for(kind: &str, id: &str) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        hasher.update([0]);
        hasher.update(id.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        let value = u64::from_le_bytes(bytes) & 0x7fff_ffff_ffff_ffff;
        value.max(2)
    }

    fn millis_to_time(value: i64) -> SystemTime {
        if value <= 0 {
            return UNIX_EPOCH;
        }
        UNIX_EPOCH + Duration::from_millis(value as u64)
    }

    fn seconds_to_time(value: i64) -> SystemTime {
        if value <= 0 {
            return UNIX_EPOCH;
        }
        UNIX_EPOCH + Duration::from_secs(value as u64)
    }

    fn current_ids() -> (u32, u32) {
        // SAFETY: getuid/getgid are side-effect-free libc calls.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn attr_for(node: &Node) -> FileAttr {
        let (uid, gid) = current_ids();
        match node {
            Node::Root => FileAttr {
                ino: ROOT_INODE,
                size: 0,
                blocks: 0,
                atime: SystemTime::now(),
                mtime: SystemTime::now(),
                ctime: SystemTime::now(),
                crtime: SystemTime::now(),
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid,
                gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            },
            Node::Directory(dir) => {
                let created = millis_to_time(dir.created_at);
                let updated = millis_to_time(dir.updated_at);
                FileAttr {
                    ino: node.inode(),
                    size: 0,
                    blocks: 0,
                    atime: updated,
                    mtime: updated,
                    ctime: updated,
                    crtime: created,
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid,
                    gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
            Node::Remote(file) => {
                let size = file.plaintext_size.unwrap_or(file.total_size);
                let created = seconds_to_time(file.created_at);
                let updated = millis_to_time(file.updated_at);
                FileAttr {
                    ino: node.inode(),
                    size,
                    blocks: size.div_ceil(512),
                    atime: updated,
                    mtime: updated,
                    ctime: updated,
                    crtime: created,
                    kind: FileType::RegularFile,
                    perm: 0o644,
                    nlink: 1,
                    uid,
                    gid,
                    rdev: 0,
                    blksize: CACHE_BLOCK_SIZE as u32,
                    flags: 0,
                }
            }
            Node::Pending(file) => {
                // Production Drive writes use bounded per-chunk staging, so
                // there is intentionally no whole-file local inode to stat.
                let size = file.size;
                let created = millis_to_time(file.created_at);
                let updated = millis_to_time(file.updated_at);
                FileAttr {
                    ino: node.inode(),
                    size,
                    blocks: size.div_ceil(512),
                    atime: updated,
                    mtime: updated,
                    ctime: updated,
                    crtime: created,
                    kind: FileType::RegularFile,
                    perm: 0o644,
                    nlink: 1,
                    uid,
                    gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
        }
    }

    struct TeraRelayFs {
        db: DbConnection,
        app: AppHandle,
        stream_port: u16,
        stream_token: String,
        cache_root: PathBuf,
        staging_root: PathBuf,
        http: reqwest::blocking::Client,
        telegram: crate::commands::TelegramState,
        mount_state: DriveMountState,
        emitted_uploads: Mutex<std::collections::HashSet<String>>,
        writer_counts: Mutex<HashMap<u64, u32>>,
    }

    impl TeraRelayFs {
        fn snapshot(&self) -> Result<DriveSnapshot, String> {
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_snapshot(&conn)
        }

        fn node_for_inode(&self, ino: u64) -> Result<Option<Node>, String> {
            if ino == ROOT_INODE {
                return Ok(Some(Node::Root));
            }
            let snapshot = self.snapshot()?;
            for directory in snapshot.directories {
                let node = Node::Directory(directory);
                if node.inode() == ino {
                    return Ok(Some(node));
                }
            }
            // Copy-on-write replacements intentionally reuse the base file's
            // inode. Prefer the pending overlay before the remote base so every
            // inode operation observes the edited view until it is finalized or
            // cancelled.
            for file in snapshot.pending {
                let node = Node::Pending(file);
                if node.inode() == ino {
                    return Ok(Some(node));
                }
            }
            for file in snapshot.files {
                let node = Node::Remote(file);
                if node.inode() == ino {
                    return Ok(Some(node));
                }
            }
            Ok(None)
        }

        fn directory_id_for_inode(&self, ino: u64) -> Result<Option<Option<String>>, String> {
            match self.node_for_inode(ino)? {
                Some(Node::Root) => Ok(Some(None)),
                Some(Node::Directory(directory)) => Ok(Some(Some(directory.id))),
                Some(_) => Ok(None),
                None => Ok(None),
            }
        }

        fn parent_inode(&self, node: &Node, snapshot: &DriveSnapshot) -> u64 {
            let parent_id = match node {
                Node::Directory(dir) => dir.parent_id.as_deref(),
                Node::Remote(file) => file.directory_id.as_deref(),
                Node::Pending(file) => file.directory_id.as_deref(),
                Node::Root => return ROOT_INODE,
            };
            parent_id
                .and_then(|id| {
                    snapshot
                        .directories
                        .iter()
                        .find(|directory| directory.id == id)
                })
                .map(|directory| inode_for("dir", &directory.id))
                .unwrap_or(ROOT_INODE)
        }

        fn lookup_child(&self, parent: u64, name: &OsStr) -> Result<Option<Node>, String> {
            let Some(name) = name.to_str() else {
                return Ok(None);
            };
            let Some(parent_id) = self.directory_id_for_inode(parent)? else {
                return Ok(None);
            };
            let snapshot = self.snapshot()?;
            let same_parent = |candidate: Option<&str>| candidate == parent_id.as_deref();

            if let Some(directory) = snapshot.directories.into_iter().find(|directory| {
                same_parent(directory.parent_id.as_deref()) && directory.name == name
            }) {
                return Ok(Some(Node::Directory(directory)));
            }
            // A replacement pending record is the live view of its base
            // path. Resolve pending paths before remote files so reopening an
            // edited file continues the same copy-on-write transaction.
            if let Some(file) = snapshot
                .pending
                .iter()
                .find(|file| same_parent(file.directory_id.as_deref()) && file.display_name == name)
                .cloned()
            {
                return Ok(Some(Node::Pending(file)));
            }
            if let Some(file) = snapshot
                .files
                .into_iter()
                .find(|file| same_parent(file.directory_id.as_deref()) && file.display_name == name)
            {
                return Ok(Some(Node::Remote(file)));
            }
            Ok(None)
        }

        fn cache_path(&self, file_id: &str, block: u64) -> PathBuf {
            self.cache_root
                .join(file_id)
                .join(format!("{block:016x}.bin"))
        }

        fn fetch_stream_range(
            &self,
            file: &DriveFileRecord,
            start: u64,
            end: u64,
        ) -> Result<Vec<u8>, String> {
            if end < start {
                return Ok(Vec::new());
            }
            let url = format!(
                "http://127.0.0.1:{}/stream/{}/{}?token={}",
                self.stream_port,
                file.backing_channel_id,
                file.first_message_id,
                urlencoding::encode(&self.stream_token)
            );
            let response = self
                .http
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .map_err(|e| format!("TeraRelay Drive could not fetch file data: {e}"))?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            {
                return Err(format!(
                    "TeraRelay Drive stream returned HTTP {}",
                    response.status()
                ));
            }
            let bytes = response
                .bytes()
                .map_err(|e| format!("TeraRelay Drive stream ended early: {e}"))?
                .to_vec();
            let expected = end - start + 1;
            if bytes.len() as u64 != expected {
                return Err(format!(
                    "TeraRelay Drive expected {expected} bytes but received {}",
                    bytes.len()
                ));
            }
            Ok(bytes)
        }

        fn fetch_encrypted_plain_chunk(
            &self,
            file: &DriveFileRecord,
            crypto_chunk: u64,
        ) -> Result<Vec<u8>, String> {
            const ENCRYPTED_CHUNK_OVERHEAD: u64 = 5 + 24 + 16;
            let plaintext_size = file
                .plaintext_size
                .ok_or_else(|| "Encrypted Drive file is missing its plaintext size".to_string())?;
            let crypto_id = file
                .crypto_id
                .as_deref()
                .ok_or_else(|| "Encrypted Drive file is missing its crypto identity".to_string())?;
            let plain_start = crypto_chunk
                .checked_mul(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
                .ok_or_else(|| "Encrypted Drive offset overflow".to_string())?;
            if plain_start >= plaintext_size {
                return Ok(Vec::new());
            }
            let plain_len = std::cmp::min(
                crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE,
                plaintext_size - plain_start,
            );
            // Never cache decrypted content on disk: encrypted files must
            // remain encrypted even in the opportunistic read cache. Older
            // unreleased builds used crypto-*.bin for plaintext; ignore and
            // remove those files instead of ever reading them back.
            let old_plaintext_cache = self
                .cache_root
                .join(&file.file_id)
                .join(format!("crypto-{crypto_chunk:016x}.bin"));
            if old_plaintext_cache.exists() {
                let _ = std::fs::remove_file(&old_plaintext_cache);
            }
            let cache_path = self
                .cache_root
                .join(&file.file_id)
                .join(format!("cipher-{crypto_chunk:016x}.bin"));
            let cipher_start = crypto_chunk
                .checked_mul(
                    crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE
                        .saturating_add(ENCRYPTED_CHUNK_OVERHEAD),
                )
                .ok_or_else(|| "Encrypted Drive ciphertext offset overflow".to_string())?;
            let cipher_len = plain_len
                .checked_add(ENCRYPTED_CHUNK_OVERHEAD)
                .ok_or_else(|| "Encrypted Drive ciphertext length overflow".to_string())?;
            let encoded = match std::fs::read(&cache_path) {
                Ok(bytes) if bytes.len() as u64 == cipher_len => bytes,
                _ => self.fetch_stream_range(file, cipher_start, cipher_start + cipher_len - 1)?,
            };
            // Even cached ciphertext must be authenticated with the unlocked
            // master key. Locking the Drive must prevent plaintext reads.
            let key = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                crate::drive_crypto::load_verified_master_key_for_db(&self.app, &conn)?
            }
            .ok_or_else(|| {
                "TeraRelay Drive is locked. Unlock Drive encryption in Settings.".to_string()
            })?;
            let plaintext = crate::drive_crypto::decrypt_chunk(
                &*key,
                crypto_id,
                crypto_chunk,
                plain_len,
                &encoded,
            )?;
            if !cache_path.is_file() {
                if let Some(parent) = cache_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let temp = cache_path.with_extension("tmp");
                if std::fs::write(&temp, &encoded).is_ok() {
                    let _ = std::fs::rename(temp, &cache_path);
                    self.evict_cache_best_effort();
                }
            }
            Ok(plaintext)
        }

        fn fetch_v2_plain_crypto_chunk(
            &self,
            file: &DriveFileRecord,
            crypto_chunk: u64,
        ) -> Result<Vec<u8>, String> {
            if file.storage_version != 2 {
                return Err("Drive V2 reader received a legacy file".to_string());
            }
            let plaintext_size = file.plaintext_size.unwrap_or(file.total_size);
            let plain_start = crypto_chunk
                .checked_mul(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
                .ok_or_else(|| "Drive V2 plaintext offset overflow".to_string())?;
            if plain_start >= plaintext_size {
                return Ok(Vec::new());
            }
            let plain_len = std::cmp::min(
                crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE,
                plaintext_size - plain_start,
            );
            let (block, part) = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_object_block_location(&conn, &file.file_id, crypto_chunk)?
                    .ok_or_else(|| "Drive V2 block location is missing".to_string())?
            };
            if part.backing_channel_id <= 0
                || part.message_id <= 0
                || block.ciphertext_size == 0
                || block.part_offset.saturating_add(block.ciphertext_size) > part.ciphertext_size
            {
                return Err("Drive V2 block location is invalid".to_string());
            }

            // V2 cache stores the exact remote block representation. For encrypted
            // objects this is authenticated ciphertext only; plaintext never hits disk.
            let cache_path = self
                .cache_root
                .join(&file.file_id)
                .join(format!("v2-cipher-{crypto_chunk:016x}.bin"));
            let encoded = match std::fs::read(&cache_path) {
                Ok(bytes) if bytes.len() as u64 == block.ciphertext_size => bytes,
                _ => {
                    let message_id = i32::try_from(part.message_id)
                        .map_err(|_| "Drive V2 message ID is invalid".to_string())?;
                    let range_end = block
                        .part_offset
                        .checked_add(block.ciphertext_size)
                        .and_then(|value| value.checked_sub(1))
                        .ok_or_else(|| "Drive V2 byte range overflow".to_string())?;
                    let url = format!(
                        "http://127.0.0.1:{}/stream/{}/{}?token={}",
                        self.stream_port,
                        part.backing_channel_id,
                        message_id,
                        urlencoding::encode(&self.stream_token)
                    );
                    let response = self
                        .http
                        .get(url)
                        .header(
                            reqwest::header::RANGE,
                            format!("bytes={}-{}", block.part_offset, range_end),
                        )
                        .send()
                        .map_err(|e| format!("Could not fetch Drive V2 block: {e}"))?;
                    if !response.status().is_success()
                        && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
                    {
                        return Err(format!(
                            "Drive V2 block returned HTTP {}",
                            response.status()
                        ));
                    }
                    let bytes = response
                        .bytes()
                        .map_err(|e| format!("Drive V2 block ended early: {e}"))?
                        .to_vec();
                    if bytes.len() as u64 != block.ciphertext_size {
                        return Err(format!(
                            "Drive V2 expected {} block bytes but received {}",
                            block.ciphertext_size,
                            bytes.len()
                        ));
                    }
                    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
                    if actual_hash != block.sha256 {
                        return Err("Drive V2 block checksum mismatch".to_string());
                    }
                    if let Some(parent) = cache_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let temp = cache_path.with_extension("tmp");
                    if std::fs::write(&temp, &bytes).is_ok() {
                        let _ = std::fs::rename(&temp, &cache_path);
                        self.evict_cache_best_effort();
                    }
                    bytes
                }
            };

            // Authenticate cached bytes too, so corruption can never turn into plaintext.
            let actual_hash = format!("{:x}", Sha256::digest(&encoded));
            if actual_hash != block.sha256 {
                let _ = std::fs::remove_file(&cache_path);
                return Err("Drive V2 cached block checksum mismatch".to_string());
            }
            if file.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
                let key = {
                    let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                    crate::drive_crypto::load_verified_master_key_for_db(&self.app, &conn)?
                }
                .ok_or_else(|| {
                    "TeraRelay Drive is locked. Unlock Drive encryption in Settings.".to_string()
                })?;
                let crypto_id = file.crypto_id.as_deref().ok_or_else(|| {
                    "Encrypted Drive V2 file is missing its crypto identity".to_string()
                })?;
                return crate::drive_crypto::decrypt_chunk(
                    &*key,
                    crypto_id,
                    crypto_chunk,
                    plain_len,
                    &encoded,
                );
            }
            if file.encryption_version != 0 {
                return Err(format!(
                    "Unsupported TeraRelay Drive encryption version {}",
                    file.encryption_version
                ));
            }
            if encoded.len() as u64 != plain_len {
                return Err("Drive V2 plaintext block size is invalid".to_string());
            }
            Ok(encoded)
        }

        fn fetch_remote_block(
            &self,
            file: &DriveFileRecord,
            block: u64,
        ) -> Result<Vec<u8>, String> {
            let visible_size = file.plaintext_size.unwrap_or(file.total_size);
            let start = block
                .checked_mul(CACHE_BLOCK_SIZE)
                .ok_or_else(|| "Drive read offset overflow".to_string())?;
            if start >= visible_size {
                return Ok(Vec::new());
            }
            let end = std::cmp::min(
                visible_size.saturating_sub(1),
                start.saturating_add(CACHE_BLOCK_SIZE - 1),
            );

            if file.storage_version == 2 {
                let crypto_chunk = start / crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                let plaintext = self.fetch_v2_plain_crypto_chunk(file, crypto_chunk)?;
                let crypto_plain_start =
                    crypto_chunk * crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                let from = start.saturating_sub(crypto_plain_start) as usize;
                let to = std::cmp::min(
                    plaintext.len(),
                    end.saturating_sub(crypto_plain_start) as usize + 1,
                );
                if from > to || to > plaintext.len() {
                    return Err("Drive V2 cache range is invalid".to_string());
                }
                return Ok(plaintext[from..to].to_vec());
            }
            if file.storage_version != 1 {
                return Err(format!(
                    "Unsupported TeraRelay Drive storage version {}",
                    file.storage_version
                ));
            }

            if file.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
                let crypto_chunk = start / crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                let plaintext = self.fetch_encrypted_plain_chunk(file, crypto_chunk)?;
                let crypto_plain_start =
                    crypto_chunk * crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                let from = start.saturating_sub(crypto_plain_start) as usize;
                let to = std::cmp::min(
                    plaintext.len(),
                    end.saturating_sub(crypto_plain_start) as usize + 1,
                );
                if from > to || to > plaintext.len() {
                    return Err("Encrypted Drive cache range is invalid".to_string());
                }
                return Ok(plaintext[from..to].to_vec());
            }
            if file.encryption_version != 0 {
                return Err(format!(
                    "Unsupported TeraRelay Drive encryption version {}",
                    file.encryption_version
                ));
            }

            let path = self.cache_path(&file.file_id, block);
            if let Ok(bytes) = std::fs::read(&path) {
                if !bytes.is_empty() || start == end {
                    return Ok(bytes);
                }
            }
            let bytes = self.fetch_stream_range(file, start, end)?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let temp = path.with_extension("tmp");
            if std::fs::write(&temp, &bytes).is_ok() {
                let _ = std::fs::rename(&temp, &path);
                self.evict_cache_best_effort();
            }
            Ok(bytes)
        }

        fn evict_cache_best_effort(&self) {
            let Ok(file_dirs) = std::fs::read_dir(&self.cache_root) else {
                return;
            };
            let mut total = 0u64;
            let mut files = Vec::new();
            for file_dir in file_dirs.flatten() {
                let Ok(entries) = std::fs::read_dir(file_dir.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    if !metadata.is_file() {
                        continue;
                    }
                    total = total.saturating_add(metadata.len());
                    files.push((
                        metadata.modified().unwrap_or(UNIX_EPOCH),
                        metadata.len(),
                        entry.path(),
                    ));
                }
            }
            if total <= CACHE_MAX_BYTES {
                return;
            }
            files.sort_by_key(|(modified, _, _)| *modified);
            for (_, size, path) in files {
                if total <= CACHE_MAX_BYTES {
                    break;
                }
                if std::fs::remove_file(path).is_ok() {
                    total = total.saturating_sub(size);
                }
            }
        }

        fn read_remote_range(
            &self,
            file: &DriveFileRecord,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>, String> {
            let visible_size = file.plaintext_size.unwrap_or(file.total_size);
            if offset >= visible_size || size == 0 {
                return Ok(Vec::new());
            }
            let end_exclusive = std::cmp::min(visible_size, offset.saturating_add(size as u64));
            let first_block = offset / CACHE_BLOCK_SIZE;
            let last_block = (end_exclusive.saturating_sub(1)) / CACHE_BLOCK_SIZE;
            let mut output = Vec::with_capacity((end_exclusive - offset) as usize);

            for block in first_block..=last_block {
                let bytes = self.fetch_remote_block(file, block)?;
                let block_start = block * CACHE_BLOCK_SIZE;
                let from = offset.saturating_sub(block_start) as usize;
                let to = std::cmp::min(
                    bytes.len(),
                    end_exclusive.saturating_sub(block_start) as usize,
                );
                if from < to && to <= bytes.len() {
                    output.extend_from_slice(&bytes[from..to]);
                }
            }
            Ok(output)
        }

        fn emit_upload_once(&self, pending: &DrivePendingRecord) {
            let Ok(mut emitted) = self.emitted_uploads.lock() else {
                return;
            };
            if !emitted.insert(pending.id.clone()) {
                return;
            }
            let payload = DriveStagedUpload {
                pending_id: pending.id.clone(),
                path: pending.staging_path.clone(),
                folder_id: pending.backing_channel_id,
                file_name: pending.display_name.clone(),
            };
            if let Err(error) = self.app.emit("drive-upload-staged", payload) {
                emitted.remove(&pending.id);
                log::error!("Failed to enqueue TeraRelay Drive upload: {}", error);
            }
        }

        fn writer_opened(&self, ino: u64) {
            if let Ok(mut writers) = self.writer_counts.lock() {
                let count = writers.entry(ino).or_insert(0);
                *count = count.saturating_add(1);
            }
        }

        fn writer_closed_is_last(&self, ino: u64) -> bool {
            let Ok(mut writers) = self.writer_counts.lock() else {
                return true;
            };
            let Some(count) = writers.get_mut(&ino) else {
                return true;
            };
            if *count > 1 {
                *count -= 1;
                return false;
            }
            writers.remove(&ino);
            true
        }

        fn ensure_write_key_ready(&self) -> Result<(), String> {
            if crate::commands::qa_feature_a::enabled() {
                return Ok(());
            }
            let unlocked = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                let configured = crate::drive_crypto::load_crypto_envelope(&conn)?.is_some();
                !configured
                    || crate::drive_crypto::load_verified_master_key_for_db(&self.app, &conn)?
                        .is_some()
            };
            if !unlocked {
                return Err(
                    "TeraRelay Drive is locked. Unlock Drive encryption before writing files."
                        .to_string(),
                );
            }
            Ok(())
        }

        fn create_replacement_pending(
            &self,
            file: &DriveFileRecord,
        ) -> Result<DrivePendingRecord, String> {
            self.ensure_write_key_ready()?;
            let pending_id = crate::commands::drive_metadata::new_drive_id();
            let staging_dir = self.staging_root.join(&pending_id);
            ensure_private_staging_directory(&staging_dir)?;
            let marker = staging_dir.join(".pending");
            if let Err(error) = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&marker)
            {
                let _ = std::fs::remove_dir_all(&staging_dir);
                return Err(format!("Could not create Drive edit marker: {error}"));
            }
            let marker_string = marker.to_string_lossy().into_owned();
            let result = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                create_drive_pending_replacement(&conn, &pending_id, &file.entry_id, &marker_string)
            };
            match result {
                Ok(pending) => {
                    self.mount_state.set_pending_active(&pending.id, true);
                    Ok(pending)
                }
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    Err(error)
                }
            }
        }

        fn cancel_pending_sync(
            &self,
            pending: &DrivePendingRecord,
            emit_cancel_event: bool,
        ) -> Result<(), String> {
            if !crate::commands::qa_feature_a::enabled() {
                tauri::async_runtime::block_on(crate::drive_stream::cancel_pending_remote(
                    &self.telegram,
                    &self.db,
                    &pending.id,
                ))?;
            }

            let path = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                delete_drive_pending(&conn, &pending.id)?
            };
            self.mount_state.set_pending_active(&pending.id, false);

            let cleanup_path = path.unwrap_or_else(|| pending.staging_path.clone());
            let cleanup_path = PathBuf::from(cleanup_path);
            let cleanup_root = cleanup_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(cleanup_path);
            let _ = std::fs::remove_dir_all(cleanup_root);

            if emit_cancel_event {
                let _ = self.app.emit(
                    "drive-upload-cancelled",
                    DrivePendingCancelled {
                        pending_id: pending.id.clone(),
                    },
                );
            }
            Ok(())
        }

        fn pending_has_content_changes(
            &self,
            pending: &DrivePendingRecord,
        ) -> Result<bool, String> {
            let Some(base_entry_id) = pending.base_entry_id.as_deref() else {
                return Ok(true);
            };
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            let base = drive_file_by_entry_id(&conn, base_entry_id)?
                .ok_or_else(|| "Drive source file is no longer available".to_string())?;
            let base_size = base.plaintext_size.unwrap_or(base.total_size);
            if pending.size != base_size {
                return Ok(true);
            }
            if !drive_pending_ranges(&conn, &pending.id)?.is_empty() {
                return Ok(true);
            }
            Ok(!drive_pending_chunks(&conn, &pending.id)?.is_empty())
        }

        fn pending_for_inode(&self, ino: u64) -> Result<Option<DrivePendingRecord>, String> {
            let snapshot = self.snapshot()?;
            for pending in snapshot.pending {
                if inode_for("pending", &pending.id) == ino
                    || pending
                        .base_entry_id
                        .as_deref()
                        .map(|entry_id| inode_for("file", entry_id) == ino)
                        .unwrap_or(false)
                {
                    return Ok(Some(pending));
                }
            }
            Ok(None)
        }

        fn base_file_for_pending(
            &self,
            pending: &DrivePendingRecord,
        ) -> Result<Option<DriveFileRecord>, String> {
            let Some(entry_id) = pending.base_entry_id.as_deref() else {
                return Ok(None);
            };
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_file_by_entry_id(&conn, entry_id)
        }

        fn pending_chunk_len(&self, pending: &DrivePendingRecord, chunk_index: u64) -> u64 {
            let start = chunk_index.saturating_mul(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
            if pending.size <= start {
                return 0;
            }
            std::cmp::min(
                crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE,
                pending.size - start,
            )
        }

        fn range_is_covered(&self, pending_id: &str, start: u64, end: u64) -> Result<bool, String> {
            if end <= start {
                return Ok(true);
            }
            let ranges = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_pending_ranges(&conn, pending_id)?
            };
            let mut cursor = start;
            for (range_start, range_end) in ranges {
                if range_end <= cursor || range_start > cursor {
                    if range_start > cursor {
                        break;
                    }
                    continue;
                }
                cursor = cursor.max(range_end);
                if cursor >= end {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        fn fetch_pending_uploaded_chunk(
            &self,
            pending: &DrivePendingRecord,
            chunk_index: u64,
        ) -> Result<Option<Vec<u8>>, String> {
            let (record, part, part_offset) = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                let chunks = drive_pending_chunks(&conn, &pending.id)?;
                let record = chunks
                    .iter()
                    .find(|chunk| chunk.chunk_index == chunk_index)
                    .cloned();
                let Some(record) = record else {
                    return Ok(None);
                };
                let part = drive_pending_parts(&conn, &pending.id)?
                    .into_iter()
                    .find(|part| {
                        chunk_index >= part.first_chunk_index
                            && chunk_index < part.first_chunk_index.saturating_add(part.chunk_count)
                            && record.message_id == part.message_id
                    });
                let offset = if let Some(part) = part.as_ref() {
                    chunks
                        .iter()
                        .filter(|chunk| {
                            chunk.message_id == part.message_id
                                && chunk.chunk_index >= part.first_chunk_index
                                && chunk.chunk_index < chunk_index
                        })
                        .try_fold(0u64, |sum, chunk| {
                            sum.checked_add(chunk.chunk_size)
                                .ok_or_else(|| "Drive pending part offset overflow".to_string())
                        })?
                } else {
                    0
                };
                (record, part, offset)
            };
            let message_id = i32::try_from(record.message_id)
                .map_err(|_| "Drive chunk message ID is invalid".to_string())?;
            let backing_channel_id = part
                .as_ref()
                .map(|part| part.backing_channel_id)
                .unwrap_or(pending.backing_channel_id);
            let url = format!(
                "http://127.0.0.1:{}/stream/{}/{}?token={}",
                self.stream_port,
                backing_channel_id,
                message_id,
                urlencoding::encode(&self.stream_token)
            );
            let mut request = self.http.get(url);
            if part.is_some() {
                let end = part_offset
                    .checked_add(record.chunk_size)
                    .and_then(|value| value.checked_sub(1))
                    .ok_or_else(|| "Drive pending part byte range overflow".to_string())?;
                request =
                    request.header(reqwest::header::RANGE, format!("bytes={part_offset}-{end}"));
            }
            let response = request
                .send()
                .map_err(|e| format!("Could not fetch pending Drive chunk: {e}"))?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            {
                return Err(format!(
                    "Pending Drive chunk returned HTTP {}",
                    response.status()
                ));
            }
            let encoded = response
                .bytes()
                .map_err(|e| format!("Pending Drive chunk ended early: {e}"))?
                .to_vec();
            if part.is_some() && encoded.len() as u64 != record.chunk_size {
                return Err(format!(
                    "Pending Drive part expected {} bytes but received {}",
                    record.chunk_size,
                    encoded.len()
                ));
            }
            if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
                let key = {
                    let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                    crate::drive_crypto::load_verified_master_key_for_db(&self.app, &conn)?
                }
                .ok_or_else(|| {
                    "TeraRelay Drive is locked. Unlock Drive encryption in Settings.".to_string()
                })?;
                let crypto_id = pending.crypto_id.as_deref().ok_or_else(|| {
                    "Encrypted pending Drive file has no crypto identity".to_string()
                })?;
                let plain_len = self.pending_chunk_len(pending, chunk_index);
                return Ok(Some(crate::drive_crypto::decrypt_chunk(
                    &*key,
                    crypto_id,
                    chunk_index,
                    plain_len,
                    &encoded,
                )?));
            }
            if pending.size == 0 {
                return Ok(Some(Vec::new()));
            }
            Ok(Some(encoded))
        }

        fn materialize_pending_chunk(
            &self,
            pending: &DrivePendingRecord,
            chunk_index: u64,
        ) -> Result<Vec<u8>, String> {
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let chunk_start = chunk_index
                .checked_mul(chunk_size)
                .ok_or_else(|| "Drive chunk offset overflow".to_string())?;
            let chunk_len = self.pending_chunk_len(pending, chunk_index);
            if chunk_len == 0 {
                return Ok(Vec::new());
            }

            let mut output =
                if let Some(uploaded) = self.fetch_pending_uploaded_chunk(pending, chunk_index)? {
                    let mut bytes = uploaded;
                    bytes.resize(chunk_len as usize, 0);
                    bytes
                } else if let Some(base) = self.base_file_for_pending(pending)? {
                    let read_size = u32::try_from(chunk_len)
                        .map_err(|_| "Drive chunk is too large to materialize".to_string())?;
                    let mut bytes = self.read_remote_range(&base, chunk_start, read_size)?;
                    bytes.resize(chunk_len as usize, 0);
                    bytes
                } else {
                    vec![0u8; chunk_len as usize]
                };

            let local_path = crate::drive_stream::local_chunk_path(pending, chunk_index);
            if local_path.exists() {
                let local = File::open(&local_path)
                    .map_err(|e| format!("Could not open Drive write chunk: {e}"))?;
                let ranges = {
                    let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                    drive_pending_ranges(&conn, &pending.id)?
                };
                for (start, end) in ranges {
                    let overlap_start = start.max(chunk_start);
                    let overlap_end = end.min(chunk_start + chunk_len);
                    if overlap_end <= overlap_start {
                        continue;
                    }
                    let local_offset = overlap_start - chunk_start;
                    let length = (overlap_end - overlap_start) as usize;
                    let mut bytes = vec![0u8; length];
                    let read = local
                        .read_at(&mut bytes, local_offset)
                        .map_err(|e| format!("Could not read Drive write chunk: {e}"))?;
                    if read != length {
                        return Err("Drive write chunk is incomplete".to_string());
                    }
                    output[local_offset as usize..local_offset as usize + length]
                        .copy_from_slice(&bytes);
                }
            }
            Ok(output)
        }

        fn read_pending_range(
            &self,
            pending: &DrivePendingRecord,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>, String> {
            if offset >= pending.size || size == 0 {
                return Ok(Vec::new());
            }
            let end = std::cmp::min(pending.size, offset.saturating_add(size as u64));
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let first = offset / chunk_size;
            let last = (end - 1) / chunk_size;
            let mut output = Vec::with_capacity((end - offset) as usize);
            for index in first..=last {
                let chunk = self.materialize_pending_chunk(pending, index)?;
                let chunk_start = index * chunk_size;
                let from = offset.saturating_sub(chunk_start) as usize;
                let to = std::cmp::min(chunk.len(), (end - chunk_start) as usize);
                if from < to {
                    output.extend_from_slice(&chunk[from..to]);
                }
            }
            Ok(output)
        }

        fn write_pending_bytes(
            &self,
            pending: &DrivePendingRecord,
            offset: u64,
            data: &[u8],
        ) -> Result<DrivePendingRecord, String> {
            if data.is_empty() {
                return Ok(pending.clone());
            }
            // Count actual allocated blocks, not logical sparse-file sizes.
            // Full chunks are flushed to Telegram and their staging files
            // removed, so arbitrarily large sequential copies do not need
            // their full size on the local disk. Check before every write to
            // bound scatter/random writes as well, including after recovery.
            // FUSE invokes write through &mut self, so these checks and the
            // matching filesystem write cannot interleave with another write.
            let allocated = allocated_stage_bytes(&self.staging_root)?;
            staging_can_accept(allocated, data.len() as u64)?;
            let end = offset
                .checked_add(data.len() as u64)
                .ok_or_else(|| "Drive write offset overflow".to_string())?;
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let mut consumed = 0usize;
            while consumed < data.len() {
                let absolute = offset + consumed as u64;
                let chunk_index = absolute / chunk_size;
                let chunk_offset = absolute % chunk_size;
                let take =
                    std::cmp::min(data.len() - consumed, (chunk_size - chunk_offset) as usize);
                let path = crate::drive_stream::local_chunk_path(pending, chunk_index);
                if let Some(parent) = path.parent() {
                    ensure_private_staging_directory(parent)?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|e| format!("Could not open Drive chunk staging: {e}"))?;
                file.write_at(&data[consumed..consumed + take], chunk_offset)
                    .map_err(|e| format!("Could not write Drive chunk staging: {e}"))?;
                consumed += take;
            }

            {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                record_drive_pending_range(&conn, &pending.id, offset, end)?;
                update_drive_pending_size(&conn, &pending.id, pending.size.max(end))?;
            }
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_pending_by_id(&conn, &pending.id)?
                .ok_or_else(|| "Drive pending write disappeared".to_string())
        }

        fn resize_pending(
            &self,
            pending: &DrivePendingRecord,
            new_size: u64,
        ) -> Result<DrivePendingRecord, String> {
            if new_size == pending.size {
                return Ok(pending.clone());
            }

            // The isolated QA profile deliberately stages a complete local
            // file so its existing no-Telegram upload-queue harness can
            // operate on a real path. It must resize that same local file,
            // rather than changing only the production chunk/range metadata.
            if crate::commands::qa_feature_a::enabled() {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&pending.staging_path)
                    .map_err(|e| format!("Could not open QA Drive staging file: {e}"))?;
                file.set_len(new_size)
                    .map_err(|e| format!("Could not resize QA Drive staging file: {e}"))?;
                file.sync_all()
                    .map_err(|e| format!("Could not sync QA Drive resized file: {e}"))?;
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                update_drive_pending_size(&conn, &pending.id, new_size)?;
                return drive_pending_by_id(&conn, &pending.id)?
                    .ok_or_else(|| "Drive pending write disappeared".to_string());
            }

            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let old_size = pending.size;
            let old_last = old_size.checked_sub(1).map(|value| value / chunk_size);
            let new_last = new_size.checked_sub(1).map(|value| value / chunk_size);
            let uploaded = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_pending_chunks(&conn, &pending.id)?
            };

            // If resize changes the plaintext length of an already-uploaded
            // boundary chunk, preserve its old plaintext locally before its
            // authenticated remote representation is invalidated. This keeps
            // ftruncate semantics correct without materializing the whole file.
            let changing_boundary = if new_size < old_size {
                new_last.filter(|_| new_size % chunk_size != 0)
            } else if old_size > 0 && old_size % chunk_size != 0 {
                old_last
            } else {
                None
            };
            let mut preserved_boundary: Option<(u64, Vec<u8>)> = None;
            if let Some(index) = changing_boundary {
                if uploaded.iter().any(|chunk| chunk.chunk_index == index) {
                    let mut bytes = self.materialize_pending_chunk(pending, index)?;
                    let chunk_start = index * chunk_size;
                    let new_len = new_size.saturating_sub(chunk_start).min(chunk_size) as usize;
                    bytes.resize(new_len, 0);
                    preserved_boundary = Some((index, bytes));
                }
            }

            let remove_from = if new_size < old_size {
                if new_size == 0 {
                    Some(0)
                } else if new_size % chunk_size == 0 {
                    Some(new_size / chunk_size)
                } else {
                    new_last
                }
            } else {
                changing_boundary
                    .filter(|index| uploaded.iter().any(|chunk| chunk.chunk_index == *index))
            };

            let (removed_chunks, removed_parts, had_v2_parts) = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                if new_size < old_size {
                    clear_drive_pending_ranges_for_span(&conn, &pending.id, new_size, old_size)?;
                }
                let had_v2_parts = !drive_pending_parts(&conn, &pending.id)?.is_empty();
                let removed_chunks = match remove_from {
                    Some(index) => take_drive_pending_chunks_from(&conn, &pending.id, index)?,
                    None => Vec::new(),
                };
                // Preserve the part that overlaps the resize boundary as a
                // crash-safe read source until its replacement upload commits.
                // Parts that start at/after the removed tail are no longer
                // referenced by the pending layout and can be retired now.
                let removed_parts = if had_v2_parts {
                    match remove_from {
                        Some(index) => {
                            take_drive_pending_parts_starting_at_chunk(&conn, &pending.id, index)?
                        }
                        None => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                update_drive_pending_size(&conn, &pending.id, new_size)?;
                (removed_chunks, removed_parts, had_v2_parts)
            };

            if let Some((index, bytes)) = preserved_boundary {
                let path = crate::drive_stream::local_chunk_path(pending, index);
                if let Some(parent) = path.parent() {
                    ensure_private_staging_directory(parent)?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|e| format!("Could not stage resized Drive chunk: {e}"))?;
                file.write_at(&bytes, 0)
                    .map_err(|e| format!("Could not preserve resized Drive chunk: {e}"))?;
                let start = index * chunk_size;
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                record_drive_pending_range(&conn, &pending.id, start, start + bytes.len() as u64)?;
            }

            if new_size < old_size {
                let first_removed_local = new_size.div_ceil(chunk_size);
                let old_chunks = old_size.div_ceil(chunk_size);
                for index in first_removed_local..old_chunks {
                    let _ =
                        std::fs::remove_file(crate::drive_stream::local_chunk_path(pending, index));
                }
                if new_size > 0 && new_size % chunk_size != 0 {
                    if let Some(index) = new_last {
                        let path = crate::drive_stream::local_chunk_path(pending, index);
                        if path.exists() {
                            let _ = OpenOptions::new()
                                .write(true)
                                .open(path)
                                .and_then(|file| file.set_len(new_size - index * chunk_size));
                        }
                    }
                }
            }

            if !crate::commands::qa_feature_a::enabled() {
                if !removed_parts.is_empty() {
                    tauri::async_runtime::block_on(
                        crate::drive_stream::delete_pending_part_records(
                            &self.telegram,
                            &removed_parts,
                        ),
                    );
                }
                // Legacy pending uploads have one Telegram message per chunk.
                // V2 chunks are block metadata inside larger remote parts, so
                // deleting by chunk message ID would risk deleting the preserved
                // boundary part that is still needed as the rewrite source.
                if !had_v2_parts && !removed_chunks.is_empty() {
                    tauri::async_runtime::block_on(
                        crate::drive_stream::delete_pending_chunk_records(
                            &self.telegram,
                            pending.backing_channel_id,
                            &removed_chunks,
                        ),
                    );
                }
            }

            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_pending_by_id(&conn, &pending.id)?
                .ok_or_else(|| "Drive pending write disappeared".to_string())
        }

        fn clear_durable_part_local(
            &self,
            pending: &DrivePendingRecord,
            first_chunk: u64,
            end_chunk: u64,
        ) -> Result<(), String> {
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            for chunk_index in first_chunk..end_chunk {
                let start = chunk_index
                    .checked_mul(chunk_size)
                    .ok_or_else(|| "Drive chunk offset overflow".to_string())?;
                let len = self.pending_chunk_len(pending, chunk_index);
                if len > 0 {
                    let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                    clear_drive_pending_ranges_for_span(&conn, &pending.id, start, start + len)?;
                    drop(conn);
                }
                let _ = std::fs::remove_file(crate::drive_stream::local_chunk_path(
                    pending,
                    chunk_index,
                ));
            }
            Ok(())
        }

        fn upload_pending_part_sync(
            &self,
            pending: &DrivePendingRecord,
            part_index: u64,
            first_chunk: u64,
            end_chunk: u64,
        ) -> Result<(), String> {
            let (existing, dirty_ranges) = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                (
                    drive_pending_parts(&conn, &pending.id)?
                        .into_iter()
                        .find(|part| part.part_index == part_index),
                    drive_pending_ranges(&conn, &pending.id)?,
                )
            };
            if let Some(part) = existing.as_ref() {
                if part.backing_channel_id <= 0 || part.message_id <= 0 {
                    return Err("Recorded Drive remote part location is invalid".to_string());
                }
                if !recorded_part_needs_reupload(
                    part.first_chunk_index,
                    part.chunk_count,
                    first_chunk,
                    end_chunk,
                    &dirty_ranges,
                ) {
                    // Crash/restart case: an unchanged matching DB record proves
                    // the remote part is durable, so leftover plaintext staging
                    // can be released without another Telegram upload.
                    return self.clear_durable_part_local(pending, first_chunk, end_chunk);
                }
                // Dirty bytes or a resize changed this part. Keep the old remote
                // message readable while materializing unchanged blocks below;
                // upload_encoded_part replaces it only after the new send + DB
                // commit succeeds, so a crash cannot discard the last good copy.
            }

            let mut blocks = Vec::with_capacity(end_chunk.saturating_sub(first_chunk) as usize);
            for chunk_index in first_chunk..end_chunk {
                let plaintext = self.materialize_pending_chunk(pending, chunk_index)?;
                blocks.push(crate::drive_stream::encode_pending_plaintext_block(
                    &self.app,
                    &self.db,
                    pending,
                    chunk_index,
                    plaintext,
                )?);
            }
            tauri::async_runtime::block_on(crate::drive_stream::upload_encoded_part(
                &self.app,
                &self.telegram,
                &self.db,
                &pending.id,
                part_index,
                blocks,
            ))?;
            // upload_encoded_part returns only after the part and all block
            // metadata commit. Never release local plaintext before that point.
            self.clear_durable_part_local(pending, first_chunk, end_chunk)
        }

        fn flush_completed_chunks(
            &self,
            pending: &DrivePendingRecord,
            first_chunk: u64,
            last_chunk: u64,
        ) -> Result<(), String> {
            if crate::commands::qa_feature_a::enabled() {
                return Ok(());
            }
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            match pending_storage_format(pending.format_version)? {
                PendingStorageFormat::LegacyV1 => {
                    let recorded = {
                        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                        drive_pending_chunks(&conn, &pending.id)?
                    };
                    let dirty_ranges = {
                        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                        drive_pending_ranges(&conn, &pending.id)?
                    };
                    for chunk_index in first_chunk..=last_chunk {
                        let start = chunk_index
                            .checked_mul(chunk_size)
                            .ok_or_else(|| "Drive chunk offset overflow".to_string())?;
                        let len = self.pending_chunk_len(pending, chunk_index);
                        let end = start.saturating_add(len);
                        let is_dirty = dirty_ranges.iter().any(|(range_start, range_end)| {
                            *range_end > start && *range_start < end
                        });
                        if recorded
                            .iter()
                            .any(|chunk| chunk.chunk_index == chunk_index)
                            && !is_dirty
                        {
                            if len > 0 {
                                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                                clear_drive_pending_ranges_for_span(
                                    &conn,
                                    &pending.id,
                                    start,
                                    end,
                                )?;
                            }
                            let _ = std::fs::remove_file(crate::drive_stream::local_chunk_path(
                                pending,
                                chunk_index,
                            ));
                            continue;
                        }
                        if pending.closed_at.is_none()
                            && (len != chunk_size
                                || !self.range_is_covered(&pending.id, start, end)?)
                        {
                            continue;
                        }
                        let plaintext = self.materialize_pending_chunk(pending, chunk_index)?;
                        tauri::async_runtime::block_on(
                            crate::drive_stream::upload_plaintext_chunk(
                                &self.app,
                                &self.telegram,
                                &self.db,
                                &pending.id,
                                chunk_index,
                                plaintext,
                            ),
                        )?;
                        if len > 0 {
                            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                            clear_drive_pending_ranges_for_span(&conn, &pending.id, start, end)?;
                        }
                        let _ = std::fs::remove_file(crate::drive_stream::local_chunk_path(
                            pending,
                            chunk_index,
                        ));
                    }
                }
                PendingStorageFormat::PoolV2 => {
                    for (part_index, part_first, part_end) in
                        crate::drive_stream::ready_remote_parts(
                            pending.size,
                            pending.closed_at.is_some(),
                        )
                    {
                        if part_end <= first_chunk || part_first > last_chunk {
                            continue;
                        }
                        let already_recorded = {
                            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                            drive_pending_parts(&conn, &pending.id)?
                                .iter()
                                .any(|part| part.part_index == part_index)
                        };
                        if !already_recorded && pending.closed_at.is_none() {
                            let mut complete = true;
                            for chunk_index in part_first..part_end {
                                let start = chunk_index
                                    .checked_mul(chunk_size)
                                    .ok_or_else(|| "Drive chunk offset overflow".to_string())?;
                                let len = self.pending_chunk_len(pending, chunk_index);
                                if len != chunk_size
                                    || !self.range_is_covered(&pending.id, start, start + len)?
                                {
                                    complete = false;
                                    break;
                                }
                            }
                            if !complete {
                                continue;
                            }
                        }
                        self.upload_pending_part_sync(pending, part_index, part_first, part_end)?;
                    }
                }
            }
            Ok(())
        }

        fn finalize_pending_sync(&self, pending_id: &str) -> Result<(), String> {
            if crate::commands::qa_feature_a::enabled() {
                let pending = {
                    let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                    drive_pending_by_id(&conn, pending_id)?
                        .ok_or_else(|| "Drive pending write disappeared".to_string())?
                };
                // Existing-file edits stay as local pending replacements in the
                // isolated no-Telegram QA mount. This lets us validate normal
                // FUSE read/write/truncate semantics without pretending a real
                // remote replacement completed. Unlinking the pending path
                // cleanly reveals the untouched seeded remote file again.
                if pending.base_entry_id.is_some() {
                    return Ok(());
                }
                self.emit_upload_once(&pending);
                return Ok(());
            }

            let pending = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                mark_drive_pending_closed(&conn, pending_id)?;
                drive_pending_by_id(&conn, pending_id)?
                    .ok_or_else(|| "Drive pending write disappeared".to_string())?
            };
            let chunk_count = if pending.size == 0 {
                1
            } else {
                pending
                    .size
                    .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
            };
            self.flush_completed_chunks(&pending, 0, chunk_count.saturating_sub(1))?;
            match pending_storage_format(pending.format_version)? {
                PendingStorageFormat::PoolV2 => {
                    tauri::async_runtime::block_on(crate::drive_stream::finalize_pending_v2(
                        &self.app,
                        &self.telegram,
                        &self.db,
                        pending_id,
                    ))?;
                }
                PendingStorageFormat::LegacyV1 => {
                    // Existing pending writes created by older builds keep their
                    // original one-message-per-block recovery/finalization path.
                    tauri::async_runtime::block_on(crate::drive_stream::finalize_pending(
                        &self.app,
                        &self.telegram,
                        &self.db,
                        pending_id,
                    ))?;
                }
            }
            Ok(())
        }
    }

    impl Filesystem for TeraRelayFs {
        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            match self.lookup_child(parent, name) {
                Ok(Some(node)) => reply.entry(&TTL, &attr_for(&node), 0),
                Ok(None) => reply.error(ENOENT),
                Err(error) => {
                    log::warn!("Drive lookup failed: {}", error);
                    reply.error(EIO);
                }
            }
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
            match self.node_for_inode(ino) {
                Ok(Some(node)) => reply.attr(&TTL, &attr_for(&node)),
                Ok(None) => reply.error(ENOENT),
                Err(error) => {
                    log::warn!("Drive getattr failed: {}", error);
                    reply.error(EIO);
                }
            }
        }

        fn readdir(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let snapshot = match self.snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    log::warn!("Drive readdir failed: {}", error);
                    reply.error(EIO);
                    return;
                }
            };
            let node = match self.node_for_inode(ino) {
                Ok(Some(node @ Node::Root)) | Ok(Some(node @ Node::Directory(_))) => node,
                Ok(Some(_)) => {
                    reply.error(ENOTDIR);
                    return;
                }
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let parent_id = match &node {
                Node::Root => None,
                Node::Directory(directory) => Some(directory.id.as_str()),
                _ => None,
            };
            let mut entries: Vec<(u64, FileType, String)> = Vec::new();
            entries.push((ino, FileType::Directory, ".".to_string()));
            entries.push((
                self.parent_inode(&node, &snapshot),
                FileType::Directory,
                "..".to_string(),
            ));
            for directory in &snapshot.directories {
                if directory.parent_id.as_deref() == parent_id {
                    entries.push((
                        inode_for("dir", &directory.id),
                        FileType::Directory,
                        directory.name.clone(),
                    ));
                }
            }
            let replaced_entry_ids: std::collections::HashSet<&str> = snapshot
                .pending
                .iter()
                .filter_map(|pending| pending.base_entry_id.as_deref())
                .collect();
            for file in &snapshot.files {
                if file.directory_id.as_deref() == parent_id
                    && !replaced_entry_ids.contains(file.entry_id.as_str())
                {
                    entries.push((
                        inode_for("file", &file.entry_id),
                        FileType::RegularFile,
                        file.display_name.clone(),
                    ));
                }
            }
            for file in &snapshot.pending {
                if file.directory_id.as_deref() == parent_id {
                    entries.push((
                        Node::Pending(file.clone()).inode(),
                        FileType::RegularFile,
                        file.display_name.clone(),
                    ));
                }
            }

            for (index, (child_ino, kind, name)) in
                entries.into_iter().enumerate().skip(offset.max(0) as usize)
            {
                if reply.add(child_ino, (index + 1) as i64, kind, name) {
                    break;
                }
            }
            reply.ok();
        }

        fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
            match self.node_for_inode(ino) {
                Ok(Some(Node::Directory(_))) | Ok(Some(Node::Root)) => reply.error(EISDIR),
                Ok(Some(Node::Remote(file))) => {
                    if flags & libc::O_ACCMODE == libc::O_RDONLY {
                        reply.opened(0, 0);
                        return;
                    }
                    let pending = match self.create_replacement_pending(&file) {
                        Ok(pending) => pending,
                        Err(error) => {
                            log::warn!("Drive copy-on-write open failed: {}", error);
                            reply.error(if error.contains("locked") {
                                EACCES
                            } else {
                                EIO
                            });
                            return;
                        }
                    };
                    if flags & libc::O_TRUNC != 0 {
                        if let Err(error) = self.resize_pending(&pending, 0) {
                            self.mount_state.set_pending_active(&pending.id, false);
                            if let Ok(conn) = self.db.lock() {
                                let _ = delete_drive_pending(&conn, &pending.id);
                            }
                            let _ = std::fs::remove_dir_all(
                                Path::new(&pending.staging_path)
                                    .parent()
                                    .unwrap_or_else(|| Path::new(&pending.staging_path)),
                            );
                            log::warn!("Drive truncate-on-open failed: {}", error);
                            reply.error(EIO);
                            return;
                        }
                    }
                    self.writer_opened(ino);
                    reply.opened(0, 0);
                }
                Ok(Some(Node::Pending(pending))) => {
                    if flags & libc::O_ACCMODE != libc::O_RDONLY {
                        if let Err(error) = self.ensure_write_key_ready() {
                            log::warn!("Drive pending open failed: {}", error);
                            reply.error(if error.contains("locked") {
                                EACCES
                            } else {
                                EIO
                            });
                            return;
                        }
                        self.mount_state.set_pending_active(&pending.id, true);
                        self.writer_opened(ino);
                    }
                    reply.opened(0, 0);
                }
                Ok(None) => reply.error(ENOENT),
                Err(_) => reply.error(EIO),
            }
        }

        fn read(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            if offset < 0 {
                reply.error(EIO);
                return;
            }
            match self.pending_for_inode(ino) {
                Ok(Some(pending)) => {
                    if crate::commands::qa_feature_a::enabled() {
                        match File::open(&pending.staging_path) {
                            Ok(local) => {
                                let mut data = vec![0u8; size as usize];
                                match local.read_at(&mut data, offset as u64) {
                                    Ok(read) => {
                                        data.truncate(read);
                                        reply.data(&data);
                                    }
                                    Err(_) => reply.error(EIO),
                                }
                            }
                            Err(_) => reply.error(ENOENT),
                        }
                    } else {
                        match self.read_pending_range(&pending, offset as u64, size) {
                            Ok(data) => reply.data(&data),
                            Err(error) => {
                                log::warn!("Drive pending read failed: {}", error);
                                reply.error(EIO);
                            }
                        }
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!("Drive pending lookup failed: {}", error);
                    reply.error(EIO);
                    return;
                }
            }

            match self.node_for_inode(ino) {
                Ok(Some(Node::Remote(file))) => {
                    match self.read_remote_range(&file, offset as u64, size) {
                        Ok(data) => reply.data(&data),
                        Err(error) => {
                            log::warn!(
                                "TeraRelay Drive read failed for {}: {}",
                                file.display_name,
                                error
                            );
                            reply.error(EIO);
                        }
                    }
                }
                Ok(Some(Node::Directory(_))) | Ok(Some(Node::Root)) => reply.error(EISDIR),
                Ok(Some(Node::Pending(_))) => reply.error(EIO),
                Ok(None) => reply.error(ENOENT),
                Err(_) => reply.error(EIO),
            }
        }

        fn create(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            flags: i32,
            reply: ReplyCreate,
        ) {
            let Some(name) = name.to_str() else {
                reply.error(EIO);
                return;
            };
            if self
                .lookup_child(parent, OsStr::new(name))
                .ok()
                .flatten()
                .is_some()
            {
                reply.error(EEXIST);
                return;
            }
            let Some(directory_id) = self.directory_id_for_inode(parent).ok().flatten() else {
                reply.error(ENOTDIR);
                return;
            };

            if let Err(error) = self.ensure_write_key_ready() {
                log::warn!("Drive create encryption check failed: {}", error);
                reply.error(if error.contains("locked") {
                    EACCES
                } else {
                    EIO
                });
                return;
            }

            let pending_id = crate::commands::drive_metadata::new_drive_id();
            let staging_dir = self.staging_root.join(&pending_id);
            if let Err(error) = ensure_private_staging_directory(&staging_dir) {
                log::warn!("Drive staging directory create failed: {}", error);
                reply.error(EIO);
                return;
            }
            // QA retains the legacy whole-file source so the isolated
            // no-Telegram harness can exercise the existing upload queue. Real
            // Drive writes use only a tiny marker plus bounded 16 MiB chunks.
            let staging_path = if crate::commands::qa_feature_a::enabled() {
                staging_dir.join(name)
            } else {
                staging_dir.join(".pending")
            };
            let staging_string = staging_path.to_string_lossy().into_owned();
            let pending = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                match create_drive_pending_file(
                    &conn,
                    &pending_id,
                    directory_id.as_deref(),
                    name,
                    &staging_string,
                ) {
                    Ok(record) => record,
                    Err(error) => {
                        log::warn!("Drive create failed: {}", error);
                        reply.error(if error.contains("already exists") {
                            EEXIST
                        } else {
                            EIO
                        });
                        return;
                    }
                }
            };
            if let Err(error) = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&staging_path)
            {
                if let Ok(conn) = self.db.lock() {
                    let _ = delete_drive_pending(&conn, &pending.id);
                }
                log::warn!("Drive staging create failed: {}", error);
                reply.error(EIO);
                return;
            }
            let node = Node::Pending(pending);
            if let Node::Pending(pending) = &node {
                self.mount_state.set_pending_active(&pending.id, true);
            }
            self.writer_opened(node.inode());
            // ReplyCreate's final value is a set of FUSE_FOPEN_* response
            // flags, not the Linux O_* request flags received above. Returning
            // O_CREAT/O_EXCL here makes the kernel reject an otherwise
            // successful create with EIO, leaving a misleading zero-byte
            // directory entry behind.
            let _ = flags;
            reply.created(&TTL, &attr_for(&node), 0, 0, 0);
        }

        fn write(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            if offset < 0 {
                reply.error(EIO);
                return;
            }
            let pending = match self.pending_for_inode(ino) {
                Ok(Some(pending)) => pending,
                Ok(None) => {
                    reply.error(EROFS);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            if crate::commands::qa_feature_a::enabled() {
                let file = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&pending.staging_path)
                {
                    Ok(file) => file,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                match file.write_at(data, offset as u64) {
                    Ok(written) => {
                        let size = std::fs::metadata(&pending.staging_path)
                            .map(|metadata| metadata.len())
                            .unwrap_or(pending.size);
                        if let Ok(conn) = self.db.lock() {
                            let _ = update_drive_pending_size(&conn, &pending.id, size);
                        }
                        reply.written(written as u32);
                    }
                    Err(_) => reply.error(EIO),
                }
                return;
            }

            match self.write_pending_bytes(&pending, offset as u64, data) {
                Ok(updated) => {
                    let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                    let first = offset as u64 / chunk_size;
                    let last = (offset as u64 + data.len().saturating_sub(1) as u64) / chunk_size;
                    if let Err(error) = self.flush_completed_chunks(&updated, first, last) {
                        log::warn!("Drive bounded upload failed: {}", error);
                        reply.error(EIO);
                        return;
                    }
                    reply.written(data.len() as u32);
                }
                Err(error) => {
                    log::warn!("Drive bounded write failed: {}", error);
                    reply.error(if error.contains("staging limit reached") {
                        ENOSPC
                    } else {
                        EIO
                    });
                }
            }
        }

        fn flush(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            _lock_owner: u64,
            reply: ReplyEmpty,
        ) {
            match self.pending_for_inode(ino) {
                Ok(Some(pending)) => {
                    if crate::commands::qa_feature_a::enabled() {
                        match sync_local_staging_for_pending(&pending) {
                            Ok(()) => reply.ok(),
                            Err(error) => {
                                log::warn!("QA Drive flush failed: {}", error);
                                reply.error(EIO);
                            }
                        }
                        return;
                    }
                    let count = pending
                        .size
                        .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
                    if count > 0 {
                        if let Err(error) = self.flush_completed_chunks(&pending, 0, count - 1) {
                            log::warn!("Drive flush failed: {}", error);
                            reply.error(EIO);
                            return;
                        }
                    }
                    if let Err(error) = sync_local_staging_for_pending(&pending) {
                        log::warn!("Drive staged bytes could not be flushed to disk: {}", error);
                        reply.error(EIO);
                        return;
                    }
                    reply.ok();
                }
                Ok(None) => reply.ok(),
                Err(_) => reply.error(EIO),
            }
        }

        fn fsync(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            _datasync: bool,
            reply: ReplyEmpty,
        ) {
            match self.pending_for_inode(ino) {
                Ok(Some(pending)) => {
                    if crate::commands::qa_feature_a::enabled() {
                        match sync_local_staging_for_pending(&pending) {
                            Ok(()) => reply.ok(),
                            Err(error) => {
                                log::warn!("QA Drive fsync failed: {}", error);
                                reply.error(EIO);
                            }
                        }
                        return;
                    }
                    let count = pending
                        .size
                        .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
                    if count > 0 {
                        if let Err(error) = self.flush_completed_chunks(&pending, 0, count - 1) {
                            log::warn!("Drive fsync failed: {}", error);
                            reply.error(EIO);
                            return;
                        }
                    }
                    if let Err(error) = sync_local_staging_for_pending(&pending) {
                        log::warn!("Drive staged bytes could not be synced to disk: {}", error);
                        reply.error(EIO);
                        return;
                    }
                    reply.ok();
                }
                Ok(None) => reply.ok(),
                Err(_) => reply.error(EIO),
            }
        }

        fn release(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            flags: i32,
            _lock_owner: Option<u64>,
            _flush: bool,
            reply: ReplyEmpty,
        ) {
            if flags & libc::O_ACCMODE == libc::O_RDONLY {
                reply.ok();
                return;
            }
            if !self.writer_closed_is_last(ino) {
                reply.ok();
                return;
            }
            match self.pending_for_inode(ino) {
                Ok(Some(mut pending)) => {
                    if crate::commands::qa_feature_a::enabled() {
                        if pending.base_entry_id.is_some() {
                            match self.pending_has_content_changes(&pending) {
                                Ok(false) => match self.cancel_pending_sync(&pending, false) {
                                    Ok(()) => reply.ok(),
                                    Err(error) => {
                                        log::error!(
                                            "QA Drive could not discard unchanged edit {}: {}",
                                            pending.display_name,
                                            error
                                        );
                                        reply.error(EIO);
                                    }
                                },
                                Ok(true) => {
                                    // Keep the replacement locally visible in QA.
                                    // No Telegram completion is implied.
                                    self.mount_state.set_pending_active(&pending.id, false);
                                    reply.ok();
                                }
                                Err(error) => {
                                    log::error!(
                                        "QA Drive could not validate pending edit {}: {}",
                                        pending.display_name,
                                        error
                                    );
                                    reply.error(EIO);
                                }
                            }
                            return;
                        }

                        if let Ok(metadata) = std::fs::metadata(&pending.staging_path) {
                            pending.size = metadata.len();
                            if let Ok(conn) = self.db.lock() {
                                let _ = update_drive_pending_size(&conn, &pending.id, pending.size);
                            }
                        }
                        self.emit_upload_once(&pending);
                        self.mount_state.set_pending_active(&pending.id, false);
                        reply.ok();
                        return;
                    }

                    match self.pending_has_content_changes(&pending) {
                        Ok(false) => {
                            match self.cancel_pending_sync(&pending, false) {
                                Ok(()) => reply.ok(),
                                Err(error) => {
                                    log::error!(
                                        "TeraRelay Drive could not discard unchanged edit {}: {}",
                                        pending.display_name,
                                        error
                                    );
                                    reply.error(EIO);
                                }
                            }
                            return;
                        }
                        Ok(true) => {}
                        Err(error) => {
                            log::error!(
                                "TeraRelay Drive could not validate pending edit {}: {}",
                                pending.display_name,
                                error
                            );
                            reply.error(EIO);
                            return;
                        }
                    }

                    let result = self.finalize_pending_sync(&pending.id);
                    self.mount_state.set_pending_active(&pending.id, false);
                    match result {
                        Ok(()) => reply.ok(),
                        Err(error) => {
                            log::error!(
                                "TeraRelay Drive could not finalize {}: {}",
                                pending.display_name,
                                error
                            );
                            reply.error(EIO);
                        }
                    }
                }
                Ok(None) => reply.ok(),
                Err(_) => reply.error(EIO),
            }
        }

        fn setattr(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<TimeOrNow>,
            _mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<u64>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<u32>,
            reply: ReplyAttr,
        ) {
            match self.node_for_inode(ino) {
                Ok(Some(Node::Pending(mut pending))) => {
                    if let Some(new_size) = size {
                        match self.resize_pending(&pending, new_size) {
                            Ok(updated) => pending = updated,
                            Err(error) => {
                                log::warn!("Drive resize failed: {}", error);
                                reply.error(EIO);
                                return;
                            }
                        }
                    }
                    reply.attr(&TTL, &attr_for(&Node::Pending(pending)));
                }
                Ok(Some(Node::Remote(file))) => {
                    let Some(new_size) = size else {
                        reply.attr(&TTL, &attr_for(&Node::Remote(file)));
                        return;
                    };
                    let visible_size = file.plaintext_size.unwrap_or(file.total_size);
                    if new_size == visible_size {
                        reply.attr(&TTL, &attr_for(&Node::Remote(file)));
                        return;
                    }
                    let pending = match self.create_replacement_pending(&file) {
                        Ok(pending) => pending,
                        Err(error) => {
                            log::warn!("Drive truncate could not start copy-on-write: {}", error);
                            reply.error(if error.contains("locked") {
                                EACCES
                            } else {
                                EIO
                            });
                            return;
                        }
                    };
                    let pending = match self.resize_pending(&pending, new_size) {
                        Ok(pending) => pending,
                        Err(error) => {
                            let _ = self.cancel_pending_sync(&pending, false);
                            log::warn!("Drive truncate failed: {}", error);
                            reply.error(EIO);
                            return;
                        }
                    };
                    if crate::commands::qa_feature_a::enabled() {
                        self.mount_state.set_pending_active(&pending.id, false);
                        reply.attr(&TTL, &attr_for(&Node::Pending(pending)));
                        return;
                    }
                    let result = self.finalize_pending_sync(&pending.id);
                    self.mount_state.set_pending_active(&pending.id, false);
                    if let Err(error) = result {
                        log::warn!("Drive truncate finalization failed: {}", error);
                        reply.error(EIO);
                        return;
                    }
                    let refreshed = self
                        .db
                        .lock()
                        .map_err(|_| "DB poisoned".to_string())
                        .and_then(|conn| drive_file_by_entry_id(&conn, &file.entry_id));
                    match refreshed {
                        Ok(Some(file)) => reply.attr(&TTL, &attr_for(&Node::Remote(file))),
                        _ => reply.error(EIO),
                    }
                }
                Ok(Some(node @ Node::Directory(_))) | Ok(Some(node @ Node::Root)) => {
                    reply.attr(&TTL, &attr_for(&node));
                }
                Ok(None) => reply.error(ENOENT),
                Err(_) => reply.error(EIO),
            }
        }

        fn mkdir(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            let Some(name) = name.to_str() else {
                reply.error(EIO);
                return;
            };
            let Some(parent_id) = self.directory_id_for_inode(parent).ok().flatten() else {
                reply.error(ENOTDIR);
                return;
            };
            let directory = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                match create_drive_directory(&conn, parent_id.as_deref(), name) {
                    Ok(directory) => directory,
                    Err(error) => {
                        log::warn!("Drive mkdir failed: {}", error);
                        reply.error(if error.contains("already exists") {
                            EEXIST
                        } else {
                            EIO
                        });
                        return;
                    }
                }
            };
            let node = Node::Directory(directory);
            reply.entry(&TTL, &attr_for(&node), 0);
        }

        fn rename(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            newparent: u64,
            newname: &OsStr,
            flags: u32,
            reply: ReplyEmpty,
        ) {
            // Modern coreutils/Nemo use renameat2(RENAME_NOREPLACE)
            // for ordinary moves. Our metadata layer already refuses a target
            // name collision, so NOREPLACE maps exactly to the behavior we
            // provide. Exchange/whiteout semantics are not safe to fake.
            let unsupported = flags & !(libc::RENAME_NOREPLACE as u32);
            if unsupported != 0 {
                reply.error(EINVAL);
                return;
            }
            let Some(new_name) = newname.to_str() else {
                reply.error(EIO);
                return;
            };
            let node = match self.lookup_child(parent, name) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let Some(new_parent_id) = self.directory_id_for_inode(newparent).ok().flatten() else {
                reply.error(ENOTDIR);
                return;
            };
            let result = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                match node {
                    Node::Directory(directory) => rename_drive_directory(
                        &conn,
                        &directory.id,
                        new_parent_id.as_deref(),
                        new_name,
                    ),
                    Node::Remote(file) => {
                        rename_drive_file(&conn, &file.entry_id, new_parent_id.as_deref(), new_name)
                    }
                    Node::Pending(file) => {
                        rename_drive_pending(&conn, &file.id, new_parent_id.as_deref(), new_name)
                    }
                    Node::Root => Err("Cannot rename the Drive root".to_string()),
                }
            };
            match result {
                Ok(()) => reply.ok(),
                Err(error) => {
                    log::warn!("Drive rename failed: {}", error);
                    reply.error(if error.contains("already exists") {
                        EEXIST
                    } else {
                        EIO
                    });
                }
            }
        }

        fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let node = match self.lookup_child(parent, name) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            match node {
                Node::Remote(file) => {
                    let result = self
                        .db
                        .lock()
                        .map_err(|_| "DB poisoned".to_string())
                        .and_then(|conn| trash_drive_file(&conn, &file.entry_id));
                    match result {
                        Ok(()) => reply.ok(),
                        Err(_) => reply.error(EIO),
                    }
                }
                Node::Pending(file) => match self.cancel_pending_sync(&file, true) {
                    Ok(()) => reply.ok(),
                    Err(error) => {
                        log::warn!("Drive pending delete failed: {}", error);
                        reply.error(EIO);
                    }
                },
                Node::Directory(_) | Node::Root => reply.error(EISDIR),
            }
        }

        fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let node = match self.lookup_child(parent, name) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let Node::Directory(directory) = node else {
                reply.error(ENOTDIR);
                return;
            };
            let result = self
                .db
                .lock()
                .map_err(|_| "DB poisoned".to_string())
                .and_then(|conn| trash_drive_directory(&conn, &directory.id));
            match result {
                Ok(()) => reply.ok(),
                Err(error) if error.contains("not empty") => reply.error(ENOTEMPTY),
                Err(_) => reply.error(EIO),
            }
        }

        fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
            let values = virtual_statfs_values();
            reply.statfs(
                values.blocks,
                values.blocks_free,
                values.blocks_available,
                values.files,
                values.files_free,
                values.block_size,
                values.name_length,
                values.fragment_size,
            );
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExistingMountDisposition {
        Ready,
        Healthy,
        Stale,
        InspectionError(i32),
    }

    fn classify_existing_mount(
        is_active: bool,
        metadata_errno: Option<i32>,
    ) -> ExistingMountDisposition {
        if !is_active {
            return ExistingMountDisposition::Ready;
        }
        match metadata_errno {
            None => ExistingMountDisposition::Healthy,
            Some(libc::ENOTCONN) | Some(libc::EIO) => ExistingMountDisposition::Stale,
            Some(errno) => ExistingMountDisposition::InspectionError(errno),
        }
    }

    fn normalize_mount_path_lexically(path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if !normalized.pop() {
                        normalized.push(component.as_os_str());
                    }
                }
                _ => normalized.push(component.as_os_str()),
            }
        }
        normalized
    }

    fn encode_mountinfo_path(path: &Path) -> String {
        normalize_mount_path_lexically(path)
            .to_string_lossy()
            .replace('\\', "\\134")
            .replace(' ', "\\040")
            .replace('\t', "\\011")
            .replace('\n', "\\012")
    }

    fn mountpoint_is_active(path: &Path) -> bool {
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        let encoded = encode_mountinfo_path(path);
        info.lines().any(|line| {
            line.split_whitespace()
                .nth(4)
                .map(|mount| mount == encoded)
                .unwrap_or(false)
        })
    }

    fn prepare_mount_point(path: &Path) -> Result<(), String> {
        let active = mountpoint_is_active(path);
        let metadata = if active {
            Some(std::fs::metadata(path))
        } else {
            None
        };
        let disposition = match metadata.as_ref() {
            None => classify_existing_mount(false, None),
            Some(Ok(_)) => classify_existing_mount(true, None),
            Some(Err(error)) => match error.raw_os_error() {
                Some(errno) => classify_existing_mount(true, Some(errno)),
                None => ExistingMountDisposition::InspectionError(0),
            },
        };

        match disposition {
            ExistingMountDisposition::Ready => {}
            ExistingMountDisposition::Healthy => {
                return Err(format!(
                    "TeraRelay Drive is already mounted at {}",
                    path.display()
                ));
            }
            ExistingMountDisposition::Stale => {
                // A crash or dev hot-reload can leave a dead FUSE mount in the
                // kernel. Only detach a mount proven unreachable by ENOTCONN/EIO;
                // never disturb a healthy mount owned by another TeraRelay process.
                let status = Command::new("fusermount3")
                    .arg("-uz")
                    .arg(path)
                    .status()
                    .map_err(|e| format!("Failed to detach stale TeraRelay Drive mount: {e}"))?;
                if !status.success() {
                    return Err(format!(
                        "Could not detach stale TeraRelay Drive mount at {}",
                        path.display()
                    ));
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while mountpoint_is_active(path) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if mountpoint_is_active(path) {
                    return Err(format!(
                        "Stale TeraRelay Drive mount is still active at {}",
                        path.display()
                    ));
                }
            }
            ExistingMountDisposition::InspectionError(_) => {
                let error = metadata
                    .and_then(Result::err)
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "unknown mount inspection error".to_string());
                return Err(format!(
                    "Could not inspect existing TeraRelay Drive mount {}: {error}",
                    path.display()
                ));
            }
        }

        std::fs::create_dir_all(path)
            .map_err(|e| format!("Failed to create TeraRelay Drive mount point: {e}"))
    }

    fn default_mount_point(app: &AppHandle) -> Result<PathBuf, String> {
        // Explicit override is primarily for isolated QA and packaging tests,
        // but is also useful for advanced Linux deployments. It must win over
        // the QA fallback so we can exercise production-like Nemo visibility
        // without touching the user's real ~/TeraRelay Drive.
        if let Some(custom) = std::env::var_os("TERARELAY_DRIVE_MOUNT_PATH") {
            return Ok(PathBuf::from(custom));
        }
        if crate::commands::qa_feature_a::enabled() {
            return Ok(app
                .path()
                .app_cache_dir()
                .map_err(|e| format!("Failed to resolve QA cache directory: {e}"))?
                .join("TeraRelay Drive"));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            "Could not resolve the home directory for TeraRelay Drive".to_string()
        })?;
        Ok(home.join("TeraRelay Drive"))
    }

    pub fn mount_impl(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
    ) -> Result<DriveMountStatus, String> {
        if state.inner.mounted.load(Ordering::SeqCst) {
            return Ok(state.status());
        }
        if state.inner.mounting.swap(true, Ordering::SeqCst) {
            return Ok(state.status());
        }

        let mount_point = default_mount_point(&app_handle)?;
        prepare_mount_point(&mount_point)?;

        {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::drive_metadata::init_drive_schema(&conn)?;
            crate::commands::drive_metadata::ensure_imported_layout(&conn)?;
        }

        let filesystem = TeraRelayFs {
            db: db_pool.inner().clone(),
            app: app_handle.clone(),
            stream_port: state.stream_port,
            stream_token: state.stream_token.clone(),
            cache_root: state.cache_root.clone(),
            staging_root: state.staging_root.clone(),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(90))
                .build()
                .map_err(|e| format!("Failed to initialize Drive HTTP client: {e}"))?,
            telegram: telegram_state.inner().clone(),
            mount_state: state.inner().clone(),
            emitted_uploads: Mutex::new(std::collections::HashSet::new()),
            writer_counts: Mutex::new(HashMap::new()),
        };

        let inner = state.inner.clone();
        let mount_for_thread = mount_point.clone();
        std::thread::Builder::new()
            .name("terarelay-drive".to_string())
            .spawn(move || {
                // Do not request fuser's AutoUnmount option here. On
                // Debian/Ubuntu-family systems it implicitly requires
                // allow_other, which in turn requires the machine-wide
                // user_allow_other setting in /etc/fuse.conf. TeraRelay must
                // mount as a normal unprivileged user without asking them to
                // weaken global FUSE policy. App shutdown already performs a
                // best-effort explicit unmount, and closing the FUSE connection
                // also tears down the userspace filesystem on process exit.
                let options = vec![
                    MountOption::FSName("TeraRelay".to_string()),
                    MountOption::Subtype("terarelay".to_string()),
                    MountOption::DefaultPermissions,
                ];
                let result = fuser::mount2(filesystem, &mount_for_thread, &options);
                if let Err(error) = result {
                    log::error!("TeraRelay Drive mount exited: {}", error);
                }
                inner.mounted.store(false, Ordering::SeqCst);
                inner.mounting.store(false, Ordering::SeqCst);
            })
            .map_err(|e| format!("Failed to start TeraRelay Drive mount thread: {e}"))?;

        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if mountpoint_is_active(&mount_point) {
                state.inner.mounted.store(true, Ordering::SeqCst);
                state.inner.mounting.store(false, Ordering::SeqCst);
                if let Ok(mut slot) = state.inner.mount_point.lock() {
                    *slot = Some(mount_point.clone());
                }
                return Ok(state.status());
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        state.inner.mounting.store(false, Ordering::SeqCst);
        Err("TeraRelay Drive did not mount within the startup timeout".to_string())
    }

    pub fn recover_pending_impl(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
    ) -> Result<DriveRecoveryResult, String> {
        if crate::commands::qa_feature_a::enabled() {
            return Ok(DriveRecoveryResult {
                recovered: 0,
                remaining: 0,
                failures: 0,
            });
        }

        let closed_ids: Vec<String> = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_snapshot(&conn)?
                .pending
                .into_iter()
                .filter(|pending| pending.closed_at.is_some())
                .map(|pending| pending.id)
                .collect()
        };
        if closed_ids.is_empty() {
            return Ok(DriveRecoveryResult {
                recovered: 0,
                remaining: 0,
                failures: 0,
            });
        }

        let helper = TeraRelayFs {
            db: db_pool.inner().clone(),
            app: app_handle,
            stream_port: state.stream_port,
            stream_token: state.stream_token.clone(),
            cache_root: state.cache_root.clone(),
            staging_root: state.staging_root.clone(),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(90))
                .build()
                .map_err(|e| format!("Failed to initialize Drive recovery client: {e}"))?,
            telegram: telegram_state.inner().clone(),
            mount_state: state.inner().clone(),
            emitted_uploads: Mutex::new(std::collections::HashSet::new()),
            writer_counts: Mutex::new(HashMap::new()),
        };

        let mut recovered = 0u32;
        let mut failures = 0u32;
        for pending_id in closed_ids {
            if state.is_pending_active(&pending_id) {
                continue;
            }
            state.set_pending_active(&pending_id, true);
            match helper.finalize_pending_sync(&pending_id) {
                Ok(()) => recovered = recovered.saturating_add(1),
                Err(error) => {
                    failures = failures.saturating_add(1);
                    log::warn!(
                        "Deferred TeraRelay Drive recovery for {}: {}",
                        pending_id,
                        error
                    );
                }
            }
            state.set_pending_active(&pending_id, false);
        }

        let remaining = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            drive_snapshot(&conn)?
                .pending
                .iter()
                .filter(|pending| pending.closed_at.is_some())
                .count() as u32
        };
        Ok(DriveRecoveryResult {
            recovered,
            remaining,
            failures,
        })
    }

    pub fn unmount_impl(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
        let mount_point = state
            .inner
            .mount_point
            .lock()
            .map_err(|_| "Drive state is unavailable".to_string())?
            .clone();
        let Some(mount_point) = mount_point else {
            state.inner.mounted.store(false, Ordering::SeqCst);
            return Ok(state.status());
        };
        if mountpoint_is_active(&mount_point) {
            let status = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg(&mount_point)
                .status()
                .map_err(|e| format!("Failed to run fusermount3: {e}"))?;
            if !status.success() {
                return Err("fusermount3 could not unmount TeraRelay Drive".to_string());
            }
        }
        state.inner.mounted.store(false, Ordering::SeqCst);
        Ok(state.status())
    }

    fn should_lazy_detach_after_unmount(normal_succeeded: bool, still_active: bool) -> bool {
        !normal_succeeded && still_active
    }

    pub fn unmount_best_effort(state: &DriveMountState) {
        let path = {
            let Ok(slot) = state.inner.mount_point.lock() else {
                return;
            };
            let Some(path) = slot.clone() else {
                return;
            };
            path
        };
        if mountpoint_is_active(&path) {
            let normal_succeeded = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg(&path)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            let still_active = mountpoint_is_active(&path);
            if should_lazy_detach_after_unmount(normal_succeeded, still_active) {
                // File managers can keep the mount busy while the application is
                // closing. Do not leave a dead FUSE endpoint behind: after a
                // normal unmount has actually failed, lazily detach only this
                // TeraRelay-owned mount point so existing handles can drain.
                match std::process::Command::new("fusermount3")
                    .arg("-uz")
                    .arg(&path)
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => log::warn!(
                        "Lazy TeraRelay Drive detach exited with status {} for {}",
                        status,
                        path.display()
                    ),
                    Err(error) => log::warn!(
                        "Could not lazily detach TeraRelay Drive {}: {}",
                        path.display(),
                        error
                    ),
                }
            }
        }
        state.inner.mounted.store(false, Ordering::SeqCst);
    }

    #[cfg(test)]
    mod staging_budget_tests {
        use super::{
            classify_existing_mount, encode_mountinfo_path, ensure_private_staging_directory,
            pending_storage_format, recorded_part_needs_reupload, should_lazy_detach_after_unmount,
            staging_can_accept, virtual_statfs_values, ExistingMountDisposition,
            PendingStorageFormat, STAGING_BUDGET_BYTES,
        };
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn private_staging_corrects_old_permissions_and_rejects_symlinks() {
            let unique = format!(
                "terarelay-drive-private-stage-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

            ensure_private_staging_directory(&root).unwrap();
            let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);

            let link = root.with_extension("link");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            assert!(ensure_private_staging_directory(&link).is_err());
            std::fs::remove_file(&link).unwrap();
            std::fs::remove_dir(&root).unwrap();
        }

        #[test]
        fn bounded_stage_accepts_exact_limit_and_rejects_overflow() {
            assert!(staging_can_accept(0, STAGING_BUDGET_BYTES).is_ok());
            assert!(staging_can_accept(STAGING_BUDGET_BYTES - 4096, 4096).is_ok());
            assert!(staging_can_accept(STAGING_BUDGET_BYTES - 4096, 4097).is_err());
            assert!(staging_can_accept(STAGING_BUDGET_BYTES, 1).is_err());
            assert!(staging_can_accept(u64::MAX, u64::MAX).is_err());
        }

        #[test]
        fn virtual_drive_capacity_is_huge_but_uses_safe_statfs_values() {
            let values = virtual_statfs_values();
            let capacity = values.blocks.checked_mul(values.block_size as u64).unwrap();
            assert!(capacity > STAGING_BUDGET_BYTES.saturating_mul(1_000_000));
            assert_eq!(values.blocks, values.blocks_free);
            assert_eq!(values.blocks, values.blocks_available);
            assert!(capacity <= i64::MAX as u64);
            assert_eq!(values.block_size, 4096);
            assert_eq!(values.fragment_size, 4096);
            assert!(values.files > 1_000_000_000);
            assert_eq!(values.files, values.files_free);
        }

        #[test]
        fn mountinfo_path_encoding_matches_linux_single_backslash_escapes() {
            let path = std::path::Path::new("/home/yash/TeraRelay Drive\tQA");
            assert_eq!(
                encode_mountinfo_path(path),
                "/home/yash/TeraRelay\\040Drive\\011QA"
            );
        }

        #[test]
        fn mountinfo_path_encoding_normalizes_dotdot_components() {
            let path =
                std::path::Path::new("/home/yash/Downloads/TeraRelay/app/../TeraRelay Drive QA");
            assert_eq!(
                encode_mountinfo_path(path),
                "/home/yash/Downloads/TeraRelay/TeraRelay\\040Drive\\040QA"
            );
        }

        #[test]
        fn busy_shutdown_mount_falls_back_to_lazy_detach() {
            assert!(!should_lazy_detach_after_unmount(true, false));
            assert!(!should_lazy_detach_after_unmount(true, true));
            assert!(!should_lazy_detach_after_unmount(false, false));
            assert!(should_lazy_detach_after_unmount(false, true));
        }

        #[test]
        fn stale_mount_errors_detach_but_healthy_mounts_are_never_duplicated() {
            assert_eq!(
                classify_existing_mount(false, None),
                ExistingMountDisposition::Ready
            );
            assert_eq!(
                classify_existing_mount(true, None),
                ExistingMountDisposition::Healthy
            );
            assert_eq!(
                classify_existing_mount(true, Some(libc::ENOTCONN)),
                ExistingMountDisposition::Stale
            );
            assert_eq!(
                classify_existing_mount(true, Some(libc::EIO)),
                ExistingMountDisposition::Stale
            );
            assert_eq!(
                classify_existing_mount(true, Some(libc::EACCES)),
                ExistingMountDisposition::InspectionError(libc::EACCES)
            );
        }

        #[test]
        fn pending_format_explicitly_selects_legacy_or_storage_pool_recovery() {
            assert_eq!(
                pending_storage_format(1).unwrap(),
                PendingStorageFormat::LegacyV1
            );
            assert_eq!(
                pending_storage_format(2).unwrap(),
                PendingStorageFormat::PoolV2
            );
            assert!(pending_storage_format(0).is_err());
            assert!(pending_storage_format(3).is_err());
        }

        #[test]
        fn recorded_remote_part_is_reuploaded_for_dirty_bytes_or_changed_layout() {
            let chunk = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            assert!(!recorded_part_needs_reupload(0, 15, 0, 15, &[],));
            assert!(recorded_part_needs_reupload(
                0,
                15,
                0,
                15,
                &[(5 * chunk + 10, 5 * chunk + 20)],
            ));
            assert!(recorded_part_needs_reupload(0, 15, 0, 5, &[],));
            assert!(!recorded_part_needs_reupload(
                0,
                15,
                0,
                15,
                &[(15 * chunk, 15 * chunk + 1)],
            ));
        }
    }
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn cmd_drive_mount(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveMountStatus, String> {
    linux::mount_impl(app_handle, state, db_pool, telegram_state)
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn cmd_drive_recover_pending(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveRecoveryResult, String> {
    linux::recover_pending_impl(app_handle, state, db_pool, telegram_state)
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    linux::unmount_impl(state)
}

#[cfg(target_os = "linux")]
pub use linux::unmount_best_effort;

#[cfg(not(target_os = "linux"))]
pub fn unmount_best_effort(_state: &DriveMountState) {}
