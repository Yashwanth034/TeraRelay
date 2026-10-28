use crate::commands::drive_metadata::{
    clear_drive_pending_ranges_for_span, clear_drive_pending_ranges_for_span_in_transaction,
    create_drive_directory, create_drive_pending_file, create_drive_pending_replacement,
    delete_drive_pending, drive_file_by_entry_id, drive_object_block_location_preinitialized,
    drive_pending_by_id, drive_pending_chunks, drive_pending_parts, drive_pending_ranges,
    drive_snapshot, mark_drive_pending_closed, record_drive_pending_range,
    record_drive_pending_range_in_transaction, rename_drive_directory, rename_drive_file,
    rename_drive_pending, take_drive_pending_chunks_from,
    take_drive_pending_parts_starting_at_chunk, trash_drive_directory, trash_drive_file,
    update_drive_pending_size, DriveDirectoryRecord, DriveFileRecord, DrivePendingRecord,
};
use crate::db::DbConnection;
#[cfg(target_os = "linux")]
#[path = "drive_read_lifetime.rs"]
mod read_lifetime;
#[cfg(target_os = "linux")]
#[path = "drive_recovery_scheduler.rs"]
mod recovery_scheduler;
#[cfg(target_os = "linux")]
#[path = "drive_safe_mode.rs"]
pub(crate) mod safe_mode;
#[cfg(target_os = "linux")]
#[path = "drive_transfer_center.rs"]
pub(crate) mod transfer_center;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::drive_fs_core::DriveFsCore;
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
    pub native_recovery: bool,
    pub automatic_upload_recovery: bool,
    pub mount_point: Option<String>,
    pub read_streaming: bool,
    pub write_staging: bool,
    pub provider_kind: String,
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
    #[cfg(target_os = "linux")]
    safe_mount_read_only: AtomicBool,
    native_recovery_running: AtomicBool,
    #[cfg(target_os = "linux")]
    automatic_upload_recovery_running: AtomicBool,
    #[cfg(target_os = "linux")]
    transfer: Mutex<transfer_center::QueueRuntime>,
    #[cfg(target_os = "linux")]
    session: Mutex<LinuxMountSession>,
    active_pending: Mutex<PendingActivity>,
    #[cfg(target_os = "linux")]
    native_restores: crate::drive_restore::RestoreRequests,
    #[cfg(target_os = "linux")]
    mount_identity: u128,
}

/// Both a writable FUSE open and an automatic/manual replay must take the
/// same lock, so a closed stage cannot begin uploading while a writer opens it.
#[derive(Default)]
struct PendingActivity {
    active: std::collections::HashSet<String>,
    recovering: std::collections::HashSet<String>,
}

pub(crate) struct PendingRecoveryClaim {
    inner: Arc<DriveMountInner>,
    id: String,
}

impl Drop for PendingRecoveryClaim {
    fn drop(&mut self) {
        if let Ok(mut activity) = self.inner.active_pending.lock() {
            activity.recovering.remove(&self.id);
            activity.active.remove(&self.id);
        }
    }
}

// A FUSE write offset is signed. Reject negative writes and writes that
// would grow a file past the representable POSIX off_t range before touching
// either the bounded Drive stage or the native staging file.
#[cfg(target_os = "linux")]
fn checked_fuse_write_end(offset: i64, length: usize) -> Result<u64, i32> {
    let start = u64::try_from(offset).map_err(|_| libc::EINVAL)?;
    let length = u64::try_from(length).map_err(|_| libc::EFBIG)?;
    let end = start.checked_add(length).ok_or(libc::EFBIG)?;
    if end > i64::MAX as u64 {
        return Err(libc::EFBIG);
    }
    Ok(end)
}

// FUSE create returns a distinct handle for an O_RDONLY-created pending file.
// It still owns the initial writer and must finalize on release.
#[cfg(target_os = "linux")]
const READ_ONLY_CREATED_HANDLE: u64 = 1;

#[cfg(target_os = "linux")]
fn created_fuse_handle(flags: i32) -> u64 {
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        READ_ONLY_CREATED_HANDLE
    } else {
        0
    }
}

#[cfg(target_os = "linux")]
fn release_closes_pending_writer(flags: i32, fh: u64) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY || fh == READ_ONLY_CREATED_HANDLE
}

// A writable FUSE open with O_TRUNC must clear even an existing pending overlay.
#[cfg(target_os = "linux")]
fn native_open_requests_truncate(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY && flags & libc::O_TRUNC != 0
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod fuse_write_bounds_tests {
    use super::{
        checked_fuse_write_end, created_fuse_handle, native_open_requests_truncate,
        release_closes_pending_writer, READ_ONLY_CREATED_HANDLE,
    };

    #[test]
    fn readonly_created_file_closes_only_its_own_pending_writer() {
        assert_eq!(created_fuse_handle(libc::O_RDONLY | libc::O_CREAT), 1);
        assert_eq!(created_fuse_handle(libc::O_WRONLY | libc::O_CREAT), 0);
        assert_eq!(created_fuse_handle(libc::O_RDWR | libc::O_CREAT), 0);
        assert!(release_closes_pending_writer(
            libc::O_RDONLY,
            READ_ONLY_CREATED_HANDLE
        ));
        assert!(!release_closes_pending_writer(libc::O_RDONLY, 0));
        assert!(release_closes_pending_writer(libc::O_WRONLY, 0));
        assert!(release_closes_pending_writer(libc::O_RDWR, 0));

        let source = include_str!("drive_mount.rs");
        let create = source.rsplit_once("fn create(\n").unwrap().1;
        assert!(create.contains("reply.created(&TTL, &attr, 0, created_fuse_handle(flags), 0)"));
        let release = source.rsplit_once("fn release(\n").unwrap().1;
        let guard = release
            .find("release_closes_pending_writer(flags, fh)")
            .unwrap();
        let last_writer = release.find("self.writer_closed_is_last(ino)").unwrap();
        assert!(guard < last_writer);
    }

    #[test]
    fn writable_truncate_open_must_apply_to_pending_stages() {
        assert!(!native_open_requests_truncate(libc::O_RDONLY));
        assert!(!native_open_requests_truncate(
            libc::O_RDONLY | libc::O_TRUNC
        ));
        assert!(!native_open_requests_truncate(libc::O_WRONLY));
        assert!(!native_open_requests_truncate(
            libc::O_RDWR | libc::O_APPEND
        ));
        assert!(native_open_requests_truncate(
            libc::O_WRONLY | libc::O_TRUNC
        ));
        assert!(native_open_requests_truncate(libc::O_RDWR | libc::O_TRUNC));

        // The in-flight pending record must be resized before its writer is
        // registered; otherwise programs replacing a staged file may retain
        // old trailing bytes or falsely succeed after a failed truncate.
        let source = include_str!("drive_mount.rs");
        let open = source
            .rsplit_once("fn open(&mut self,")
            .expect("Linux FUSE open must exist")
            .1
            .split_once("fn read(\n")
            .expect("Linux FUSE read must follow open")
            .0;
        let pending = open
            .split_once("Ok(Some(Node::Pending(pending))) => {")
            .expect("pending open branch must exist")
            .1
            .split_once("Ok(None) =>")
            .expect("pending open must handle missing nodes")
            .0;
        let key_check = pending.find("self.ensure_write_key_ready()").unwrap();
        let truncation = pending.find("self.resize_pending(&pending, 0)").unwrap();
        let active = pending
            .find("self.mount_state.try_open_pending_writer")
            .unwrap();
        let writer = pending.find("self.writer_opened(ino)").unwrap();
        assert!(key_check < active && active < truncation && truncation < writer);
        assert!(pending.contains("native_open_requests_truncate(flags)"));
    }

    #[test]
    fn signed_fuse_offsets_must_not_overflow_file_size() {
        assert_eq!(checked_fuse_write_end(0, 0), Ok(0));
        assert_eq!(checked_fuse_write_end(0, 4096), Ok(4096));
        assert_eq!(checked_fuse_write_end(i64::MAX - 1, 1), Ok(i64::MAX as u64));
        assert_eq!(checked_fuse_write_end(i64::MAX, 0), Ok(i64::MAX as u64));
        assert_eq!(checked_fuse_write_end(-1, 0), Err(libc::EINVAL));
        assert_eq!(checked_fuse_write_end(-1, 1024), Err(libc::EINVAL));
        assert_eq!(checked_fuse_write_end(i64::MAX, 1), Err(libc::EFBIG));
        assert_eq!(checked_fuse_write_end(i64::MAX - 1, 2), Err(libc::EFBIG));
    }

    #[test]
    fn native_fuse_write_checks_extent_before_touching_staging() {
        let source = include_str!("drive_mount.rs");
        let handler = source
            .rsplit_once("fn write(\n")
            .unwrap()
            .1
            .split_once("fn flush(\n")
            .unwrap()
            .0;
        assert!(handler.contains("checked_fuse_write_end(offset, data.len())"));
    }
}

// FUSE directory offsets are opaque signed cookies. A malformed negative
// cookie must not restart the directory at entry zero, and on 32-bit hosts
// a 64-bit cookie must not wrap into a smaller index.
#[cfg(target_os = "linux")]
fn checked_fuse_directory_offset(offset: i64) -> Result<usize, i32> {
    usize::try_from(offset).map_err(|_| {
        if offset < 0 {
            libc::EINVAL
        } else {
            libc::EOVERFLOW
        }
    })
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod fuse_directory_cookie_tests {
    #[test]
    fn directory_cookies_reject_negative_and_unrepresentable_offsets() {
        assert_eq!(super::checked_fuse_directory_offset(0), Ok(0));
        assert_eq!(super::checked_fuse_directory_offset(2), Ok(2));
        assert_eq!(super::checked_fuse_directory_offset(-1), Err(libc::EINVAL));
        assert_eq!(
            super::checked_fuse_directory_offset(i64::MIN),
            Err(libc::EINVAL)
        );
        #[cfg(target_pointer_width = "32")]
        assert_eq!(
            super::checked_fuse_directory_offset(i64::from(u32::MAX) + 1),
            Err(libc::EOVERFLOW)
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            super::checked_fuse_directory_offset(i64::MAX),
            Ok(i64::MAX as usize)
        );
    }

    #[test]
    fn readdir_and_both_readdirplus_paths_reject_invalid_cookies() {
        let source = include_str!("drive_mount.rs");
        let fast_plus = source
            .rsplit_once("fn reply_directory_plus_listing(")
            .unwrap()
            .1
            .split_once("fn lookup_child(")
            .unwrap()
            .0;
        let plain = source
            .rsplit_once("fn readdir(\n")
            .unwrap()
            .1
            .split_once("fn readdirplus(\n")
            .unwrap()
            .0;
        let fallback_plus = source
            .rsplit_once("fn readdirplus(\n")
            .unwrap()
            .1
            .split_once("fn open(")
            .unwrap()
            .0;
        for section in [plain, fast_plus, fallback_plus] {
            assert!(
                section.contains("checked_fuse_directory_offset(offset)"),
                "every FUSE directory pagination path must validate its signed cookie"
            );
            assert!(
                !section.contains("offset.max(0) as usize"),
                "negative/overflowing directory cookies cannot silently restart enumeration"
            );
        }
    }

    #[test]
    fn fuse_folder_stall_tracing_distinguishes_remote_reads_from_metadata_waits() {
        // FUSE callbacks execute serially: a long remote read or SQLite lock
        // wait can block a later folder lookup even when lookup SQL is fast.
        // These labels must remain path-free for private Drive contents.
        let source = include_str!("drive_mount.rs");
        let block_read = source
            .rsplit_once("fn read_remote_range_with_locations(")
            .unwrap()
            .1
            .split_once("fn emit_upload_once(")
            .unwrap()
            .0;
        assert!(block_read.contains("SlowFusePhase::new(\"remote-block-fetch\")"));
        let attr = source
            .rsplit_once("fn getattr(&mut self,")
            .unwrap()
            .1
            .split_once("fn readdir(")
            .unwrap()
            .0;
        assert!(attr.contains("report_slow_drive_db_wait(\"getattr-indexed\", started)"));
        assert!(attr.contains("SlowFusePhase::new(\"getattr-indexed-sql\")"));
    }

    #[test]
    fn readdirplus_separates_inode_index_work_from_sql_latency() {
        let source = include_str!("drive_mount.rs");
        let fast = source
            .rsplit_once("fn reply_directory_plus_listing(")
            .unwrap()
            .1
            .split_once("fn lookup_child(&self, parent: u64, name: &OsStr)")
            .unwrap()
            .0;
        let fallback = source
            .rsplit_once("fn readdirplus(\n")
            .unwrap()
            .1
            .split_once("fn open(")
            .unwrap()
            .0;
        assert!(
            fast.contains("SlowFusePhase::new(\"readdirplus-inode-index\")"),
            "indexed readdirplus must distinguish inode-index contention from SQL latency"
        );
        assert!(
            fallback.contains("SlowFusePhase::new(\"readdirplus-fallback-inode-index\")"),
            "fallback readdirplus must also distinguish inode-index contention"
        );
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod fuse_setattr_directory_link_tests {
    use super::desktop::{
        checked_native_setattr_size, directory_link_count, shared_inode_mutation_errno,
    };
    use fuser::FileType;

    #[test]
    fn shared_inode_mutation_errors_are_permission_denied_not_spurious_io_failures() {
        assert_eq!(
            shared_inode_mutation_errno(
                "Drive hardlink mutation requires shared-inode POSIX support"
            ),
            Some(libc::EPERM)
        );
        assert_eq!(
            shared_inode_mutation_errno(
                "Drive hardlink editing requires shared-inode copy-on-write support"
            ),
            Some(libc::EPERM)
        );
        assert_eq!(shared_inode_mutation_errno("DB poisoned"), None);
        assert_eq!(
            shared_inode_mutation_errno("Drive source does not exist"),
            None
        );
    }

    #[test]
    fn native_setattr_must_reject_unsupported_size_before_any_permission_changes() {
        assert_eq!(
            checked_native_setattr_size(FileType::RegularFile, 0),
            Ok(())
        );
        assert_eq!(
            checked_native_setattr_size(FileType::RegularFile, i64::MAX as u64),
            Ok(())
        );
        assert_eq!(
            checked_native_setattr_size(FileType::RegularFile, (i64::MAX as u64) + 1),
            Err(libc::EFBIG)
        );
        assert_eq!(
            checked_native_setattr_size(FileType::Directory, 0),
            Err(libc::EISDIR)
        );
        assert_eq!(
            checked_native_setattr_size(FileType::Symlink, 0),
            Err(libc::EINVAL)
        );

        let source = include_str!("drive_mount.rs");
        let setattr = source
            .rsplit_once("fn setattr(\n")
            .expect("native FUSE setattr must exist")
            .1
            .split_once("fn mkdir(\n")
            .expect("native FUSE mkdir follows setattr")
            .0;
        let size_guard = setattr
            .find("checked_native_setattr_size(attr_for(&node).kind, new_size)")
            .expect("setattr must use native size guard");
        let permission_write = setattr
            .find("self.permission_change(&node, mode & 0o777)")
            .expect("setattr must still support chmod");
        assert!(
            size_guard < permission_write,
            "rejected size must never commit an unrelated permission change"
        );
    }

    #[test]
    fn native_lookup_must_release_database_before_inode_index_updates() {
        let source = include_str!("drive_mount.rs");
        let lookup = source
            .rsplit_once("fn lookup_child(&self, parent: u64, name: &OsStr)")
            .expect("native FUSE lookup_child must exist")
            .1
            .split_once("fn lookup_child_identity(")
            .expect("lookup_child_identity must follow lookup_child")
            .0;
        let sql_scope = lookup
            .find("let node = {")
            .expect("lookup SQL must run in its own scoped database lock");
        let index_phase = lookup
            .find("SlowFusePhase::new(\"lookup-child-inode-index\")")
            .expect("inode index update must have a separate path-free phase");
        let index_update = lookup
            .find("self.remember_node(&node)")
            .expect("lookup must remember discovered nodes");
        assert!(sql_scope < index_phase && index_phase < index_update);
        let sql_scope_end = lookup[sql_scope..]
            .find("\n            };")
            .expect("scoped SQL connection must end before indexing")
            + sql_scope;
        assert!(
            sql_scope_end < index_phase,
            "SQLite mutex remained held during inode-index update"
        );
    }

    #[test]
    fn hardlink_creation_must_fail_closed_without_atomic_shared_inode_mutations() {
        // The current manifest has no atomic alias-wide write/unlink semantics.
        // Never let the kernel believe hardlink creation succeeded until these
        // operations and cross-device manifest compatibility are implemented.
        let source = include_str!("drive_mount.rs");
        let link = source
            .rsplit_once("fn link(\n")
            .expect("native FUSE must explicitly handle hardlink requests")
            .1
            .split_once("fn symlink(\n")
            .expect("native symlink handler follows link")
            .0;
        assert!(
            link.contains("reply.error(libc::EPERM)"),
            "hardlink must explicitly refuse unsupported shared inode alias creation"
        );
        assert!(
            !link.contains("self.db.lock()")
                && !link.contains("self.lookup_child(")
                && !link.contains("self.remember_node("),
            "refused hardlink must never mutate or even resolve a user's files"
        );
    }

    #[test]
    fn directory_link_counts_follow_only_live_immediate_subdirectories() {
        let db = sqlite::open(":memory:").unwrap();
        db.execute(
            "CREATE TABLE drive_directories (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                trashed_at INTEGER
             );
             INSERT INTO drive_directories VALUES
                ('alpha',NULL,NULL),
                ('beta',NULL,NULL),
                ('trashed-root',NULL,123),
                ('alpha-child','alpha',NULL),
                ('alpha-dead','alpha',123),
                ('grandchild','alpha-child',NULL);",
        )
        .unwrap();

        assert_eq!(directory_link_count(&db, None).unwrap(), 4);
        assert_eq!(directory_link_count(&db, Some("alpha")).unwrap(), 3);
        assert_eq!(directory_link_count(&db, Some("alpha-child")).unwrap(), 3);
        assert_eq!(directory_link_count(&db, Some("beta")).unwrap(), 2);
        db.execute(
            "UPDATE drive_directories SET trashed_at=321
             WHERE id='alpha-child'",
        )
        .unwrap();
        assert_eq!(directory_link_count(&db, Some("alpha")).unwrap(), 2);
        assert_eq!(directory_link_count(&db, Some("alpha-child")).unwrap(), 3);
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LinuxMountSession {
    generation: u64,
    worker_running: bool,
    cancelled: bool,
    cancellation_epoch: u64,
    requested: bool,
    unmounter: Option<fuser::SessionUnmounter>,
    notifier: Option<fuser::Notifier>,
}

#[cfg(target_os = "linux")]
impl DriveMountInner {
    fn request_mount(&self) -> Result<u64, String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        session.requested = true;
        Ok(session.cancellation_epoch)
    }

    fn requested_epoch(&self) -> Result<Option<u64>, String> {
        let session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        Ok(session.requested.then_some(session.cancellation_epoch))
    }

    #[cfg(test)]
    fn cancellation_epoch(&self) -> Result<u64, String> {
        Ok(self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?
            .cancellation_epoch)
    }

    fn cancel_session(&self) {
        if let Ok(mut session) = self.session.lock() {
            session.cancellation_epoch = session.cancellation_epoch.wrapping_add(1);
            session.cancelled = true;
            session.requested = false;
            self.mounted.store(false, Ordering::SeqCst);
        }
    }

    fn take_unmounter(&self) -> Result<Option<fuser::SessionUnmounter>, String> {
        Ok(self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?
            .unmounter
            .take())
    }

    fn register_unmounter(
        &self,
        generation: u64,
        unmounter: fuser::SessionUnmounter,
        notifier: fuser::Notifier,
    ) -> Result<(), String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        if session.generation != generation {
            return Err("Drive mount session was superseded".to_string());
        }
        session.unmounter = Some(unmounter);
        session.notifier = Some(notifier);
        Ok(())
    }

    fn begin_session(&self, expected_epoch: u64) -> Result<u64, String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        if session.cancellation_epoch != expected_epoch {
            return Err("Drive mount was cancelled during startup".to_string());
        }
        session.generation = session.generation.wrapping_add(1);
        session.cancelled = false;
        session.worker_running = true;
        self.mounted.store(false, Ordering::SeqCst);
        Ok(session.generation)
    }

    fn session_is_starting(&self) -> Result<bool, String> {
        let session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        Ok(session.worker_running
            && session.unmounter.is_none()
            && !self.mounted.load(Ordering::SeqCst))
    }

    fn finish_session(&self, generation: u64) {
        // A lazily detached FUSE worker can finish after a replacement mounts.
        // Keep ownership checks and state updates under the same lock so an
        // old worker cannot disconnect the new session.
        if let Ok(mut session) = self.session.lock() {
            if session.generation == generation {
                session.worker_running = false;
                session.notifier = None;
                self.mounted.store(false, Ordering::SeqCst);
            }
        }
    }

    fn publish_session(&self, generation: u64, mount_point: &Path) -> Result<bool, String> {
        let session = self
            .session
            .lock()
            .map_err(|_| "Drive session poisoned".to_string())?;
        if session.generation != generation || !session.worker_running || session.cancelled {
            return Ok(false);
        }
        *self
            .mount_point
            .lock()
            .map_err(|_| "Drive mount path poisoned".to_string())? =
            Some(mount_point.to_path_buf());
        self.mounted.store(true, Ordering::SeqCst);
        Ok(true)
    }
}

#[derive(Clone)]
pub struct DriveMountState {
    inner: Arc<DriveMountInner>,
    stream_port: u16,
    stream_token: String,
    cache_root: PathBuf,
    staging_root: PathBuf,
}

/// Prevents mount startup while an all-or-none local metadata restore holds
/// the database transaction. Uses the SAME atomic flag as MountAttemptGuard.
#[cfg(target_os = "linux")]
pub(crate) struct BatchRestoreUnmountGuard(Arc<DriveMountInner>);

#[cfg(target_os = "linux")]
impl Drop for BatchRestoreUnmountGuard {
    fn drop(&mut self) {
        self.0.mounting.store(false, Ordering::SeqCst);
    }
}

/// Native health checks do not depend on a running WebView or React timer.
#[cfg(target_os = "linux")]
pub struct NativeDriveRecovery {
    stopped: Arc<AtomicBool>,
    thread: std::thread::Thread,
    upload_thread: std::thread::Thread,
    inner: Arc<DriveMountInner>,
}

#[cfg(target_os = "linux")]
struct NativeRecoveryRunning(Arc<DriveMountInner>);

#[cfg(target_os = "linux")]
struct NativeUploadRecoveryRunning(Arc<DriveMountInner>);

#[cfg(target_os = "linux")]
impl Drop for NativeUploadRecoveryRunning {
    fn drop(&mut self) {
        self.0
            .automatic_upload_recovery_running
            .store(false, Ordering::SeqCst);
    }
}

#[cfg(target_os = "linux")]
impl Drop for NativeRecoveryRunning {
    fn drop(&mut self) {
        self.0
            .native_recovery_running
            .store(false, Ordering::SeqCst);
    }
}

// Query only the small durable upload queue, never walk the full Drive
// namespace on the recurring native health tick. Keep the DB lock bounded.
#[cfg(target_os = "linux")]
fn closed_pending_ids(conn: &sqlite::Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("SELECT id FROM drive_pending_files WHERE closed_at IS NOT NULL ORDER BY id")
        .map_err(|error: sqlite::Error| error.to_string())?;
    let mut ids = Vec::new();
    while statement
        .next()
        .map_err(|error: sqlite::Error| error.to_string())?
        == sqlite::State::Row
    {
        ids.push(
            statement
                .read::<String, _>(0)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(ids)
}

#[cfg(target_os = "linux")]
fn closed_pending_count(conn: &sqlite::Connection) -> Result<u32, String> {
    let mut statement = conn
        .prepare("SELECT COUNT(*) FROM drive_pending_files WHERE closed_at IS NOT NULL")
        .map_err(|error: sqlite::Error| error.to_string())?;
    if statement
        .next()
        .map_err(|error: sqlite::Error| error.to_string())?
        != sqlite::State::Row
    {
        return Err("Drive pending count unavailable".to_string());
    }
    let count = statement
        .read::<i64, _>(0)
        .map_err(|error| error.to_string())?;
    u32::try_from(count).map_err(|_| "Drive pending count overflow".to_string())
}

#[cfg(target_os = "linux")]
impl NativeDriveRecovery {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.inner
            .automatic_upload_recovery_running
            .store(false, Ordering::SeqCst);
        self.thread.unpark();
        self.upload_thread.unpark();
    }
}

#[cfg(target_os = "linux")]
impl Drop for NativeDriveRecovery {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
pub fn start_native_recovery(app: &AppHandle) -> Result<NativeDriveRecovery, String> {
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stop = stopped.clone();
    let inner = app.state::<DriveMountState>().inner.clone();
    inner.native_recovery_running.store(true, Ordering::SeqCst);
    let worker_inner = inner.clone();
    let upload_app = app.clone();
    let app = app.clone();
    let worker = std::thread::Builder::new()
        .name("terarelay-drive-health".into())
        .spawn(move || {
            let _running = NativeRecoveryRunning(worker_inner);
            let mut last_error: Option<String> = None;
            while !worker_stop.load(Ordering::SeqCst) {
                std::thread::park_timeout(std::time::Duration::from_secs(5));
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                let attempt = (|| -> Result<(), String> {
                    let state = app.state::<DriveMountState>();
                    let Some(epoch) = state.inner.requested_epoch()? else {
                        return Ok(());
                    };
                    if state.status().mounted {
                        return Ok(());
                    }
                    desktop::recover_requested_mount(
                        app.clone(),
                        state,
                        app.state::<DbConnection>(),
                        app.state::<crate::commands::TelegramState>(),
                        epoch,
                    )?;
                    Ok(())
                })();
                match attempt {
                    Ok(()) => last_error = None,
                    Err(error) => {
                        if last_error.as_ref() != Some(&error) {
                            log::warn!("Native Drive recovery deferred: {error}");
                            last_error = Some(error);
                        }
                    }
                }
            }
        })
        .map_err(|error| {
            inner.native_recovery_running.store(false, Ordering::SeqCst);
            format!("Could not start native Drive recovery: {error}")
        })?;
    // Upload retry has its own worker so slow Telegram operations cannot
    // stall native mount self-healing. Closed stages remain durable in SQLite.
    let upload_stop = stopped.clone();
    let upload_inner = inner.clone();
    inner
        .automatic_upload_recovery_running
        .store(true, Ordering::SeqCst);
    let upload_worker = std::thread::Builder::new()
        .name("terarelay-drive-deferred-writes".into())
        .spawn(move || {
            let _running = NativeUploadRecoveryRunning(upload_inner);
            let mut last_error: Option<String> = None;
            while !upload_stop.load(Ordering::SeqCst) {
                std::thread::park_timeout(recovery_scheduler::SCAN_INTERVAL);
                if upload_stop.load(Ordering::SeqCst) {
                    break;
                }
                let attempt = (|| -> Result<(), String> {
                    let state = upload_app.state::<DriveMountState>();
                    let Some(epoch) = state.inner.requested_epoch()? else {
                        return Ok(());
                    };
                    if !state.status().mounted {
                        return Ok(());
                    }
                    if crate::commands::qa_feature_a::enabled() || state.safe_mount_read_only() {
                        return Ok(());
                    }
                    let pending = {
                        let db = upload_app.state::<DbConnection>();
                        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
                        closed_pending_ids(&conn)?
                    };
                    let selected = {
                        let mut queue = state
                            .inner
                            .transfer
                            .lock()
                            .map_err(|_| "Drive transfer state unavailable".to_string())?;
                        queue.schedule(&pending)
                    };
                    if selected.is_empty() {
                        return Ok(());
                    }
                    let outcome = desktop::recover_pending_selected_impl(
                        upload_app.clone(),
                        state,
                        upload_app.state::<DbConnection>(),
                        upload_app.state::<crate::commands::TelegramState>(),
                        Some(&selected),
                        Some(epoch),
                    );
                    let result = outcome.as_ref().map(|(_, attempted)| attempted.as_slice());
                    {
                        let runtime_state = upload_app.state::<DriveMountState>();
                        let mut queue = runtime_state
                            .inner
                            .transfer
                            .lock()
                            .map_err(|_| "Drive transfer state unavailable".to_string())?;
                        queue.finish(&selected, result.unwrap_or(&[]), outcome.is_err());
                    }
                    outcome.map(|_| ())
                })();
                match attempt {
                    Ok(()) => last_error = None,
                    Err(error) => {
                        if last_error.as_ref() != Some(&error) {
                            log::warn!("Automatic Drive write recovery deferred: {error}");
                            last_error = Some(error);
                        }
                    }
                }
            }
        })
        .map_err(|error| {
            stopped.store(true, Ordering::SeqCst);
            worker.thread().unpark();
            inner
                .automatic_upload_recovery_running
                .store(false, Ordering::SeqCst);
            format!("Could not start native Drive write recovery: {error}")
        })?;
    Ok(NativeDriveRecovery {
        stopped,
        thread: worker.thread().clone(),
        upload_thread: upload_worker.thread().clone(),
        inner,
    })
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

#[cfg(all(test, target_os = "linux"))]
mod mount_health_tests {
    use super::*;

    fn fixture() -> (DriveMountState, PathBuf) {
        let root = std::env::temp_dir().join(format!("tr-session-{:016x}", rand::random::<u64>()));
        (
            DriveMountState::new(0, "fixture".into(), root.clone()).unwrap(),
            root,
        )
    }

    #[test]
    fn closed_pending_queue_scans_only_durable_closed_uploads() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE drive_pending_files (id TEXT PRIMARY KEY, closed_at INTEGER);
             INSERT INTO drive_pending_files VALUES ('closed-b', 123);
             INSERT INTO drive_pending_files VALUES ('open', NULL);
             INSERT INTO drive_pending_files VALUES ('closed-a', 456);",
        )
        .unwrap();
        assert_eq!(
            closed_pending_ids(&conn).unwrap(),
            vec!["closed-a", "closed-b"]
        );
        assert_eq!(closed_pending_count(&conn).unwrap(), 2);
        conn.execute("DELETE FROM drive_pending_files WHERE id='closed-a'")
            .unwrap();
        assert_eq!(closed_pending_ids(&conn).unwrap(), vec!["closed-b"]);
        assert_eq!(closed_pending_count(&conn).unwrap(), 1);
    }

    #[test]
    fn automatic_replay_and_native_writers_claim_one_stage_exclusively() {
        let (state, root) = fixture();
        assert_eq!(state.try_open_pending_writer("pending"), Some(false));
        assert_eq!(state.try_open_pending_writer("pending"), Some(true));
        assert!(state.claim_pending_recovery("pending").is_none());

        state.set_pending_active("pending", false);
        let claim = state
            .claim_pending_recovery("pending")
            .expect("closed stage");
        assert!(state.is_pending_active("pending"));
        assert_eq!(state.try_open_pending_writer("pending"), None);
        assert!(state.claim_pending_recovery("pending").is_none());
        // Completion from another codepath cannot remove a live replay claim.
        state.set_pending_active("pending", false);
        assert!(state.is_pending_active("pending"));
        drop(claim);
        assert!(!state.is_pending_active("pending"));
        assert_eq!(state.try_open_pending_writer("pending"), Some(false));
        state.set_pending_active("pending", false);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rescue_worker_claim_outlives_its_request_and_blocks_writers_until_drop() {
        let (state, root) = fixture();
        let claim = state
            .claim_pending_recovery("rescued-pending")
            .expect("exclusive pending claim");
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (finish_sender, finish_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Mirroring the rescue IPC: the blocking worker owns the claim.
            let _claim = claim;
            ready_sender.send(()).unwrap();
            finish_receiver.recv().unwrap();
        });
        ready_receiver.recv().unwrap();
        assert!(state.is_pending_active("rescued-pending"));
        assert_eq!(state.try_open_pending_writer("rescued-pending"), None);
        assert!(state.claim_pending_recovery("rescued-pending").is_none());
        finish_sender.send(()).unwrap();
        worker.join().unwrap();
        assert!(!state.is_pending_active("rescued-pending"));
        assert_eq!(
            state.try_open_pending_writer("rescued-pending"),
            Some(false)
        );
        state.set_pending_active("rescued-pending", false);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_restore_gate_excludes_mount_start_and_releases_on_drop() {
        let (state, root) = fixture();
        let guard = state.acquire_unmounted_batch_restore().unwrap();
        assert!(state.inner.mounting.load(Ordering::SeqCst));
        assert!(state.acquire_unmounted_batch_restore().is_err());
        drop(guard);
        assert!(!state.inner.mounting.load(Ordering::SeqCst));
        let generation = state.inner.begin_session(0).unwrap();
        assert!(state.acquire_unmounted_batch_restore().is_err());
        assert!(!state.inner.mounting.load(Ordering::SeqCst));
        state.inner.finish_session(generation);
        assert!(state.acquire_unmounted_batch_restore().is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_recovery_retains_unexpected_disconnect_but_respects_explicit_unmount() {
        let (state, root) = fixture();
        assert_eq!(state.inner.requested_epoch().unwrap(), None);
        let epoch = state.inner.request_mount().unwrap();
        assert_eq!(state.inner.requested_epoch().unwrap(), Some(epoch));
        let generation = state.inner.begin_session(epoch).unwrap();
        state.inner.finish_session(generation);
        assert_eq!(state.inner.requested_epoch().unwrap(), Some(epoch));
        state.inner.cancel_session();
        assert_eq!(state.inner.requested_epoch().unwrap(), None);
        assert!(state.inner.begin_session(epoch).is_err());
        let fresh = state.inner.request_mount().unwrap();
        assert_ne!(fresh, epoch);
        assert_eq!(state.inner.requested_epoch().unwrap(), Some(fresh));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_retry_cannot_revive_a_signout_even_after_a_new_request() {
        let (state, root) = fixture();
        let old = state.inner.request_mount().unwrap();
        state.inner.cancel_session();
        let current = state.inner.request_mount().unwrap();
        assert_ne!(old, current);
        assert!(state.inner.begin_session(old).is_err());
        assert!(state.inner.begin_session(current).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delayed_old_worker_completion_preserves_the_recovered_mount() {
        let (state, root) = fixture();
        let old = state.inner.begin_session(0).unwrap();
        let current = state.inner.begin_session(0).unwrap();
        assert!(state.inner.publish_session(current, &root).unwrap());
        state.inner.finish_session(old);
        assert!(state.inner.mounted.load(Ordering::SeqCst));
        assert!(state.inner.publish_session(current, &root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_worker_can_publish_after_the_callers_startup_wait_has_expired() {
        let (state, root) = fixture();
        let current = state.inner.begin_session(0).unwrap();
        assert!(state.inner.session_is_starting().unwrap());
        // The caller timing out must neither abandon nor duplicate this worker.
        assert!(!state.inner.mounted.load(Ordering::SeqCst));
        assert!(state.inner.publish_session(current, &root).unwrap());
        assert!(state.inner.mounted.load(Ordering::SeqCst));
        assert!(!state.inner.session_is_starting().unwrap());
        state.inner.finish_session(current);
        assert!(!state.inner.session_is_starting().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_worker_completion_clears_the_mount_and_cannot_be_republished() {
        let (state, root) = fixture();
        let current = state.inner.begin_session(0).unwrap();
        assert!(state.inner.publish_session(current, &root).unwrap());
        state.inner.finish_session(current);
        assert!(!state.inner.mounted.load(Ordering::SeqCst));
        assert!(!state.inner.publish_session(current, &root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_old_worker_cannot_publish_over_a_new_session() {
        let (state, root) = fixture();
        let old = state.inner.begin_session(0).unwrap();
        let current = state.inner.begin_session(0).unwrap();
        assert!(!state.inner.publish_session(old, &root).unwrap());
        assert!(state.inner.publish_session(current, &root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signout_during_preparation_cancels_the_original_mount_request() {
        let (state, root) = fixture();
        let epoch = state.inner.cancellation_epoch().unwrap();
        state.inner.cancel_session();
        assert!(state.inner.begin_session(epoch).is_err());
        let fresh_epoch = state.inner.cancellation_epoch().unwrap();
        assert!(state.inner.begin_session(fresh_epoch).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signout_during_slow_mount_startup_blocks_late_publication() {
        let (state, root) = fixture();
        let current = state.inner.begin_session(0).unwrap();
        state.inner.cancel_session();
        assert!(!state.inner.publish_session(current, &root).unwrap());
        assert!(state.inner.session_is_starting().unwrap());
        state.inner.finish_session(current);
        assert!(!state.inner.session_is_starting().unwrap());
        assert!(!state.inner.mounted.load(Ordering::SeqCst));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cached_claim_without_a_mount_path_is_not_connected() {
        let root = std::env::temp_dir().join(format!("tr-health-{:016x}", rand::random::<u64>()));
        let state = DriveMountState::new(0, "fixture".into(), root.clone()).unwrap();
        state.inner.mounted.store(true, Ordering::SeqCst);
        assert!(!state.status().mounted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_detached_mount_directory_is_not_reported_as_connected() {
        let root = std::env::temp_dir().join(format!("tr-health-{:016x}", rand::random::<u64>()));
        let state = DriveMountState::new(0, "fixture".into(), root.clone()).unwrap();
        let path = root.join("detached");
        std::fs::create_dir_all(&path).unwrap();
        *state.inner.mount_point.lock().unwrap() = Some(path);
        state.inner.mounted.store(true, Ordering::SeqCst);
        assert!(!state.status().mounted);
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl DriveMountState {
    /// Tell the running Linux FUSE session about committed namespace changes.
    /// Never hold a SQLite lock or call from a FUSE callback while notifying.
    ///
    /// Batch only genuinely changed namespace entries from a committed remote
    /// manifest. Do not hold the SQLite lock during kernel notifications and
    /// never reveal private file names in diagnostic logs.
    #[cfg(target_os = "linux")]
    pub(crate) fn notify_namespace_changed(
        &self,
        entries: &[(Option<String>, String)],
        inodes: &[(String, String)],
    ) {
        if entries.is_empty() && inodes.is_empty() {
            return;
        }
        let notifier = match self.inner.session.lock() {
            Ok(session)
                if session.worker_running
                    && !session.cancelled
                    && self.inner.mounted.load(Ordering::SeqCst) =>
            {
                session.notifier.clone()
            }
            _ => None,
        };
        let Some(notifier) = notifier else {
            return;
        };
        let mut parents = std::collections::HashSet::new();
        for (parent, name) in entries {
            let parent_ino = parent
                .as_deref()
                .map(|id| desktop::inode_for("dir", id))
                .unwrap_or(desktop::ROOT_INODE);
            if let Err(error) = notifier.inval_entry(parent_ino, std::ffi::OsStr::new(name)) {
                log::debug!("Drive FUSE entry invalidation unavailable: {error}");
            }
            parents.insert(parent_ino);
        }
        for parent_ino in parents {
            if let Err(error) = notifier.inval_inode(parent_ino, 0, 0) {
                log::debug!("Drive FUSE parent invalidation unavailable: {error}");
            }
        }
        for (kind, id) in inodes {
            let inode = desktop::inode_for(kind, id);
            if let Err(error) = notifier.inval_inode(inode, 0, 0) {
                log::debug!("Drive FUSE changed inode invalidation unavailable: {error}");
            }
        }
    }

    /// Exclusively hold the mount-start gate during an atomic local Drive
    /// restore. Refuse when a session is active, starting, or still mounted
    /// in the kernel, even if a stale flag temporarily reports unmounted.
    #[cfg(target_os = "linux")]
    pub(crate) fn acquire_unmounted_batch_restore(
        &self,
    ) -> Result<BatchRestoreUnmountGuard, String> {
        if self.inner.mounting.swap(true, Ordering::SeqCst) {
            return Err(
                "Drive mount is starting or another exclusive restore is in progress".into(),
            );
        }
        let guard = BatchRestoreUnmountGuard(self.inner.clone());
        {
            let session = self
                .inner
                .session
                .lock()
                .map_err(|_| "Drive mount session unavailable".to_string())?;
            if self.inner.mounted.load(Ordering::SeqCst) || session.worker_running {
                return Err("Unmount TeraRelay Drive before batch restore".into());
            }
        }
        let point = self
            .inner
            .mount_point
            .lock()
            .map_err(|_| "Drive mount path unavailable".to_string())?
            .clone();
        if point
            .as_ref()
            .is_some_and(|p| desktop::mountpoint_is_active(p))
        {
            return Err(
                "The kernel Drive mount is still active; unmount before batch restore".into(),
            );
        }
        Ok(guard)
    }

    // This read-only owned mount context also guards verified bulk content
    // exports; callers recheck generation between filesystem reads.
    #[cfg(target_os = "linux")]
    pub(crate) fn restore_context(&self) -> Result<Option<(u64, PathBuf)>, String> {
        let session = self
            .inner
            .session
            .lock()
            .map_err(|_| "Drive session poisoned")?;
        if !self.inner.mounted.load(Ordering::SeqCst)
            || session.cancelled
            || !session.worker_running
        {
            return Ok(None);
        }
        let path = self
            .inner
            .mount_point
            .lock()
            .map_err(|_| "Drive mount path poisoned")?
            .clone()
            .ok_or("Drive mount has no owned path")?;
        Ok(Some((session.generation, path)))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn restore_trash_item(
        &self,
        db: &DbConnection,
        kind: &str,
        id: &str,
    ) -> Result<String, String> {
        use crate::drive_restore::{self, RestoreKind};
        let kind = RestoreKind::parse(kind)?;
        let Some(context) = self.restore_context()? else {
            let conn = db.lock().map_err(|_| "DB poisoned")?;
            return crate::commands::drive_metadata::restore_drive_trash_item(
                &conn,
                kind.as_str(),
                id,
            );
        };
        let plan = {
            let conn = db.lock().map_err(|_| "DB poisoned")?;
            drive_restore::plan(&conn, kind, id)?
        };
        let owner = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        let identity = format!("{:032x}:{}:{}", self.inner.mount_identity, context.0, owner);
        // Pin the proven FUSE root before registering a request. No DB/session
        // mutex may be held while the kernel calls back into this filesystem.
        let parent = drive_restore::open_parent(&context.1, &plan, &identity)?;
        if self.restore_context()?.as_ref() != Some(&context) {
            return Err("Drive mount changed while preparing restoration".into());
        }
        let guard = self.inner.native_restores.reserve(owner, context.0, plan)?;
        let native = drive_restore::create_native(&parent, &guard.request.plan);
        match guard.result()? {
            Some(Ok(name)) => match native {
                Ok(()) => Ok(name),
                Err(error) => Err(format!(
                    "Restored {name}, but its native notification failed: {error}. Refresh Files."
                )),
            },
            Some(Err(error)) => Err(error),
            None => Err(native.err().unwrap_or_else(|| {
                "Native restore was not acknowledged by the owned mount".into()
            })),
        }
    }

    pub fn new(
        stream_port: u16,
        stream_token: String,
        cache_root: PathBuf,
    ) -> Result<Self, String> {
        let drive_root = cache_root.join("drive");
        let block_cache = drive_root.join("blocks");
        let staging_root = drive_root.join("staging");
        // Cloud write staging can contain plaintext before encryption and
        // pending upload. Never leave any of these app-owned directories
        // world-readable, including directories from earlier prototypes.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            for path in [&drive_root, &block_cache, &staging_root] {
                crate::cache_directory::private_directory(path)
                    .map_err(|e| format!("Failed to secure private Drive cache: {e}"))?;
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        for path in [&block_cache, &staging_root] {
            std::fs::create_dir_all(path)
                .map_err(|e| format!("Failed to create Drive cache directory: {e}"))?;
        }
        purge_legacy_plaintext_read_cache(&block_cache);
        Ok(Self {
            inner: Arc::new(DriveMountInner {
                mounted: AtomicBool::new(false),
                mount_point: Mutex::new(None),
                mounting: AtomicBool::new(false),
                #[cfg(target_os = "linux")]
                safe_mount_read_only: AtomicBool::new(false),
                native_recovery_running: AtomicBool::new(false),
                #[cfg(target_os = "linux")]
                automatic_upload_recovery_running: AtomicBool::new(false),
                #[cfg(target_os = "linux")]
                transfer: Mutex::new(transfer_center::QueueRuntime::default()),
                #[cfg(target_os = "linux")]
                session: Mutex::new(LinuxMountSession::default()),
                active_pending: Mutex::new(PendingActivity::default()),
                #[cfg(target_os = "linux")]
                native_restores: crate::drive_restore::RestoreRequests::default(),
                #[cfg(target_os = "linux")]
                mount_identity: rand::random(),
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
            .map(|pending| pending.active.contains(pending_id))
            .unwrap_or(true) // Poisoned ownership must never permit a second upload.
    }

    pub(crate) fn set_pending_active(&self, pending_id: &str, active: bool) {
        if let Ok(mut pending) = self.inner.active_pending.lock() {
            if active {
                pending.active.insert(pending_id.to_string());
            } else if !pending.recovering.contains(pending_id) {
                pending.active.remove(pending_id);
            }
        }
    }

    /// Refuse writable reopens while background recovery owns the stage.
    #[cfg(target_os = "linux")]
    fn try_open_pending_writer(&self, pending_id: &str) -> Option<bool> {
        let mut pending = self.inner.active_pending.lock().ok()?;
        if pending.recovering.contains(pending_id) {
            return None;
        }
        Some(!pending.active.insert(pending_id.to_string()))
    }

    /// Claim an upload atomically with native writer ownership. A Drop guard
    /// releases the claim on every error or cancellation path. Also used by
    /// local rescue-pack exports/restores to freeze the pending staging tree.
    pub(crate) fn claim_pending_recovery(&self, pending_id: &str) -> Option<PendingRecoveryClaim> {
        let mut pending = self.inner.active_pending.lock().ok()?;
        if !pending.active.insert(pending_id.to_string()) {
            return None;
        }
        pending.recovering.insert(pending_id.to_string());
        Some(PendingRecoveryClaim {
            inner: self.inner.clone(),
            id: pending_id.to_string(),
        })
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub(crate) fn mark_provider_connected(&self, path: Option<PathBuf>) {
        if let Ok(mut slot) = self.inner.mount_point.lock() {
            *slot = path;
        }
        self.inner.mounting.store(false, Ordering::SeqCst);
        self.inner.mounted.store(true, Ordering::SeqCst);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub(crate) fn mark_provider_disconnected(&self) {
        self.inner.mounted.store(false, Ordering::SeqCst);
        self.inner.mounting.store(false, Ordering::SeqCst);
    }

    fn status(&self) -> DriveMountStatus {
        let desktop = cfg!(any(
            target_os = "linux",
            target_os = "windows",
            target_os = "macos"
        ));
        let provider_kind = if cfg!(target_os = "linux") {
            "fuse"
        } else if cfg!(target_os = "windows") {
            "windows-cloud-files"
        } else if cfg!(target_os = "macos") {
            "macos-file-provider"
        } else {
            "unsupported"
        };
        let mount_point = self
            .inner
            .mount_point
            .lock()
            .ok()
            .and_then(|path| path.clone());
        let mounted = self.inner.mounted.load(Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        let mounted = mounted
            && mount_point
                .as_ref()
                .is_some_and(|path| desktop::mountpoint_is_active(path));
        DriveMountStatus {
            supported: desktop,
            mounted,
            native_recovery: self.inner.native_recovery_running.load(Ordering::SeqCst),
            automatic_upload_recovery: {
                #[cfg(target_os = "linux")]
                {
                    self.inner
                        .automatic_upload_recovery_running
                        .load(Ordering::SeqCst)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            },
            mount_point: mount_point.map(|value| value.to_string_lossy().into_owned()),
            read_streaming: desktop,
            write_staging: desktop,
            provider_kind: provider_kind.to_string(),
        }
    }
}

#[tauri::command]
pub fn cmd_drive_status(state: State<'_, DriveMountState>) -> DriveMountStatus {
    state.status()
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn build_drive_fs_core(
    app_handle: &AppHandle,
    state: &DriveMountState,
    db_pool: &DbConnection,
    telegram_state: &crate::commands::TelegramState,
) -> Result<DriveFsCore, String> {
    desktop::build_core(app_handle, state, db_pool, telegram_state)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod desktop {
    #[cfg(target_os = "linux")]
    use super::read_lifetime::{
        overlay_ranges, recover_retained_stages, FrozenV2Locations, InodeLifetimes, RetainedStage,
    };
    use super::*;
    use crate::commands::drive_metadata::DriveSnapshot;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    use crate::drive_fs_core::DriveFsCore;
    use crate::drive_fs_core::{
        DriveFsBackend, DriveFsError, DriveNodeId, DriveNodeKind, DriveNodeMetadata,
        DriveRecoverySummary, DriveVirtualCapacity,
    };
    #[cfg(target_os = "linux")]
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
        ReplyWrite, Request, TimeOrNow,
    };
    #[cfg(target_os = "linux")]
    use libc::{EACCES, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, EROFS};
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};
    #[cfg(target_os = "linux")]
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    #[cfg(unix)]
    use std::os::unix::fs::{FileExt as UnixFileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    #[cfg(windows)]
    use std::os::windows::fs::{FileExt as WindowsFileExt, OpenOptionsExt};
    #[cfg(target_os = "linux")]
    use std::process::Command;
    use std::time::{Duration, UNIX_EPOCH};
    #[cfg(target_os = "linux")]
    use std::time::{Instant, SystemTime};

    // Treat only a proven missing child as an available creation slot.
    // A broken SQLite lookup or unreadable child must not be mistaken for
    // absence, otherwise a create can collide with an existing Drive item.
    fn require_missing_for_creation<T>(
        lookup: Result<T, DriveFsError>,
    ) -> Result<(), DriveFsError> {
        match lookup {
            Ok(_) => Err(DriveFsError::AlreadyExists),
            Err(DriveFsError::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(target_os = "linux")]
    fn require_fuse_directory_parent(
        parent: Result<Option<Option<String>>, String>,
    ) -> Result<Option<String>, i32> {
        match parent {
            Ok(Some(directory_id)) => Ok(directory_id),
            Ok(None) => Err(ENOTDIR),
            Err(_) => Err(EIO),
        }
    }

    #[cfg(target_os = "linux")]
    fn require_missing_fuse_child<T>(lookup: Result<Option<T>, String>) -> Result<(), i32> {
        match lookup {
            Ok(Some(_)) => Err(EEXIST),
            Ok(None) => Ok(()),
            Err(_) => Err(EIO),
        }
    }

    #[cfg(test)]
    mod creation_lookup_guard_tests {
        use super::{require_missing_for_creation, DriveFsError};

        #[test]
        fn core_create_rejects_existing_and_lookup_errors_without_downgrading_to_absent() {
            assert_eq!(
                require_missing_for_creation(Ok(())),
                Err(DriveFsError::AlreadyExists)
            );
            assert_eq!(
                require_missing_for_creation(Err::<(), _>(DriveFsError::NotFound)),
                Ok(())
            );
            for error in [
                DriveFsError::Unavailable("SQLite lookup failed".into()),
                DriveFsError::InvalidInput("Corrupt child metadata".into()),
                DriveFsError::Internal("Database lock poisoned".into()),
            ] {
                assert_eq!(
                    require_missing_for_creation(Err::<(), _>(error.clone())),
                    Err(error)
                );
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn fuse_create_and_mkdir_preserve_directory_lookup_errors() {
            use super::require_fuse_directory_parent;
            assert_eq!(require_fuse_directory_parent(Ok(Some(None))), Ok(None));
            assert_eq!(
                require_fuse_directory_parent(Ok(Some(Some("valid-dir".into())))),
                Ok(Some("valid-dir".into()))
            );
            assert_eq!(require_fuse_directory_parent(Ok(None)), Err(libc::ENOTDIR));
            assert_eq!(
                require_fuse_directory_parent(Err("SQLite database unavailable".into())),
                Err(libc::EIO)
            );

            let source = include_str!("drive_mount.rs");
            for (start, end) in [
                ("fn create(\n", "fn write(\n"),
                ("fn mkdir(\n", "fn rename(\n"),
            ] {
                let callback = source
                    .rsplit_once(start)
                    .unwrap()
                    .1
                    .split_once(end)
                    .unwrap()
                    .0;
                assert!(callback.contains("require_fuse_directory_parent("));
                assert!(!callback.contains("directory_id_for_inode(parent).ok().flatten()"));
                assert!(callback.contains("reply.error(errno)"));
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn fuse_create_rejects_corrupt_lookups_and_existing_names() {
            use super::require_missing_fuse_child;
            assert_eq!(require_missing_fuse_child(Ok(Some(()))), Err(libc::EEXIST));
            assert_eq!(
                require_missing_fuse_child(Ok::<Option<()>, String>(None)),
                Ok(())
            );
            assert_eq!(
                require_missing_fuse_child(Err::<Option<()>, _>("SQLite error".into())),
                Err(libc::EIO)
            );
        }
    }

    // Event-driven, zero-network latency diagnostics for the intermittent
    // ~10-second folder-open complaint. Log ONLY expensive FUSE requests;
    // filenames, user paths and payloads never enter diagnostic logs.
    #[cfg(target_os = "linux")]
    struct SlowFuseRequest {
        operation: &'static str,
        started: Instant,
    }

    #[cfg(target_os = "linux")]
    impl SlowFuseRequest {
        fn new(operation: &'static str) -> Self {
            Self {
                operation,
                started: Instant::now(),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SlowFuseRequest {
        fn drop(&mut self) {
            let elapsed = self.started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                log::warn!(
                    "Slow native Drive FUSE {} request: {} ms (no paths logged)",
                    self.operation,
                    elapsed.as_millis(),
                );
            }
        }
    }

    // The native FUSE session executes callbacks serially. A slow read,
    // close/finalization, or SQLite enumeration can therefore delay an
    // unrelated file-manager folder open even when its own lookup is quick.
    // Keep phase-only (no pathname, inode, message, or content) warnings
    // so an intermittent incident can be attributed to its real phase.
    #[cfg(target_os = "linux")]
    struct SlowFusePhase {
        phase: &'static str,
        started: Instant,
    }

    #[cfg(target_os = "linux")]
    impl SlowFusePhase {
        fn new(phase: &'static str) -> Self {
            Self {
                phase,
                started: Instant::now(),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SlowFusePhase {
        fn drop(&mut self) {
            let elapsed = self.started.elapsed();
            if elapsed >= Duration::from_millis(250) {
                log::warn!(
                    "Native Drive FUSE phase {} took {} ms (no paths logged)",
                    self.phase,
                    elapsed.as_millis(),
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn report_slow_drive_db_wait(operation: &'static str, started: Instant) {
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(250) {
            log::warn!(
                "Native Drive {} waited {} ms for SQLite metadata mutex (no paths logged)",
                operation,
                elapsed.as_millis(),
            );
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn shared_inode_mutation_errno(error: &str) -> Option<i32> {
        // The metadata layer deliberately refuses POSIX-incomplete aliases.
        // Keep the denial explicit to callers rather than misreporting an
        // unexpected storage/transport I/O failure.
        match error {
            "Drive hardlink mutation requires shared-inode POSIX support"
            | "Drive hardlink editing requires shared-inode copy-on-write support" => {
                Some(libc::EPERM)
            }
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) const ROOT_INODE: u64 = 1;
    #[cfg(target_os = "linux")]
    fn listable_drive_xattrs(ino: u64) -> Vec<u8> {
        if ino != ROOT_INODE {
            return Vec::new();
        }
        // listxattr is a NUL-terminated sequence of names, not the value.
        // The restore owner token remains available only via explicit getxattr.
        let mut names = crate::drive_restore::IDENTITY_XATTR.as_bytes().to_vec();
        names.push(0);
        names
    }
    #[cfg(all(test, target_os = "linux"))]
    mod native_xattr_listing_tests {
        use super::{listable_drive_xattrs, ROOT_INODE};

        #[test]
        fn restore_owner_xattr_is_listed_only_on_drive_root() {
            let names = listable_drive_xattrs(ROOT_INODE);
            let mut expected = crate::drive_restore::IDENTITY_XATTR.as_bytes().to_vec();
            expected.push(0);
            assert_eq!(names, expected);
            assert_eq!(names.iter().filter(|byte| **byte == 0).count(), 1);
            assert!(listable_drive_xattrs(ROOT_INODE + 1).is_empty());

            let source = include_str!("drive_mount.rs");
            let callback = source
                .rsplit_once("fn listxattr(")
                .expect("native listxattr callback must be implemented")
                .1
                .split_once("fn mknod(")
                .expect("xattr callback must finish before mknod")
                .0;
            assert!(callback.contains("listable_drive_xattrs(ino)"));
            assert!(callback.contains("reply.size(names.len() as u32)"));
            assert!(callback.contains("reply.error(libc::ERANGE)"));
            assert!(callback.contains("reply.data(&names)"));
        }
    }

    #[cfg(target_os = "linux")]
    const TTL: Duration = Duration::from_secs(1);
    const CACHE_BLOCK_SIZE: u64 = 4 * 1024 * 1024;
    const CACHE_MAX_BYTES: u64 = crate::cache_cleanup::DRIVE_CACHE_BYTES;

    /// A malformed cache or remote response must fail the read instead of
    /// returning successful but incomplete bytes (which applications may
    /// misinterpret as EOF). Clamp to one block, not the available byte count.
    fn checked_remote_block_slice(
        bytes: &[u8],
        block_start: u64,
        request_start: u64,
        request_end: u64,
    ) -> Result<&[u8], String> {
        let from = request_start.saturating_sub(block_start);
        let to = request_end
            .saturating_sub(block_start)
            .min(CACHE_BLOCK_SIZE);
        let from = usize::try_from(from).map_err(|_| "Drive remote block range overflow")?;
        let to = usize::try_from(to).map_err(|_| "Drive remote block range overflow")?;
        bytes
            .get(from..to)
            .ok_or_else(|| "Drive remote block is incomplete".to_string())
    }

    #[cfg(test)]
    mod remote_block_slice_tests {
        use super::{checked_remote_block_slice, CACHE_BLOCK_SIZE};

        #[test]
        fn truncated_remote_block_is_an_error_not_successful_short_data() {
            assert_eq!(
                checked_remote_block_slice(b"abcdefghij", 0, 2, 7).unwrap(),
                b"cdefg"
            );
            assert_eq!(
                checked_remote_block_slice(
                    b"abcdefghij",
                    CACHE_BLOCK_SIZE,
                    CACHE_BLOCK_SIZE + 2,
                    CACHE_BLOCK_SIZE + 7
                )
                .unwrap(),
                b"cdefg"
            );
            assert!(
                checked_remote_block_slice(b"abcdefghij", 0, 2, 12).is_err(),
                "remote short data must not be reported as success"
            );
            assert!(
                checked_remote_block_slice(b"abcdefghij", 0, 11, 12).is_err(),
                "reading beyond the returned block must not fake EOF"
            );
        }
    }
    // Only bounded chunks may reside locally. Scatter/random writes must not
    // silently consume the entire local disk while waiting to be completed.
    const STAGING_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

    fn reject_symlink_file(path: &Path) -> Result<(), String> {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                return Err("Drive staging path cannot be a symbolic link".to_string());
            }
        }
        Ok(())
    }

    fn open_private_read(path: &Path) -> Result<File, String> {
        reject_symlink_file(path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        options
            .open(path)
            .map_err(|e| format!("Could not open private Drive file for reading: {e}"))
    }

    // A single native read may cross many 16 MiB chunks. The staged chunk
    // ledger and dirty ranges describe one consistent local provenance view;
    // fetch them only once after discovering a local chunk. Otherwise each
    // chunk would repeat two complete SQLite scans and could observe a
    // different remote-upload state midway through the same read.
    struct PendingReadProvenance {
        uploaded_chunks: HashSet<u64>,
        ranges: Vec<(u64, u64)>,
    }

    fn pending_read_provenance_once<F>(
        cache: &mut Option<PendingReadProvenance>,
        load: F,
    ) -> Result<&PendingReadProvenance, String>
    where
        F: FnOnce() -> Result<PendingReadProvenance, String>,
    {
        if cache.is_none() {
            *cache = Some(load()?);
        }
        cache
            .as_ref()
            .ok_or_else(|| "Drive staged read provenance is unavailable".to_string())
    }

    #[cfg(test)]
    mod pending_read_provenance_tests {
        use super::{pending_read_provenance_once, PendingReadProvenance};
        use std::cell::Cell;
        use std::collections::HashSet;

        #[test]
        fn multi_chunk_pending_read_loads_metadata_once_and_never_swallows_errors() {
            let fetches = Cell::new(0);
            let mut metadata = None;
            assert_eq!(fetches.get(), 0, "remote-only reads must stay query-free");
            for index in [1, 2, 3] {
                let loaded = pending_read_provenance_once(&mut metadata, || {
                    fetches.set(fetches.get() + 1);
                    Ok(PendingReadProvenance {
                        uploaded_chunks: HashSet::from([2]),
                        ranges: vec![(0, 16), (32, 48)],
                    })
                })
                .unwrap();
                assert_eq!(loaded.uploaded_chunks.contains(&index), index == 2);
                assert_eq!(loaded.ranges, vec![(0, 16), (32, 48)]);
            }
            assert_eq!(
                fetches.get(),
                1,
                "a read crossing local chunks must load SQL only once"
            );

            let mut failed = None;
            assert!(pending_read_provenance_once(&mut failed, || {
                Err("SQLite integrity failure".into())
            })
            .is_err());
            assert!(
                failed.is_none(),
                "a query failure must not cache an empty provenance"
            );
            assert!(pending_read_provenance_once(&mut failed, || {
                Ok(PendingReadProvenance {
                    uploaded_chunks: HashSet::new(),
                    ranges: vec![(0, 1)],
                })
            })
            .is_ok());

            let source = include_str!("drive_mount.rs");
            let callback = source
                .rsplit_once("fn read_pending_range(\n")
                .unwrap()
                .1
                .split_once("fn write_pending_bytes(\n")
                .unwrap()
                .0;
            assert!(callback.contains("let mut provenance = None"));
            assert!(callback.contains("pending_read_provenance_once(&mut provenance"));
            assert_eq!(
                callback
                    .matches("drive_pending_chunks(&conn, &pending.id)")
                    .count(),
                1
            );
            assert_eq!(
                callback
                    .matches("drive_pending_ranges(&conn, &pending.id)")
                    .count(),
                1
            );
            assert!(callback.contains("metadata.uploaded_chunks.contains(&index)"));
            assert!(callback.contains("&metadata.ranges"));
        }
    }

    /// The full materializer fetches remote pending chunks or a COW base
    /// before overlaying staged bytes. Preserve that path whenever either
    /// source may exist: it also verifies encrypted V2 keys and checksums.
    fn staged_read_can_skip_remote_provenance(
        has_cow_base: bool,
        has_uploaded_chunk: bool,
    ) -> bool {
        !has_cow_base && !has_uploaded_chunk
    }

    /// Dirty ranges are an ordered union of staged bytes. Only a *fully*
    /// covered request may skip the uploaded/base data and whole-chunk merge.
    /// Adjoining ranges count as coverage, but even one missing byte doesn't.
    fn staged_ranges_cover(ranges: &[(u64, u64)], start: u64, end: u64) -> bool {
        if end <= start {
            return false;
        }
        let mut cursor = start;
        for &(range_start, range_end) in ranges {
            if range_end <= cursor {
                continue;
            }
            if range_start > cursor {
                return false;
            }
            cursor = range_end.min(end);
            if cursor == end {
                return true;
            }
        }
        false
    }

    /// The fast path never invents zero bytes or returns an incomplete read.
    /// It reads only the requested portion from the already-written staging
    /// chunk. A partial dirty overlay must use the established merge path.
    fn read_covered_staged_slice(
        path: &Path,
        ranges: &[(u64, u64)],
        chunk_start: u64,
        start: u64,
        end: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        let chunk_end = chunk_start
            .checked_add(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
            .ok_or_else(|| "Drive read chunk overflow".to_string())?;
        if start < chunk_start || end > chunk_end || end <= start {
            return Err("Drive staged read is outside its chunk".to_string());
        }
        if !staged_ranges_cover(ranges, start, end) {
            return Ok(None);
        }
        let offset = start - chunk_start;
        let length = usize::try_from(end - start)
            .map_err(|_| "Drive staged read is too large".to_string())?;
        let file = open_private_read(path)?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() < offset + length as u64 {
            return Err("Drive staged read has incomplete local bytes".to_string());
        }
        let mut bytes = vec![0u8; length];
        let read = positioned_read(&file, &mut bytes, offset)
            .map_err(|e| format!("Could not read local Drive dirty bytes: {e}"))?;
        if read != length {
            return Err("Drive staged read was truncated".to_string());
        }
        Ok(Some(bytes))
    }

    // Legacy ciphertext has no separate stored checksum. Authenticate before
    // trusting a cached block; if authentication fails, discard that cache and
    // fetch exactly one fresh remote copy in this same read. Never return
    // remote data without authenticating it, and never loop on a bad source.
    fn authenticated_legacy_cache_read(
        cached: Option<Vec<u8>>,
        mut fetch_remote: impl FnMut() -> Result<Vec<u8>, String>,
        mut authenticate: impl FnMut(&[u8]) -> Result<Vec<u8>, String>,
    ) -> Result<(Vec<u8>, Vec<u8>, bool), String> {
        if let Some(encoded) = cached {
            if let Ok(plaintext) = authenticate(&encoded) {
                return Ok((encoded, plaintext, false));
            }
        }
        let encoded = fetch_remote()?;
        let plaintext = authenticate(&encoded)?;
        Ok((encoded, plaintext, true))
    }

    #[cfg(test)]
    mod legacy_cache_repair_tests {
        use super::authenticated_legacy_cache_read;
        use crate::drive_crypto::{decrypt_chunk, encrypt_chunk};
        use std::cell::Cell;

        #[test]
        fn corrupted_legacy_ciphertext_recovers_in_one_authenticated_read() {
            let key = [42u8; 32];
            let plaintext = b"repair encrypted cached bytes";
            let valid = encrypt_chunk(&key, "cache-fixture", 0, plaintext).unwrap();
            let mut corrupt = valid.clone();
            let last = corrupt.len() - 1;
            corrupt[last] ^= 1;
            let fetched = Cell::new(0);
            let decode = |encoded: &[u8]| {
                decrypt_chunk(&key, "cache-fixture", 0, plaintext.len() as u64, encoded)
            };
            let (encoded, bytes, from_remote) = authenticated_legacy_cache_read(
                Some(corrupt.clone()),
                || {
                    fetched.set(fetched.get() + 1);
                    Ok(valid.clone())
                },
                decode,
            )
            .unwrap();
            assert_eq!(bytes, plaintext);
            assert_eq!(encoded, valid);
            assert!(from_remote);
            assert_eq!(fetched.get(), 1);

            let (encoded, bytes, from_remote) = authenticated_legacy_cache_read(
                Some(valid.clone()),
                || panic!("healthy authenticated cache must not hit Telegram"),
                decode,
            )
            .unwrap();
            assert_eq!(encoded, valid);
            assert_eq!(bytes, plaintext);
            assert!(!from_remote);
        }

        #[test]
        fn corrupt_remote_ciphertext_or_network_failure_never_returns_plaintext() {
            let key = [23u8; 32];
            let plain = b"authenticated";
            let mut corrupt = encrypt_chunk(&key, "read-fixture", 4, plain).unwrap();
            let last = corrupt.len() - 1;
            corrupt[last] ^= 1;
            let decode = |encoded: &[u8]| {
                decrypt_chunk(&key, "read-fixture", 4, plain.len() as u64, encoded)
            };
            let tries = Cell::new(0);
            assert!(authenticated_legacy_cache_read(
                Some(corrupt.clone()),
                || {
                    tries.set(tries.get() + 1);
                    Ok(corrupt.clone())
                },
                decode,
            )
            .is_err());
            assert_eq!(tries.get(), 1);
            assert!(authenticated_legacy_cache_read(
                Some(corrupt),
                || Err("Remote Drive unavailable".into()),
                decode,
            )
            .is_err());
        }
    }

    /// A cache entry is disposable and must be a complete bounded block.
    /// Check the open file's size before allocating, then limit the read too:
    /// a concurrent grow/truncate must never become a short successful read.
    fn read_cached_block(path: &Path, expected: u64) -> Option<Vec<u8>> {
        use std::io::Read;
        let maximum = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE
            + crate::drive_stream::DRIVE_ENCRYPTED_CHUNK_OVERHEAD;
        if expected == 0 || expected > maximum {
            return None;
        }
        let file = open_private_read(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() != expected {
            return None;
        }
        let mut bytes = Vec::with_capacity(expected as usize);
        file.take(expected + 1).read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 == expected).then_some(bytes)
    }

    /// Concurrent remote FUSE reads may miss the same cache entry. Using a
    /// fixed `block.tmp` filename lets two writers truncate each other's
    /// staging bytes, and a rename may publish a partially overwritten file.
    /// Publish one complete disposable block at a time via a unique private
    /// temp file. Any failed write is best-effort and leaves the fetch valid.
    fn write_cached_block_atomic(path: &Path, bytes: &[u8]) -> bool {
        use std::io::Write;
        let Some(parent) = path.parent() else {
            return false;
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let directory = match crate::cache_directory::private_directory(parent) {
            Ok(directory) => directory,
            Err(_) => return false,
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
        for _ in 0..4 {
            let temp = path.with_extension(format!("tmp-{:032x}", rand::random::<u128>()));
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let opened = crate::cache_directory::open_child(
                &directory,
                temp.file_name().unwrap(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            );
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let opened = {
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                options.open(&temp)
            };
            let mut file = match opened {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return false,
            };
            // Hold the lock through publication, so even a long-paused writer
            // is protected from age-based background orphan reclamation.
            let wrote = file.lock().is_ok() && file.write_all(bytes).is_ok();
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let renamed = wrote
                && crate::cache_directory::rename_child(
                    &directory,
                    temp.file_name().unwrap(),
                    path.file_name().unwrap(),
                )
                .is_ok();
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let renamed = {
                drop(file);
                wrote && std::fs::rename(&temp, path).is_ok()
            };
            if renamed {
                return true;
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let _ = crate::cache_directory::remove_child(&directory, temp.file_name().unwrap());
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let _ = std::fs::remove_file(&temp);
            return false;
        }
        false
    }

    #[cfg(test)]
    mod concurrent_cache_publish_tests {
        use super::write_cached_block_atomic;
        use std::sync::{Arc, Barrier};

        #[cfg(unix)]
        #[test]
        fn cache_publication_creates_private_parents_and_refuses_symlink_ancestors() {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let root = std::env::temp_dir()
                .join(format!("tr-cache-parents-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&root).unwrap();
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            symlink(&outside, root.join("redirect")).unwrap();
            let redirected = write_cached_block_atomic(
                &root.join("redirect/child/0000000000000000.bin"),
                b"private",
            );
            let escaped = outside.join("child").exists();
            let target = root.join("owned/nested/0000000000000000.bin");
            assert!(write_cached_block_atomic(&target, b"private"));
            let modes: Vec<_> = [root.join("owned"), root.join("owned/nested")]
                .iter()
                .map(|p| std::fs::metadata(p).unwrap().permissions().mode() & 0o777)
                .collect();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                !redirected && !escaped,
                "cache writer followed a symlink and created files outside its tree"
            );
            assert_eq!(modes, [0o700, 0o700]);
        }

        #[cfg(unix)]
        #[test]
        fn cache_initialization_rejects_symlink_before_creating_children() {
            use std::os::unix::fs::symlink;
            let root =
                std::env::temp_dir().join(format!("tr-cache-init-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&root).unwrap();
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            symlink(&outside, root.join("drive")).unwrap();
            let result =
                crate::drive_mount::DriveMountState::new(1, "fake-token".into(), root.clone());
            let changed = std::fs::read_dir(&outside).unwrap().next().is_some();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.is_err());
            assert!(
                !changed,
                "initialization created children through a rejected symlink"
            );
        }

        #[cfg(unix)]
        #[test]
        fn cached_blocks_are_private_even_with_permissive_process_umask() {
            use std::os::unix::fs::PermissionsExt;
            let root = std::env::temp_dir()
                .join(format!("tr-private-cache-{:032x}", rand::random::<u128>()));
            let target = root.join("owned").join("encrypted.bin");
            assert!(write_cached_block_atomic(&target, &[10, 20, 30]));
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            let bytes = std::fs::read(&target).unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert_eq!(
                mode, 0o600,
                "cached remote data must be owner-readable only"
            );
            assert_eq!(bytes, [10, 20, 30]);
        }

        #[test]
        fn concurrent_cache_writers_never_publish_mixed_or_partial_bytes() {
            let root = std::env::temp_dir().join(format!(
                "tr-cache-concurrent-{:032x}",
                rand::random::<u128>()
            ));
            let target = root.join("block").join("0001.bin");
            let barrier = Arc::new(Barrier::new(9));
            let mut handles = Vec::new();
            for index in 0..8u8 {
                let target = target.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    let bytes = vec![index; 256 * 1024];
                    barrier.wait();
                    assert!(write_cached_block_atomic(&target, &bytes));
                }));
            }
            barrier.wait();
            for handle in handles {
                handle.join().unwrap();
            }
            let final_bytes = std::fs::read(&target).unwrap();
            assert_eq!(final_bytes.len(), 256 * 1024);
            assert!(
                (0..8u8).any(|index| final_bytes.iter().all(|byte| *byte == index)),
                "the published cache block must consist of one complete writer's bytes"
            );
            assert_eq!(
                std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
                1,
                "completed concurrent writers must not leave cache temp files"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// Require the precise requested range and bound acquisition even if the
    /// loopback server sends a full object or an unlimited chunked response.
    fn read_range_response(
        response: reqwest::blocking::Response,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let expected = end
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| "Drive stream byte range is invalid".to_string())?;
        // This bounds one read window, not the logical cloud file size.
        const MAX_WINDOW: u64 = 16 * 1024 * 1024 + 45;
        if expected > MAX_WINDOW {
            return Err("Drive stream read exceeds one bounded block".to_string());
        }
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(format!(
                "Drive stream returned HTTP {} for a range read",
                response.status()
            ));
        }
        let header = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "Drive stream Content-Range is missing".to_string())?;
        let valid_range = (|| -> Option<bool> {
            let (range, total) = header.strip_prefix("bytes ")?.split_once('/')?;
            let (first, last) = range.split_once('-')?;
            let first = first.parse::<u64>().ok()?;
            let last = last.parse::<u64>().ok()?;
            let total = total.parse::<u64>().ok()?;
            Some(first == start && last == end && end < total)
        })();
        if valid_range != Some(true) {
            return Err("Drive stream returned a different byte range".to_string());
        }
        if response
            .content_length()
            .is_some_and(|length| length != expected)
        {
            return Err(
                "Drive stream Content-Length does not match its requested range".to_string(),
            );
        }
        let mut bytes = Vec::with_capacity(expected as usize);
        response
            .take(expected + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Drive stream ended early: {e}"))?;
        if bytes.len() as u64 != expected {
            return Err(format!(
                "Drive stream expected {expected} bytes but received {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    fn create_private_file(path: &Path, truncate: bool) -> Result<File, String> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate);
        if !truncate {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(path)
            .map_err(|e| format!("Could not create private Drive file: {e}"))
    }

    fn open_or_create_private_file(path: &Path) -> Result<File, String> {
        reject_symlink_file(path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(path)
            .map_err(|e| format!("Could not open Drive staging file: {e}"))
    }

    #[cfg(unix)]
    fn positioned_read(file: &File, data: &mut [u8], offset: u64) -> std::io::Result<usize> {
        UnixFileExt::read_at(file, data, offset)
    }

    #[cfg(windows)]
    fn positioned_read(file: &File, data: &mut [u8], offset: u64) -> std::io::Result<usize> {
        WindowsFileExt::seek_read(file, data, offset)
    }

    #[cfg(unix)]
    fn positioned_write(file: &File, data: &[u8], offset: u64) -> std::io::Result<usize> {
        UnixFileExt::write_at(file, data, offset)
    }

    #[cfg(windows)]
    fn positioned_write(file: &File, data: &[u8], offset: u64) -> std::io::Result<usize> {
        WindowsFileExt::seek_write(file, data, offset)
    }

    // A single positioned_write may legally write fewer bytes than requested.
    // Do not publish an entire dirty chunk range until every byte is staged.
    // Preflight the final offset so overflow cannot leave a partial write.
    fn positioned_write_all_with(
        data: &[u8],
        offset: u64,
        mut write: impl FnMut(&[u8], u64) -> std::io::Result<usize>,
    ) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        let length = u64::try_from(data.len())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "Drive write length overflow"))?;
        offset
            .checked_add(length)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Drive write offset overflow"))?;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let at = offset + consumed as u64;
            match write(&data[consumed..], at) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "Drive staging write returned zero",
                    ))
                }
                Ok(count) if count <= data.len() - consumed => consumed += count,
                Ok(_) => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Drive staging wrote more bytes than requested",
                    ))
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn positioned_write_all(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
        positioned_write_all_with(data, offset, |remaining, at| {
            positioned_write(file, remaining, at)
        })
    }

    // Prepare an entire replacement boundary on a distinct private inode. An
    // interrupted or short write must not truncate an already-dirty chunk.
    // Retain the previous local tail: if the process exits before the following
    // SQL savepoint commits, the old size/ranges still find their original bytes.
    fn stage_resized_boundary_with(
        path: &Path,
        bytes: &[u8],
        write: impl FnMut(&File, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), String> {
        stage_resized_boundary_with_directory_sync(path, bytes, write, |parent| {
            #[cfg(unix)]
            {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                    .open(parent)?
                    .sync_all()
            }
            #[cfg(not(unix))]
            {
                let _ = parent;
                Ok(())
            }
        })
    }

    // The temporary inode is synced before rename. Post-publication failure
    // must not trick the caller into rolling back metadata for a visible file.
    fn stage_resized_boundary_with_directory_sync(
        path: &Path,
        bytes: &[u8],
        mut write: impl FnMut(&File, &[u8]) -> std::io::Result<()>,
        sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<(), String> {
        use std::io::copy;
        let parent = path
            .parent()
            .ok_or("Drive boundary staging path has no parent")?;
        ensure_private_staging_directory(parent)?;
        let temp = parent.join(format!(".resize-{:032x}.tmp", rand::random::<u128>()));
        let result = (|| {
            let mut staged = create_private_file(&temp, false)?;
            // exists() follows the target and treats dangling symlinks as
            // absent. A poisoned Drive staging entry is never a valid
            // replacement source, even if its symlink target is missing.
            let previous_exists = match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
                Ok(_) => return Err("Previous Drive chunk is not a regular private file".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(format!("Cannot inspect previous Drive stage: {error}")),
            };
            if previous_exists {
                // O_NOFOLLOW in open_private_read closes the replacement race.
                let mut previous = open_private_read(path)?;
                let old_len = previous
                    .metadata()
                    .map_err(|e| format!("Could not stat previous Drive stage: {e}"))?
                    .len();
                if old_len > crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE {
                    return Err("Previous Drive chunk exceeds bounded staging size".to_string());
                }
                let copied = copy(&mut previous, &mut staged)
                    .map_err(|e| format!("Could not preserve old Drive stage tail: {e}"))?;
                if copied != old_len {
                    return Err("Previous Drive chunk changed during boundary stage".to_string());
                }
            }
            write(&staged, bytes)
                .map_err(|e| format!("Could not fully stage resized Drive boundary: {e}"))?;
            staged
                .sync_all()
                .map_err(|e| format!("Could not sync resized Drive boundary: {e}"))?;
            drop(staged);
            std::fs::rename(&temp, path)
                .map_err(|e| format!("Could not install resized Drive boundary: {e}"))?;
            // The new stage is already published. Reporting failure here could
            // roll back SQLite references while readers see the new chunk.
            if let Err(error) = sync_directory(parent) {
                log::warn!(
                    "Drive resized boundary published but directory durability sync failed: {error}"
                );
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn stage_resized_boundary(path: &Path, bytes: &[u8]) -> Result<(), String> {
        stage_resized_boundary_with(path, bytes, |file, bytes| {
            positioned_write_all(file, bytes, 0)
        })
    }

    fn allocated_file_bytes(metadata: &std::fs::Metadata) -> u64 {
        #[cfg(unix)]
        {
            metadata.blocks().saturating_mul(512)
        }
        #[cfg(not(unix))]
        {
            metadata.len()
        }
    }

    fn ensure_private_staging_directory(path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Could not create private Drive staging directory: {e}"))?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("Could not inspect private Drive staging directory: {e}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Drive staging path is not a private directory".to_string());
        }
        #[cfg(unix)]
        {
            // Do not chmod a pathname after checking it: the path could have
            // been replaced with a symlink between stat and chmod. Open a
            // verified directory descriptor and change that inode only.
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(path)
                .map_err(|e| format!("Could not open private Drive staging directory: {e}"))?;
            directory
                .set_permissions(std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("Could not secure Drive staging directory: {e}"))?;
        }
        Ok(())
    }

    // A newly allocated pending ID owns its staging directory only after an
    // exclusive create. Never reuse or delete a preexisting directory when
    // a metadata insert, marker creation, or permissions check fails.
    fn create_owned_staging_directory(root: &Path, id: &str) -> Result<PathBuf, String> {
        let directory = root.join(id);
        std::fs::create_dir(&directory)
            .map_err(|error| format!("Could not allocate Drive staging directory: {error}"))?;
        if let Err(error) = ensure_private_staging_directory(&directory) {
            let _ = std::fs::remove_dir(&directory);
            return Err(error);
        }
        Ok(directory)
    }

    // Pending rows survive restarts and come from SQLite, not a trusted path
    // argument. Validate the exact stage and every app-owned ancestor before
    // opening, creating, resizing, or removing a pending plaintext chunk.
    fn verified_pending_staging(
        staging_root: &Path,
        pending: &DrivePendingRecord,
    ) -> Result<(), String> {
        let cache_root = staging_root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "Drive staging root is invalid".to_string())?;
        let owned = crate::drive_stream::owned_finalized_staging_dir(cache_root, pending)
            .map_err(str::to_string)?;
        if owned.parent() != Some(staging_root) {
            return Err("Drive staging directory does not match mount root".into());
        }
        Ok(())
    }

    // Provider read requests are u64 (and may cover enormous ranges). Serve
    // them in bounded partial chunks instead of truncating to u32 or allocating
    // the requested length. Linux FUSE has its own kernel-bounded u32 read
    // requests and continues through read_resolved_node without this clamp.
    fn bounded_provider_read_length(requested: u64) -> u64 {
        requested.min(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
    }

    // The isolated QA profile uses a whole plaintext .pending file rather than
    // chunk files. Verify the persisted stage before opening and never follow
    // a substituted leaf symlink (including when the path itself is owned).
    fn open_verified_qa_pending_file(
        staging_root: &Path,
        pending: &DrivePendingRecord,
        writable: bool,
    ) -> Result<File, String> {
        verified_pending_staging(staging_root, pending)?;
        let path = Path::new(&pending.staging_path);
        let info = std::fs::symlink_metadata(path)
            .map_err(|error| format!("Could not inspect QA Drive staging file: {error}"))?;
        if !info.file_type().is_file() || info.file_type().is_symlink() {
            return Err("Invalid QA Drive staging file identity".into());
        }
        let mut options = OpenOptions::new();
        options.read(true).write(writable);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .map_err(|error| format!("Could not open private QA Drive staging file: {error}"))?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("QA Drive staging descriptor is not a regular file".into());
        }
        Ok(file)
    }

    // A QA upload must not be announced as queued until the exact private
    // file length has been saved. Silent stat or SQLite failures otherwise
    // leave an incorrect upload size while reporting a successful close.
    fn record_verified_qa_pending_size(
        staging_root: &Path,
        db: &DbConnection,
        pending: &DrivePendingRecord,
    ) -> Result<DrivePendingRecord, String> {
        let file = open_verified_qa_pending_file(staging_root, pending, false)?;
        let size = file
            .metadata()
            .map_err(|e| format!("Could not inspect QA Drive file size: {e}"))?
            .len();
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        // Size and closed_at must become visible together: a failed stage
        // validation, deleted row, or SQL error must not turn an unsuccessful
        // close into an upload-eligible recovery record.
        conn.execute("SAVEPOINT qa_verified_close")
            .map_err(|e| e.to_string())?;
        let result = (|| {
            update_drive_pending_size(&conn, &pending.id, size)?;
            mark_drive_pending_closed(&conn, &pending.id)?;
            let updated = drive_pending_by_id(&conn, &pending.id)?
                .ok_or_else(|| "Drive pending file disappeared before QA close".to_string())?;
            if updated.closed_at.is_none() || updated.size != size {
                return Err("QA Drive pending close metadata did not persist".to_string());
            }
            Ok::<_, String>(updated)
        })();
        match result {
            Ok(updated) => {
                conn.execute("RELEASE SAVEPOINT qa_verified_close")
                    .map_err(|e| e.to_string())?;
                Ok(updated)
            }
            Err(error) => {
                let _ = conn.execute("ROLLBACK TO SAVEPOINT qa_verified_close");
                let _ = conn.execute("RELEASE SAVEPOINT qa_verified_close");
                Err(error)
            }
        }
    }

    // A failed replacement owns only its exclusively created directory and
    // optional marker. Never recursively remove a preexisting/foreign entry.
    fn cleanup_failed_replacement_staging(directory: &Path, marker: Option<&Path>) {
        if let Some(marker) = marker {
            let _ = std::fs::remove_file(marker);
        }
        let _ = std::fs::remove_dir(directory);
    }

    #[cfg(test)]
    mod staging_creation_safety_tests {
        use super::{
            bounded_provider_read_length, cleanup_failed_replacement_staging,
            create_owned_staging_directory, open_verified_qa_pending_file,
            record_verified_qa_pending_size, sync_local_staging_for_pending,
            verified_pending_staging, DrivePendingRecord,
        };
        use std::fs;

        #[test]
        fn qa_close_records_real_size_and_never_queues_unverified_staging() {
            let cache = std::env::temp_dir().join(format!(
                "terarelay-qa-close-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            let staging = cache.join("drive").join("staging");
            fs::create_dir_all(&staging).unwrap();
            let owned = create_owned_staging_directory(&staging, "pending-size").unwrap();
            let stage_file = owned.join(".pending");
            fs::write(&stage_file, b"accurate length").unwrap();
            let pending = DrivePendingRecord {
                id: "pending-size".into(),
                directory_id: None,
                display_name: "file.bin".into(),
                staging_path: stage_file.to_string_lossy().into_owned(),
                size: 0,
                format_version: 2,
                backing_channel_id: 77,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            };
            let conn = sqlite::open(":memory:").unwrap();
            crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
            let mut insert = conn.prepare(
                "INSERT INTO drive_pending_files (id,display_name,staging_path,size,format_version,
                    backing_channel_id,encryption_version,created_at,updated_at)
                 VALUES (?,'file.bin',?,0,2,77,0,1,1)",
            ).unwrap();
            insert.bind((1, pending.id.as_str())).unwrap();
            insert.bind((2, pending.staging_path.as_str())).unwrap();
            insert.next().unwrap();
            drop(insert);
            let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
            let updated = record_verified_qa_pending_size(&staging, &db, &pending).unwrap();
            assert_eq!(updated.size, b"accurate length".len() as u64);
            assert!(
                updated.closed_at.is_some(),
                "verified size must be closed atomically"
            );
            assert_eq!(
                crate::commands::drive_metadata::drive_pending_by_id(
                    &db.lock().unwrap(),
                    &pending.id
                )
                .unwrap()
                .unwrap()
                .size,
                updated.size
            );
            fs::remove_file(&stage_file).unwrap();
            assert!(record_verified_qa_pending_size(&staging, &db, &pending).is_err());
            let outside = cache.join("sentinel");
            fs::write(&outside, b"external unchanged").unwrap();
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&outside, &stage_file).unwrap();
                assert!(record_verified_qa_pending_size(&staging, &db, &pending).is_err());
            }
            assert_eq!(fs::read(&outside).unwrap(), b"external unchanged");
            let source = include_str!("drive_mount.rs");
            let shared_close = source
                .rsplit_once("fn close_write(&self,")
                .unwrap()
                .1
                .split_once("fn abort_write(&self,")
                .unwrap()
                .0;
            let fuse_close = source
                .rsplit_once("fn release(\n")
                .unwrap()
                .1
                .split_once("fn setattr(\n")
                .unwrap()
                .0;
            for callback in [shared_close, fuse_close] {
                let replacement = callback.find("if pending.base_entry_id.is_some()").unwrap();
                assert!(
                    !callback[..replacement].contains("mark_drive_pending_closed("),
                    "new pending files must not be closed before staging verification"
                );
                let validate = callback.find("record_verified_qa_pending_size(").unwrap();
                let enqueue = callback.find("self.emit_upload_once(&pending)").unwrap();
                assert!(
                    validate < enqueue,
                    "size must be committed before announcing an upload"
                );
            }
            fs::remove_dir_all(cache).unwrap();
        }

        #[test]
        fn failed_qa_close_rolls_back_size_and_never_marks_upload_ready() {
            let cache = std::env::temp_dir().join(format!(
                "terarelay-qa-close-atomic-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            let staging = cache.join("drive").join("staging");
            fs::create_dir_all(&staging).unwrap();
            let owned = create_owned_staging_directory(&staging, "pending-atomic").unwrap();
            let stage_file = owned.join(".pending");
            fs::write(&stage_file, b"original payload").unwrap();
            let pending = DrivePendingRecord {
                id: "pending-atomic".into(),
                directory_id: None,
                display_name: "data.bin".into(),
                staging_path: stage_file.to_string_lossy().into_owned(),
                size: 1,
                format_version: 2,
                backing_channel_id: 77,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            };
            let conn = sqlite::open(":memory:").unwrap();
            crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
            let mut insert = conn
                .prepare(
                    "INSERT INTO drive_pending_files
                    (id,display_name,staging_path,size,format_version,backing_channel_id,
                     encryption_version,created_at,updated_at)
                    VALUES (?,'data.bin',?,1,2,77,0,1,1)",
                )
                .unwrap();
            insert.bind((1, pending.id.as_str())).unwrap();
            insert.bind((2, pending.staging_path.as_str())).unwrap();
            insert.next().unwrap();
            drop(insert);
            let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
            let status = || {
                crate::commands::drive_metadata::drive_pending_by_id(
                    &db.lock().unwrap(),
                    &pending.id,
                )
                .unwrap()
                .unwrap()
            };
            assert_eq!(status().size, 1);
            assert!(status().closed_at.is_none());

            // Force the second SQL update to fail, after size was written in
            // the savepoint. Both fields must roll back to the original values.
            db.lock()
                .unwrap()
                .execute(
                    "CREATE TRIGGER reject_qa_close BEFORE UPDATE OF closed_at
                     ON drive_pending_files BEGIN SELECT RAISE(ABORT, 'reject close'); END",
                )
                .unwrap();
            assert!(record_verified_qa_pending_size(&staging, &db, &pending).is_err());
            assert_eq!(status().size, 1);
            assert!(status().closed_at.is_none());
            db.lock()
                .unwrap()
                .execute("DROP TRIGGER reject_qa_close")
                .unwrap();

            fs::remove_file(&stage_file).unwrap();
            assert!(record_verified_qa_pending_size(&staging, &db, &pending).is_err());
            assert_eq!(status().size, 1);
            assert!(status().closed_at.is_none());

            let sentinel = cache.join("foreign.dat");
            fs::write(&sentinel, b"do not read or modify").unwrap();
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&sentinel, &stage_file).unwrap();
                assert!(record_verified_qa_pending_size(&staging, &db, &pending).is_err());
                assert_eq!(status().size, 1);
                assert!(status().closed_at.is_none());
                fs::remove_file(&stage_file).unwrap();
            }
            assert_eq!(fs::read(&sentinel).unwrap(), b"do not read or modify");

            fs::write(&stage_file, b"complete payload").unwrap();
            let closed = record_verified_qa_pending_size(&staging, &db, &pending).unwrap();
            assert_eq!(closed.size, b"complete payload".len() as u64);
            assert!(closed.closed_at.is_some());
            assert_eq!(status().size, closed.size);
            assert_eq!(status().closed_at, closed.closed_at);
            fs::remove_dir_all(cache).unwrap();
        }

        #[test]
        fn shared_qa_drive_io_rejects_symlinked_or_unowned_pending_files() {
            let cache = std::env::temp_dir().join(format!(
                "terarelay-qa-stage-owner-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            let staging = cache.join("drive").join("staging");
            fs::create_dir_all(&staging).unwrap();
            let owned = create_owned_staging_directory(&staging, "pending-qa").unwrap();
            let pending_path = owned.join(".pending");
            fs::write(&pending_path, b"owned bytes").unwrap();
            let mut pending = DrivePendingRecord {
                id: "pending-qa".into(),
                directory_id: None,
                display_name: "data.bin".into(),
                staging_path: pending_path.to_string_lossy().into_owned(),
                size: 11,
                format_version: 2,
                backing_channel_id: 77,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            };
            assert!(open_verified_qa_pending_file(&staging, &pending, false).is_ok());
            let writable = open_verified_qa_pending_file(&staging, &pending, true).unwrap();
            writable.set_len(11).unwrap();
            assert_eq!(fs::read(&pending_path).unwrap(), b"owned bytes");
            let outside = cache.join("unrelated.txt");
            fs::write(&outside, b"do not modify").unwrap();
            #[cfg(unix)]
            {
                fs::remove_file(&pending_path).unwrap();
                std::os::unix::fs::symlink(&outside, &pending_path).unwrap();
                assert!(open_verified_qa_pending_file(&staging, &pending, false).is_err());
                assert!(open_verified_qa_pending_file(&staging, &pending, true).is_err());
            }
            pending.staging_path = outside.to_string_lossy().into_owned();
            assert!(open_verified_qa_pending_file(&staging, &pending, true).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"do not modify");
            let source = include_str!("drive_mount.rs");
            let resize = source
                .rsplit_once("fn resize_pending(\n")
                .unwrap()
                .1
                .split_once("fn flush_completed_chunks(\n")
                .unwrap()
                .0;
            let reader = source
                .rsplit_once("fn read_resolved_node(\n")
                .unwrap()
                .1
                .split_once("fn core_flush_pending(")
                .unwrap()
                .0;
            let writer = source
                .rsplit_once("fn write(&self, id: &DriveNodeId,")
                .unwrap()
                .1
                .split_once("fn resize(&self, id: &DriveNodeId,")
                .unwrap()
                .0;
            for callback in [resize, reader, writer] {
                assert!(callback.contains("open_verified_qa_pending_file("));
            }
            assert!(writer.contains(".metadata()"));
            fs::remove_dir_all(cache).unwrap();
        }

        #[test]
        fn provider_large_read_requests_are_bounded_without_u32_wraparound() {
            let cap = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            assert!(cap < u32::MAX as u64);
            assert_eq!(bounded_provider_read_length(0), 0);
            assert_eq!(bounded_provider_read_length(1), 1);
            assert_eq!(bounded_provider_read_length(cap - 1), cap - 1);
            assert_eq!(bounded_provider_read_length(cap), cap);
            assert_eq!(bounded_provider_read_length(cap + 1), cap);
            assert_eq!(bounded_provider_read_length(u32::MAX as u64), cap);
            assert_eq!(bounded_provider_read_length(u64::MAX), cap);
            let source = include_str!("drive_mount.rs");
            let backend_read = source
                .split_once("fn read(\n            &self,\n            id: &DriveNodeId,\n")
                .unwrap()
                .1
                .split_once("fn create_file(")
                .unwrap()
                .0;
            assert!(backend_read.contains("bounded_provider_read_length(length)"));
            let resolved = source
                .rsplit_once("fn read_resolved_node(")
                .unwrap()
                .1
                .split_once("fn core_flush_pending(")
                .unwrap()
                .0;
            assert_eq!(
                resolved.matches("u32::try_from(take)").count(),
                2,
                "remote and staged reads must reject narrowing, never wrap"
            );
            assert!(resolved.contains("vec![0u8; take as usize]"));
            assert!(resolved.contains("self.read_pending_range(pending, offset, take)"));
        }

        #[test]
        fn native_pending_fsync_rejects_redirected_stage_and_symlinked_children() {
            let cache = std::env::temp_dir().join(format!(
                "terarelay-fsync-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            let root = cache.join("drive").join("staging");
            fs::create_dir_all(&root).unwrap();
            let owned = create_owned_staging_directory(&root, "sync-pending").unwrap();
            let pending_file = owned.join(".pending");
            fs::write(&pending_file, b"unsent local bytes").unwrap();
            let mut pending = DrivePendingRecord {
                id: "sync-pending".into(),
                directory_id: None,
                display_name: "local.bin".into(),
                staging_path: pending_file.to_string_lossy().into_owned(),
                size: 18,
                format_version: 2,
                backing_channel_id: 0,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            };
            assert!(sync_local_staging_for_pending(&root, &pending).is_ok());
            let outside = cache.join("unrelated.dat");
            fs::write(&outside, b"external data must survive").unwrap();
            #[cfg(unix)]
            {
                let linked = owned.join("0000000000000000.bin");
                std::os::unix::fs::symlink(&outside, &linked).unwrap();
                assert!(sync_local_staging_for_pending(&root, &pending).is_err());
                fs::remove_file(&linked).unwrap();

                // Even a perfectly named foreign directory cannot become a
                // valid fsync target through a substituted staging ancestor.
                let actual = cache.join("saved-drive");
                fs::rename(cache.join("drive"), &actual).unwrap();
                let foreign = cache.join("external-drive");
                let fake_stage = foreign.join("staging").join("sync-pending");
                fs::create_dir_all(&fake_stage).unwrap();
                fs::write(fake_stage.join(".pending"), b"external data").unwrap();
                std::os::unix::fs::symlink(&foreign, cache.join("drive")).unwrap();
                assert!(sync_local_staging_for_pending(&root, &pending).is_err());
                assert_eq!(
                    fs::read(fake_stage.join(".pending")).unwrap(),
                    b"external data"
                );
                fs::remove_file(cache.join("drive")).unwrap();
                fs::rename(&actual, cache.join("drive")).unwrap();
                assert!(sync_local_staging_for_pending(&root, &pending).is_ok());
            }
            pending.staging_path = outside.to_string_lossy().into_owned();
            assert!(sync_local_staging_for_pending(&root, &pending).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"external data must survive");
            let source = include_str!("drive_mount.rs");
            let helper = source
                .rsplit_once("fn sync_local_staging_for_pending(")
                .unwrap()
                .1
                .split_once("fn allocated_stage_bytes(")
                .unwrap()
                .0;
            assert!(helper.contains("verified_pending_staging(staging_root, pending)?"));
            #[cfg(unix)]
            {
                assert!(helper.contains("crate::cache_directory::open_directory(dir, false)"));
                assert!(helper.contains("crate::cache_directory::open_child("));
                assert!(helper.contains("directory"));
                assert!(helper.contains(".sync_all()"));
            }
            let backend = source
                .rsplit_once("fn core_flush_pending(")
                .unwrap()
                .1
                .split_once("impl DriveFsBackend for TeraRelayFs")
                .unwrap()
                .0;
            assert!(backend.contains("sync_local_staging_for_pending(&self.staging_root, pending)"));
            fs::remove_dir_all(cache).unwrap();
        }

        #[test]
        fn pending_flush_and_qa_fuse_io_validate_owned_stage_before_touching_files() {
            let source = include_str!("drive_mount.rs");
            let backend_flush = source
                .rsplit_once("fn core_flush_pending(")
                .unwrap()
                .1
                .split_once("impl DriveFsBackend for TeraRelayFs")
                .unwrap()
                .0;
            assert!(backend_flush.contains("verified_pending_staging(&self.staging_root, pending)"));
            let write = source
                .rsplit_once("fn write(\n")
                .unwrap()
                .1
                .split_once("fn flush(\n")
                .unwrap()
                .0;
            let guard = write
                .find("verified_pending_staging(&self.backend.staging_root, &pending)")
                .unwrap();
            assert!(guard < write.find(".open(&pending.staging_path)").unwrap());
            assert!(write.contains(".custom_flags(libc::O_NOFOLLOW)"));
            for (start, end, operation) in [
                (
                    "fn flush(\n",
                    "fn fsync(\n",
                    "sync_local_staging_for_pending(&self.backend.staging_root, &pending)",
                ),
                (
                    "fn fsync(\n",
                    "fn release(\n",
                    "sync_local_staging_for_pending(&self.backend.staging_root, &pending)",
                ),
                (
                    "fn release(\n",
                    "fn setattr(\n",
                    "mark_drive_pending_closed(&conn, &pending.id)",
                ),
            ] {
                let callback = source
                    .rsplit_once(start)
                    .unwrap()
                    .1
                    .split_once(end)
                    .unwrap()
                    .0;
                let verified = callback
                    .find("verified_pending_staging(&self.backend.staging_root, &pending)")
                    .unwrap();
                let io = callback.find(operation).unwrap();
                assert!(verified < io, "{start} must guard staging before I/O");
            }
        }

        #[test]
        fn active_pending_chunk_io_rejects_forged_and_symlinked_stage_paths() {
            let cache = std::env::temp_dir().join(format!(
                "terarelay-active-stage-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            let staging = cache.join("drive").join("staging");
            fs::create_dir_all(&staging).unwrap();
            let owned = create_owned_staging_directory(&staging, "pending-valid").unwrap();
            let mut pending = DrivePendingRecord {
                id: "pending-valid".into(),
                directory_id: None,
                display_name: "important.bin".into(),
                staging_path: owned.join(".pending").to_string_lossy().into_owned(),
                size: 0,
                format_version: 2,
                backing_channel_id: 77,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            };
            assert!(verified_pending_staging(&staging, &pending).is_ok());
            let outside = cache.join("private-user-files");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("sentinel"), b"untouched").unwrap();
            pending.staging_path = outside.join(".pending").to_string_lossy().into_owned();
            assert!(verified_pending_staging(&staging, &pending).is_err());
            pending.id = "../private-user-files".into();
            assert!(verified_pending_staging(&staging, &pending).is_err());
            #[cfg(unix)]
            {
                let link = staging.join("linked");
                std::os::unix::fs::symlink(&outside, &link).unwrap();
                pending.id = "linked".into();
                pending.staging_path = link.join(".pending").to_string_lossy().into_owned();
                assert!(verified_pending_staging(&staging, &pending).is_err());
            }
            assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");

            let source = include_str!("drive_mount.rs");
            for (start, end) in [
                (
                    "fn materialize_pending_chunk(\n",
                    "fn read_pending_range(\n",
                ),
                ("fn read_pending_range(\n", "fn write_pending_bytes(\n"),
                ("fn write_pending_bytes(\n", "fn resize_pending(\n"),
                ("fn resize_pending(\n", "fn flush_completed_chunks(\n"),
                ("fn flush_completed_chunks(\n", "fn core_create_pending(\n"),
            ] {
                let segment = source
                    .rsplit_once(start)
                    .unwrap()
                    .1
                    .split_once(end)
                    .unwrap()
                    .0;
                assert!(segment.contains("verified_pending_staging(&self.staging_root, pending)?"));
            }
            fs::remove_dir_all(cache).unwrap();
        }

        #[test]
        fn pending_cancellation_must_validate_staging_ownership_before_deletion() {
            let callback = include_str!("drive_mount.rs")
                .rsplit_once("fn cancel_pending_sync_inner(")
                .expect("native cancellation callback must exist")
                .1
                .split_once("fn pending_has_content_changes(")
                .expect("native cancellation callback must have an end")
                .0;
            assert!(
                callback.contains("self.cleanup_removed_pending_stage(pending, path.as_deref())")
            );
            let owner = include_str!("drive_mount.rs")
                .rsplit_once("fn cleanup_removed_pending_stage(")
                .unwrap()
                .1
                .split_once("fn cancel_pending_sync(")
                .unwrap()
                .0;
            assert!(owner.contains("owned_cancelled_staging_dir("));
            assert!(owner.contains("deleted_path"));
            assert!(!callback.contains("remove_dir_all("));
        }

        #[test]
        fn failed_truncate_on_open_never_deletes_unverified_staging() {
            let source = include_str!("drive_mount.rs");
            let open = source
                .rsplit_once("fn open(&mut self,")
                .expect("Linux FUSE open callback must exist")
                .1
                .split_once("fn read(\n")
                .expect("Linux FUSE read callback must follow open")
                .0;
            assert!(open.contains("delete_drive_pending(&conn, &pending.id)"));
            assert!(open
                .contains("self.cleanup_removed_pending_stage(&pending, deleted_path.as_deref())"));
            assert!(!open.contains("remove_dir_all("));
        }

        #[test]
        fn failed_replacement_only_cleans_its_marker_and_empty_owned_directory() {
            let root = std::env::temp_dir().join(format!(
                "terarelay-replacement-stage-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            fs::create_dir(&root).unwrap();
            let stage = create_owned_staging_directory(&root, "replacement").unwrap();
            let marker = stage.join(".pending");
            fs::write(&marker, b"owned marker").unwrap();
            fs::write(stage.join("unexpected.bin"), b"keep this").unwrap();
            cleanup_failed_replacement_staging(&stage, Some(&marker));
            assert!(!marker.exists());
            assert_eq!(
                fs::read(stage.join("unexpected.bin")).unwrap(),
                b"keep this"
            );
            assert!(
                stage.is_dir(),
                "never recursively remove foreign stage content"
            );

            let stage = create_owned_staging_directory(&root, "empty-replacement").unwrap();
            let marker = stage.join(".pending");
            fs::write(&marker, b"owned marker").unwrap();
            cleanup_failed_replacement_staging(&stage, Some(&marker));
            assert!(
                !stage.exists(),
                "empty owned replacement stage can be removed"
            );

            let implementation = include_str!("drive_mount.rs")
                .rsplit_once("fn create_replacement_pending(")
                .unwrap()
                .1
                .split_once("fn cleanup_removed_pending_stage(")
                .unwrap()
                .0;
            assert!(implementation.contains("create_owned_staging_directory("));
            assert!(!implementation.contains("remove_dir_all("));
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn staging_creation_never_takes_ownership_of_existing_directories() {
            let root = std::env::temp_dir().join(format!(
                "terarelay-staging-create-{}",
                crate::commands::drive_metadata::new_drive_id()
            ));
            fs::create_dir(&root).unwrap();
            let existing = root.join("collision");
            fs::create_dir(&existing).unwrap();
            fs::write(existing.join("important.bin"), b"preserve").unwrap();
            assert!(create_owned_staging_directory(&root, "collision").is_err());
            assert_eq!(
                fs::read(existing.join("important.bin")).unwrap(),
                b"preserve"
            );

            let owned = create_owned_staging_directory(&root, "new-stage").unwrap();
            assert_eq!(owned, root.join("new-stage"));
            assert!(owned.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&owned).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            // Failed pre-metadata creation may discard only an empty owned
            // directory. Other entries must never be recursively removed.
            fs::write(owned.join("unexpected"), b"preserve").unwrap();
            assert!(fs::remove_dir(&owned).is_err());
            assert_eq!(fs::read(owned.join("unexpected")).unwrap(), b"preserve");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    fn sync_local_staging_for_pending(
        staging_root: &Path,
        pending: &DrivePendingRecord,
    ) -> Result<(), String> {
        // Validate within this function, not only in its callers: the
        // committed staging path is persisted in SQLite and can be corrupted.
        verified_pending_staging(staging_root, pending)?;
        // fsync must make the still-local, not-yet-uploaded chunk data durable.
        // Uploading completed chunks alone is insufficient: a crash after
        // acknowledging fsync must not lose the unfinished final chunk.
        let dir = Path::new(&pending.staging_path)
            .parent()
            .ok_or_else(|| "Drive staging location has no parent directory".to_string())?;
        #[cfg(unix)]
        let directory = crate::cache_directory::open_directory(dir, false)
            .map_err(|e| format!("Cannot open owned Drive staging directory: {e}"))?;
        for entry in std::fs::read_dir(dir)
            .map_err(|e| format!("Cannot inspect pending Drive staging files: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Cannot inspect Drive staging entry: {e}"))?;
            #[cfg(unix)]
            let file = crate::cache_directory::open_child(
                &directory,
                &entry.file_name(),
                libc::O_RDONLY | libc::O_NONBLOCK,
            )
            .map_err(|e| format!("Cannot open private Drive staging file: {e}"))?;
            #[cfg(not(unix))]
            let file = open_private_read(&entry.path())?;
            if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                return Err("Drive staging contains an unsafe non-file entry".to_string());
            }
            file.sync_all()
                .map_err(|e| format!("Could not sync local Drive staged bytes: {e}"))?;
        }
        #[cfg(unix)]
        directory
            .sync_all()
            .map_err(|e| format!("Could not sync Drive staging directory: {e}"))?;
        Ok(())
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
                if meta.file_type().is_symlink() {
                    return Err("Drive staging budget contains an unsafe symlink".to_string());
                }
                if meta.is_dir() {
                    pending.push(entry.path());
                } else if meta.is_file() {
                    total = total.saturating_add(allocated_file_bytes(&meta));
                } else {
                    return Err(
                        "Drive staging budget contains an unsafe non-file entry".to_string()
                    );
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
        // Telegram does not expose a measurable total/free cloud quota. On
        // Linux FUSE, GIO treats free == available == 0 as unknown and omits the
        // capacity attributes; Nemo then skips its whole-copy space precheck.
        // Local write staging remains separately bounded by STAGING_BUDGET_BYTES.
        const BLOCK_SIZE: u64 = 4096;
        #[cfg(target_os = "linux")]
        let blocks = 0;
        // Native providers on other platforms keep their existing capacity ABI.
        #[cfg(not(target_os = "linux"))]
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

    fn symlink_node(link: &crate::drive_posix::DriveSymlinkRecord) -> Node {
        Node::Remote(DriveFileRecord {
            entry_id: link.id.clone(),
            file_id: format!("symlink:{}", link.id),
            inode_id: None,
            directory_id: link.directory_id.clone(),
            display_name: link.display_name.clone(),
            total_size: link.target.len() as u64,
            mime_type: None,
            first_message_id: 0,
            backing_channel_id: 0,
            logical_channel_id: String::new(),
            created_at: link.created_at / 1000,
            updated_at: link.updated_at,
            encryption_version: 0,
            plaintext_size: None,
            crypto_id: None,
            content_fingerprint: None,
            storage_version: 3,
        })
    }

    #[cfg(target_os = "linux")]
    fn capture_symlink_target(
        conn: &sqlite::Connection,
        node: &Node,
    ) -> Result<Option<String>, String> {
        match node {
            Node::Remote(file) if file.storage_version == 3 => Ok(Some(
                crate::drive_posix::symlink_by_id(conn, &file.entry_id)?
                    .ok_or_else(|| "Retained Drive symlink is missing".to_string())?
                    .target,
            )),
            _ => Ok(None),
        }
    }

    #[cfg(target_os = "linux")]
    fn lookup_child_identity_locked(
        conn: &sqlite::Connection,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Option<(DriveNodeId, bool)>, String> {
        crate::drive_lookup::child_with_symlink(conn, parent_id, name)
            .map_err(|error| error.to_string())
    }

    #[cfg(target_os = "linux")]
    fn lookup_child_node_locked(
        conn: &sqlite::Connection,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Node>, String> {
        crate::drive_lookup::child_entry(conn, parent_id, name)
            .map(|entry| {
                entry.map(|entry| match entry {
                    crate::drive_lookup::DriveChildEntry::Symlink(link) => symlink_node(&link),
                    crate::drive_lookup::DriveChildEntry::Directory(directory) => {
                        Node::Directory(directory)
                    }
                    crate::drive_lookup::DriveChildEntry::Pending(pending) => {
                        Node::Pending(pending)
                    }
                    crate::drive_lookup::DriveChildEntry::File(file) => Node::Remote(file),
                })
            })
            .map_err(|error| error.to_string())
    }

    impl Node {
        #[cfg(target_os = "linux")]
        fn inode(&self) -> u64 {
            match self {
                Node::Root => ROOT_INODE,
                Node::Directory(dir) => inode_for("dir", &dir.id),
                Node::Remote(file) => inode_for("file", &file.entry_id),
                Node::Pending(file) => {
                    inode_for("file", file.base_entry_id.as_deref().unwrap_or(&file.id))
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn inode_for(kind: &str, id: &str) -> u64 {
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

    #[cfg(target_os = "linux")]
    struct IndexedDirectoryEntry {
        ino: u64,
        kind: FileType,
        name: String,
        identity: DriveNodeId,
    }

    #[cfg(target_os = "linux")]
    type IndexedDirectoryListing = (Option<String>, Vec<IndexedDirectoryEntry>);

    #[cfg(target_os = "linux")]
    #[derive(Clone)]
    struct IndexedInode {
        identity: DriveNodeId,
        kind: FileType,
    }

    /// Verify the entire inode batch before modifying the hot FUSE index.
    /// SHA-256-derived 63-bit inode numbers are stable but not mathematically
    /// collision-free. An overwritten inode can direct an open/read to a
    /// completely different file, so never overwrite a different identity.
    ///
    /// This also intentionally refuses future shared-inode aliases until the
    /// cache is redesigned to represent all names for one POSIX inode.
    #[cfg(target_os = "linux")]
    fn checked_inode_index_batch(
        index: &mut HashMap<u64, IndexedInode>,
        entries: impl IntoIterator<Item = (u64, IndexedInode)>,
    ) -> Result<(), String> {
        let mut additions: HashMap<u64, IndexedInode> = HashMap::new();
        for (ino, node) in entries {
            let previous = additions.get(&ino).or_else(|| index.get(&ino));
            if let Some(previous) = previous {
                if previous.identity != node.identity || previous.kind != node.kind {
                    return Err(format!(
                        "Drive FUSE inode identity collision at {ino}; refusing to alias different nodes"
                    ));
                }
            } else {
                additions.insert(ino, node);
            }
        }
        index.extend(additions);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn rename_identity_inode(
        index: &HashMap<u64, IndexedInode>,
        identity: &DriveNodeId,
    ) -> Option<u64> {
        match identity {
            DriveNodeId::Root => Some(ROOT_INODE),
            DriveNodeId::Directory(id) => Some(inode_for("dir", id)),
            DriveNodeId::File(id) => Some(inode_for("file", id)),
            DriveNodeId::Pending(_) => index
                .iter()
                .find_map(|(ino, entry)| (entry.identity == *identity).then_some(*ino)),
        }
    }

    #[cfg(target_os = "linux")]
    fn rename_source_fields(
        index: &HashMap<u64, IndexedInode>,
        identity: &DriveNodeId,
        is_symlink: bool,
    ) -> Option<(
        crate::commands::drive_metadata::NativeRenameKind,
        String,
        u64,
        FileType,
    )> {
        use crate::commands::drive_metadata::NativeRenameKind;

        let ino = rename_identity_inode(index, identity)?;
        if index
            .get(&ino)
            .is_some_and(|entry| entry.identity != *identity)
        {
            return None;
        }
        match identity {
            DriveNodeId::Root => None,
            DriveNodeId::Directory(id) => Some((
                NativeRenameKind::Directory,
                id.clone(),
                ino,
                FileType::Directory,
            )),
            DriveNodeId::File(id) => Some((
                if is_symlink {
                    NativeRenameKind::Symlink
                } else {
                    NativeRenameKind::File
                },
                id.clone(),
                ino,
                if is_symlink {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                },
            )),
            DriveNodeId::Pending(id) => Some((
                NativeRenameKind::Pending,
                id.clone(),
                ino,
                FileType::RegularFile,
            )),
        }
    }

    #[cfg(target_os = "linux")]
    fn indexed_directory_id<T>(
        index: &HashMap<u64, IndexedInode>,
        lifetimes: &InodeLifetimes<T>,
        ino: u64,
    ) -> Option<Option<String>> {
        if ino == ROOT_INODE {
            return Some(None);
        }
        if !lifetimes.referenced(ino) || lifetimes.retired(ino).is_some() {
            return None;
        }
        let entry = index.get(&ino)?;
        if entry.kind != FileType::Directory {
            return None;
        }
        match &entry.identity {
            DriveNodeId::Root => Some(None),
            DriveNodeId::Directory(id) => Some(Some(id.clone())),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    fn indexed_regular_file_identity(
        index: &HashMap<u64, IndexedInode>,
        ino: u64,
    ) -> Option<DriveNodeId> {
        let entry = index.get(&ino)?;
        if entry.kind != FileType::RegularFile {
            return None;
        }
        match &entry.identity {
            DriveNodeId::File(_) => Some(entry.identity.clone()),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    fn indexed_node_kind(node: &Node) -> FileType {
        match node {
            Node::Root | Node::Directory(_) => FileType::Directory,
            Node::Remote(file) if file.storage_version == 3 => FileType::Symlink,
            Node::Remote(_) | Node::Pending(_) => FileType::RegularFile,
        }
    }

    #[cfg(target_os = "linux")]
    fn capture_retained_if_referenced<T>(
        lifetimes: &InodeLifetimes<T>,
        ino: u64,
        capture: impl FnOnce() -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        if lifetimes.referenced(ino) {
            capture().map(Some)
        } else {
            Ok(None)
        }
    }

    #[cfg(target_os = "linux")]
    fn resolve_symlink_target(
        retained_target: Option<&str>,
        live_target: impl FnOnce() -> Result<Option<String>, String>,
    ) -> Result<Option<String>, String> {
        match retained_target {
            Some(target) => Ok(Some(target.to_string())),
            None => live_target(),
        }
    }

    #[cfg(target_os = "linux")]
    fn indexed_read_only_open_kind<T>(
        index: &HashMap<u64, IndexedInode>,
        lifetimes: &InodeLifetimes<T>,
        ino: u64,
    ) -> Option<FileType> {
        lifetimes
            .referenced(ino)
            .then(|| index.get(&ino).map(|entry| entry.kind))
            .flatten()
    }

    #[cfg(test)]
    static INDEXED_DIRECTORY_LISTING_QUERY_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[cfg(test)]
    fn reset_indexed_directory_listing_query_count() {
        INDEXED_DIRECTORY_LISTING_QUERY_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn indexed_directory_listing_query_count() -> usize {
        INDEXED_DIRECTORY_LISTING_QUERY_CALLS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(target_os = "linux")]
    fn indexed_directory_listing(
        conn: &sqlite::Connection,
        parent: Option<&str>,
    ) -> Result<Option<IndexedDirectoryListing>, String> {
        #[cfg(test)]
        INDEXED_DIRECTORY_LISTING_QUERY_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut statement = conn
            .prepare(
                "WITH current_directory AS (
                    SELECT NULL AS parent_id WHERE ?1 IS NULL
                    UNION ALL
                    SELECT d.parent_id FROM drive_directories d
                    WHERE d.id=?1 AND d.trashed_at IS NULL
                 ),
                 child_rows AS (
                    SELECT 0 AS kind,d.id AS id,d.name AS name,NULL AS base_entry_id

                    FROM drive_directories d
                    WHERE d.parent_id IS ?1 AND d.trashed_at IS NULL
                    UNION ALL
                    SELECT 1,p.id,p.display_name,p.base_entry_id
                    FROM drive_pending_files p
                    WHERE p.directory_id IS ?1
                    UNION ALL
                    SELECT 2,e.entry_id,e.display_name,NULL
                    FROM drive_file_entries e
                    JOIN logical_files f ON f.file_id=e.file_id
                    JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                    WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL AND f.status='complete'
                      AND NOT EXISTS(
                          SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id
                      )
                      AND NOT EXISTS (
                          SELECT 1 FROM drive_entries_v2 shadow
                          JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                          WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                      )
                    UNION ALL
                    SELECT 2,e.entry_id,e.display_name,NULL
                    FROM drive_entries_v2 e
                    JOIN drive_objects o ON o.object_id=e.object_id
                    WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL
                      AND NOT EXISTS(
                          SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id
                      )
                    UNION ALL
                    SELECT 3,s.id,s.display_name,NULL
                    FROM drive_symlinks s
                    WHERE s.directory_id IS ?1 AND s.trashed_at IS NULL
                 )
                 SELECT current_directory.parent_id AS parent_id,
                        child_rows.kind AS kind,
                        child_rows.id AS id,
                        child_rows.name AS name,
                        child_rows.base_entry_id AS base_entry_id
                 FROM current_directory
                 LEFT JOIN child_rows ON 1=1
                 ORDER BY child_rows.name COLLATE NOCASE ASC,
                          child_rows.kind ASC, child_rows.id ASC",
            )
            .map_err(|error: sqlite::Error| error.to_string())?;
        statement
            .bind((1, parent))
            .map_err(|error: sqlite::Error| error.to_string())?;

        let mut current_parent = None;
        let mut entries = Vec::new();
        while statement
            .next()
            .map_err(|error: sqlite::Error| error.to_string())?
            == sqlite::State::Row
        {
            if current_parent.is_none() {
                current_parent = Some(
                    statement
                        .read::<Option<String>, _>("parent_id")
                        .map_err(|error| error.to_string())?,
                );
            }
            let Some(kind) = statement
                .read::<Option<i64>, _>("kind")
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let id = statement
                .read::<String, _>("id")
                .map_err(|error| error.to_string())?;
            let name = statement
                .read::<String, _>("name")
                .map_err(|error| error.to_string())?;
            let base_entry_id = statement
                .read::<Option<String>, _>("base_entry_id")
                .map_err(|error| error.to_string())?;
            let (ino, file_type, identity) = match kind {
                0 => (
                    inode_for("dir", &id),
                    FileType::Directory,
                    DriveNodeId::Directory(id),
                ),
                1 => {
                    let visible_id = base_entry_id.unwrap_or(id);
                    (
                        inode_for("file", &visible_id),
                        FileType::RegularFile,
                        DriveNodeId::File(visible_id),
                    )
                }
                2 => (
                    inode_for("file", &id),
                    FileType::RegularFile,
                    DriveNodeId::File(id),
                ),
                3 => (
                    inode_for("file", &id),
                    FileType::Symlink,
                    DriveNodeId::File(id),
                ),
                _ => {
                    return Err("Drive directory listing returned an invalid node kind".to_string())
                }
            };
            entries.push(IndexedDirectoryEntry {
                ino,
                kind: file_type,
                name,
                identity,
            });
        }
        Ok(current_parent.map(|parent| (parent, entries)))
    }

    #[cfg(target_os = "linux")]
    struct IndexedDirectoryPlusEntry {
        ino: u64,
        kind: FileType,
        name: String,
        identity: DriveNodeId,
        attr: FileAttr,
    }

    #[cfg(target_os = "linux")]
    struct IndexedDirectoryPlusListing {
        node: Node,
        parent_inode: u64,
        current_attr: FileAttr,
        parent_attr: FileAttr,
        entries: Vec<IndexedDirectoryPlusEntry>,
    }

    #[cfg(test)]
    static INDEXED_DIRECTORY_PLUS_LISTING_QUERY_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[cfg(test)]
    fn reset_indexed_directory_plus_listing_query_count() {
        INDEXED_DIRECTORY_PLUS_LISTING_QUERY_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn indexed_directory_plus_listing_query_count() -> usize {
        INDEXED_DIRECTORY_PLUS_LISTING_QUERY_CALLS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(target_os = "linux")]
    fn indexed_directory_plus_listing(
        conn: &sqlite::Connection,
        current_id: Option<&str>,
    ) -> Result<Option<IndexedDirectoryPlusListing>, String> {
        #[cfg(test)]
        INDEXED_DIRECTORY_PLUS_LISTING_QUERY_CALLS
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut statement = conn
            .prepare(
                "WITH current_directory AS (
                    SELECT NULL AS id,NULL AS parent_id,NULL AS name,
                           NULL AS backing_logical_channel_id,NULL AS created_at,
                           NULL AS updated_at,493 AS mode,
                           2 + (SELECT COUNT(*) FROM drive_directories child
                                WHERE child.parent_id IS NULL AND child.trashed_at IS NULL) AS nlink
                    WHERE ?1 IS NULL
                    UNION ALL
                    SELECT d.id,d.parent_id,d.name,d.backing_logical_channel_id,
                           d.created_at,d.updated_at,COALESCE(pm.mode,493) AS mode,
                            2 + (SELECT COUNT(*) FROM drive_directories child
                                 WHERE child.parent_id=d.id AND child.trashed_at IS NULL) AS nlink
                    FROM drive_directories d
                    LEFT JOIN drive_permissions pm
                      ON pm.node_kind='directory' AND pm.node_id=d.id
                    WHERE d.id=?1 AND d.trashed_at IS NULL
                 ),
                 parent_directory AS (
                    SELECT p.id,p.parent_id,p.name,p.backing_logical_channel_id,
                           p.created_at,p.updated_at,COALESCE(pm.mode,493) AS mode,
                           2 + (SELECT COUNT(*) FROM drive_directories child
                                WHERE child.parent_id=p.id AND child.trashed_at IS NULL) AS nlink
                    FROM current_directory cd
                    JOIN drive_directories p
                      ON p.id=cd.parent_id AND p.trashed_at IS NULL
                    LEFT JOIN drive_permissions pm
                      ON pm.node_kind='directory' AND pm.node_id=p.id
                 ),
                 child_rows AS (
                    SELECT 0 AS kind,d.id AS id,d.name AS name,NULL AS base_entry_id,
                           0 AS size,d.created_at AS created_at,d.updated_at AS updated_at,
                           COALESCE(pm.mode,493) AS mode,
                           2 + (SELECT COUNT(*) FROM drive_directories child
                                WHERE child.parent_id=d.id AND child.trashed_at IS NULL) AS nlink
                    FROM drive_directories d
                    LEFT JOIN drive_permissions pm
                      ON pm.node_kind='directory' AND pm.node_id=d.id
                    WHERE d.parent_id IS ?1 AND d.trashed_at IS NULL
                    UNION ALL
                    SELECT 1,p.id,p.display_name,p.base_entry_id,p.size,p.created_at,p.updated_at,
                           COALESCE(pp.mode,pb.mode,420),1
                    FROM drive_pending_files p
                    LEFT JOIN drive_permissions pp
                      ON pp.node_kind='pending' AND pp.node_id=p.id
                    LEFT JOIN drive_permissions pb
                      ON pb.node_kind='file' AND pb.node_id=p.base_entry_id
                    WHERE p.directory_id IS ?1
                    UNION ALL
                    SELECT 2,e.entry_id,e.display_name,NULL,
                           COALESCE(e.plaintext_size,f.total_size),f.created_at,e.updated_at,
                           COALESCE(pm.mode,420),1
                    FROM drive_file_entries e
                    JOIN logical_files f ON f.file_id=e.file_id
                    JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                    LEFT JOIN drive_permissions pm
                      ON pm.node_kind='file' AND pm.node_id=e.entry_id
                    WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL AND f.status='complete'
                      AND NOT EXISTS(
                          SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id
                      )
                      AND NOT EXISTS (
                          SELECT 1 FROM drive_entries_v2 shadow
                          JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                          WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                      )
                    UNION ALL
                    SELECT 2,e.entry_id,e.display_name,NULL,o.plaintext_size,e.created_at,e.updated_at,
                           COALESCE(pm.mode,420),1
                    FROM drive_entries_v2 e
                    JOIN drive_objects o ON o.object_id=e.object_id
                    LEFT JOIN drive_permissions pm
                      ON pm.node_kind='file' AND pm.node_id=e.entry_id
                    WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL
                      AND NOT EXISTS(
                          SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id
                      )
                    UNION ALL
                    SELECT 3,s.id,s.display_name,NULL,length(CAST(s.target AS BLOB)),
                           s.created_at,s.updated_at,511,1
                    FROM drive_symlinks s
                    WHERE s.directory_id IS ?1 AND s.trashed_at IS NULL
                 )
                 SELECT current_directory.id AS current_id,
                        current_directory.parent_id AS current_parent_id,
                        current_directory.name AS current_name,
                        current_directory.backing_logical_channel_id AS current_backing_channel_id,
                        current_directory.created_at AS current_created_at,
                        current_directory.updated_at AS current_updated_at,
                        current_directory.mode AS current_mode,
                         current_directory.nlink AS current_nlink,
                         2 + (SELECT COUNT(*) FROM drive_directories root_child
                              WHERE root_child.parent_id IS NULL AND root_child.trashed_at IS NULL) AS root_nlink,
                        parent_directory.id AS parent_row_id,
                        parent_directory.parent_id AS parent_parent_id,
                        parent_directory.name AS parent_name,
                        parent_directory.backing_logical_channel_id AS parent_backing_channel_id,
                        parent_directory.created_at AS parent_created_at,
                        parent_directory.updated_at AS parent_updated_at,
                        parent_directory.mode AS parent_mode,
                         parent_directory.nlink AS parent_nlink,
                        CASE WHEN current_directory.parent_id IS NOT NULL
                                  AND parent_directory.id IS NULL THEN 1 ELSE 0 END AS missing_parent,
                        child_rows.kind AS kind, child_rows.id AS id, child_rows.name AS name,
                        child_rows.base_entry_id AS base_entry_id, child_rows.size AS size,
                        child_rows.created_at AS created_at, child_rows.updated_at AS updated_at,
                        child_rows.mode AS mode, child_rows.nlink AS nlink
                 FROM current_directory
                 LEFT JOIN parent_directory ON 1=1
                 LEFT JOIN child_rows ON 1=1
                 ORDER BY child_rows.name COLLATE NOCASE ASC, child_rows.kind ASC,
                          child_rows.id ASC",
            )
            .map_err(|error: sqlite::Error| error.to_string())?;
        statement
            .bind((1, current_id))
            .map_err(|error: sqlite::Error| error.to_string())?;
        if statement
            .next()
            .map_err(|error: sqlite::Error| error.to_string())?
            != sqlite::State::Row
        {
            return Ok(None);
        }

        let row_id = statement
            .read::<Option<String>, _>("current_id")
            .map_err(|error| error.to_string())?;
        let current_parent_id = statement
            .read::<Option<String>, _>("current_parent_id")
            .map_err(|error| error.to_string())?;
        let missing_parent = statement
            .read::<i64, _>("missing_parent")
            .map_err(|error| error.to_string())?;
        if missing_parent != 0 {
            return Err(
                "Drive readdirplus parent lookup failed: parent is missing or trashed".into(),
            );
        }
        let current_mode = statement
            .read::<i64, _>("current_mode")
            .map_err(|error| error.to_string())?;
        if !(0..=0o777).contains(&current_mode) {
            return Err("Invalid stored Drive directory permissions".to_string());
        }

        let node = match row_id {
            None if current_id.is_none() => Node::Root,
            None => return Ok(None),
            Some(id) => {
                let name = statement
                    .read::<Option<String>, _>("current_name")
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Drive readdirplus current directory has no name".to_string())?;
                let backing_logical_channel_id = statement
                    .read::<Option<String>, _>("current_backing_channel_id")
                    .map_err(|error| error.to_string())?;
                let created_at = statement
                    .read::<Option<i64>, _>("current_created_at")
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        "Drive readdirplus current directory has no creation time".to_string()
                    })?;
                let updated_at = statement
                    .read::<Option<i64>, _>("current_updated_at")
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        "Drive readdirplus current directory has no update time".to_string()
                    })?;
                Node::Directory(DriveDirectoryRecord {
                    id,
                    parent_id: current_parent_id.clone(),
                    name,
                    backing_logical_channel_id,
                    created_at,
                    updated_at,
                    trashed_at: None,
                })
            }
        };
        let current_nlink = statement
            .read::<i64, _>("current_nlink")
            .map_err(|error| error.to_string())?;
        let current_nlink = u32::try_from(current_nlink)
            .map_err(|_| "Invalid Drive current-directory link count".to_string())?;
        if current_nlink < 2 {
            return Err("Invalid Drive current-directory link count".into());
        }
        let mut current_attr = attr_for(&node);
        current_attr.perm = current_mode as u16;
        current_attr.nlink = current_nlink;

        let (parent_inode, parent_attr) = if current_parent_id.is_some() {
            let id = statement
                .read::<Option<String>, _>("parent_row_id")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent directory is missing".to_string())?;
            let parent_id = statement
                .read::<Option<String>, _>("parent_parent_id")
                .map_err(|error| error.to_string())?;
            let name = statement
                .read::<Option<String>, _>("parent_name")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent directory has no name".to_string())?;
            let backing_logical_channel_id = statement
                .read::<Option<String>, _>("parent_backing_channel_id")
                .map_err(|error| error.to_string())?;
            let created_at = statement
                .read::<Option<i64>, _>("parent_created_at")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent has no creation time".to_string())?;
            let updated_at = statement
                .read::<Option<i64>, _>("parent_updated_at")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent has no update time".to_string())?;
            let mode = statement
                .read::<Option<i64>, _>("parent_mode")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent has no permissions".to_string())?;
            if !(0..=0o777).contains(&mode) {
                return Err("Invalid stored Drive parent permissions".into());
            }
            let parent_node = Node::Directory(DriveDirectoryRecord {
                id,
                parent_id,
                name,
                backing_logical_channel_id,
                created_at,
                updated_at,
                trashed_at: None,
            });
            let parent_nlink = statement
                .read::<Option<i64>, _>("parent_nlink")
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Drive readdirplus parent has no link count".to_string())?;
            let parent_nlink = u32::try_from(parent_nlink)
                .map_err(|_| "Invalid Drive parent-directory link count".to_string())?;
            if parent_nlink < 2 {
                return Err("Invalid Drive parent-directory link count".into());
            }
            let mut attr = attr_for(&parent_node);
            attr.perm = mode as u16;
            attr.nlink = parent_nlink;
            (attr.ino, attr)
        } else {
            let root_nlink = statement
                .read::<i64, _>("root_nlink")
                .map_err(|error| error.to_string())?;
            let root_nlink = u32::try_from(root_nlink)
                .map_err(|_| "Invalid Drive root-directory link count".to_string())?;
            if root_nlink < 2 {
                return Err("Invalid Drive root-directory link count".into());
            }
            let mut root = attr_for(&Node::Root);
            root.nlink = root_nlink;
            (ROOT_INODE, root)
        };

        let (uid, gid) = current_ids();
        let mut entries = Vec::new();
        loop {
            let row_kind = statement
                .read::<Option<i64>, _>("kind")
                .map_err(|error| error.to_string())?;
            let Some(kind) = row_kind else {
                if statement
                    .next()
                    .map_err(|error: sqlite::Error| error.to_string())?
                    == sqlite::State::Row
                {
                    continue;
                }
                break;
            };
            let id = statement
                .read::<String, _>("id")
                .map_err(|error| error.to_string())?;
            let name = statement
                .read::<String, _>("name")
                .map_err(|error| error.to_string())?;
            let base_entry_id = statement
                .read::<Option<String>, _>("base_entry_id")
                .map_err(|error| error.to_string())?;
            let size = statement
                .read::<i64, _>("size")
                .map_err(|error| error.to_string())?;
            if size < 0 {
                return Err("Drive directory-plus returned a negative size".to_string());
            }
            let size = size as u64;
            let created_at = statement
                .read::<i64, _>("created_at")
                .map_err(|error| error.to_string())?;
            let updated_at = statement
                .read::<i64, _>("updated_at")
                .map_err(|error| error.to_string())?;
            let mode = statement
                .read::<i64, _>("mode")
                .map_err(|error| error.to_string())?;
            if !(0..=0o777).contains(&mode) {
                return Err("Invalid stored Drive permissions".to_string());
            }
            let nlink = statement
                .read::<i64, _>("nlink")
                .map_err(|error| error.to_string())?;
            let nlink = u32::try_from(nlink)
                .map_err(|_| "Drive directory-plus returned an invalid link count".to_string())?;
            if nlink == 0 {
                return Err("Drive directory-plus returned a zero link count".to_string());
            }

            let (ino, file_type, identity, created, blocks, blksize) = match kind {
                0 => (
                    inode_for("dir", &id),
                    FileType::Directory,
                    DriveNodeId::Directory(id),
                    millis_to_time(created_at),
                    0,
                    4096,
                ),
                1 => {
                    let visible_id = base_entry_id.unwrap_or(id);
                    (
                        inode_for("file", &visible_id),
                        FileType::RegularFile,
                        DriveNodeId::File(visible_id),
                        millis_to_time(created_at),
                        size.div_ceil(512),
                        4096,
                    )
                }
                2 => (
                    inode_for("file", &id),
                    FileType::RegularFile,
                    DriveNodeId::File(id),
                    seconds_to_time(created_at),
                    0,
                    CACHE_BLOCK_SIZE as u32,
                ),
                3 => (
                    inode_for("file", &id),
                    FileType::Symlink,
                    DriveNodeId::File(id),
                    seconds_to_time(created_at / 1000),
                    0,
                    CACHE_BLOCK_SIZE as u32,
                ),
                _ => return Err("Drive directory-plus returned an invalid node kind".to_string()),
            };
            let updated = millis_to_time(updated_at);
            let attr = FileAttr {
                ino,
                size,
                blocks,
                atime: updated,
                mtime: updated,
                ctime: updated,
                crtime: created,
                kind: file_type,
                perm: mode as u16,
                nlink,
                uid,
                gid,
                rdev: 0,
                blksize,
                flags: 0,
            };
            entries.push(IndexedDirectoryPlusEntry {
                ino,
                kind: file_type,
                name,
                identity,
                attr,
            });
            if statement
                .next()
                .map_err(|error: sqlite::Error| error.to_string())?
                != sqlite::State::Row
            {
                break;
            }
        }
        Ok(Some(IndexedDirectoryPlusListing {
            node,
            parent_inode,
            current_attr,
            parent_attr,
            entries,
        }))
    }

    #[cfg(target_os = "linux")]
    fn indexed_directory_plus_entries(
        conn: &sqlite::Connection,
        current_id: Option<&str>,
    ) -> Result<Vec<IndexedDirectoryPlusEntry>, String> {
        indexed_directory_plus_listing(conn, current_id)?
            .map(|listing| listing.entries)
            .ok_or_else(|| "Drive directory disappeared during readdirplus".to_string())
    }

    #[cfg(target_os = "linux")]
    fn millis_to_time(value: i64) -> SystemTime {
        if value <= 0 {
            return UNIX_EPOCH;
        }
        UNIX_EPOCH + Duration::from_millis(value as u64)
    }

    #[cfg(target_os = "linux")]
    fn seconds_to_time(value: i64) -> SystemTime {
        if value <= 0 {
            return UNIX_EPOCH;
        }
        UNIX_EPOCH + Duration::from_secs(value as u64)
    }

    #[cfg(target_os = "linux")]
    fn current_ids() -> (u32, u32) {
        // SAFETY: getuid/getgid are side-effect-free libc calls.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn directory_link_count(
        conn: &sqlite::Connection,
        parent_id: Option<&str>,
    ) -> Result<u32, String> {
        let mut query = conn
            .prepare(
                "SELECT COUNT(*) FROM drive_directories
             WHERE parent_id IS ? AND trashed_at IS NULL",
            )
            .map_err(|error: sqlite::Error| error.to_string())?;
        query
            .bind((1, parent_id))
            .map_err(|error: sqlite::Error| error.to_string())?;
        if query
            .next()
            .map_err(|error: sqlite::Error| error.to_string())?
            != sqlite::State::Row
        {
            return Err("Drive directory link count could not be read".into());
        }
        let children = query.read::<i64, _>(0).map_err(|error| error.to_string())?;
        let children = u32::try_from(children)
            .map_err(|_| "Drive directory link count exceeds POSIX limits".to_string())?;
        2u32.checked_add(children)
            .ok_or_else(|| "Drive directory link count exceeds POSIX limits".to_string())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn checked_native_setattr_size(kind: FileType, size: u64) -> Result<(), i32> {
        match kind {
            FileType::Directory => Err(libc::EISDIR),
            FileType::RegularFile if size > i64::MAX as u64 => Err(libc::EFBIG),
            FileType::RegularFile => Ok(()),
            _ => Err(libc::EINVAL),
        }
    }

    #[cfg(target_os = "linux")]
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
                    // Remote contents occupy Telegram storage, not this mounted path.
                    blocks: 0,
                    atime: updated,
                    mtime: updated,
                    ctime: updated,
                    crtime: created,
                    kind: if file.storage_version == 3 {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    },
                    perm: if file.storage_version == 3 {
                        0o777
                    } else {
                        0o644
                    },
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

    #[cfg(test)]
    static CORE_NODE_LOCKED_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    // Test-only observability of the live indexed-file resolution boundary.
    // Production reads never allocate or update this counter.
    #[cfg(test)]
    static INDEXED_REGULAR_NODE_RESOLUTIONS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[cfg(test)]
    fn reset_indexed_regular_node_resolution_count() {
        INDEXED_REGULAR_NODE_RESOLUTIONS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn indexed_regular_node_resolution_count() -> usize {
        INDEXED_REGULAR_NODE_RESOLUTIONS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    fn reset_core_node_locked_call_count() {
        CORE_NODE_LOCKED_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn core_node_locked_call_count() -> usize {
        CORE_NODE_LOCKED_CALLS.load(std::sync::atomic::Ordering::SeqCst)
    }

    struct TeraRelayFs {
        db: DbConnection,
        app: AppHandle,
        stream_port: u16,
        stream_token: String,
        cache_root: PathBuf,
        staging_root: PathBuf,
        cache_pruner: crate::cache_cleanup::DriveCachePruner,
        http: reqwest::blocking::Client,
        telegram: crate::commands::TelegramState,
        mount_state: DriveMountState,
        emitted_uploads: Mutex<std::collections::HashSet<String>>,
        writer_counts: Mutex<HashMap<u64, u32>>,
        // Cache immutable identities and stable file types; authoritative metadata
        // is still resolved for reads, attributes, and mutations.
        #[cfg(target_os = "linux")]
        inode_index: Mutex<HashMap<u64, IndexedInode>>,
    }

    fn build_backend(
        app_handle: &AppHandle,
        state: &DriveMountState,
        db_pool: &DbConnection,
        telegram_state: &crate::commands::TelegramState,
    ) -> Result<Arc<TeraRelayFs>, String> {
        {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::drive_metadata::init_drive_schema(&conn)?;
            crate::drive_lookup::init_indexes(&conn)?;
            #[cfg(target_os = "linux")]
            recover_retained_stages(&state.staging_root, |id| {
                drive_pending_by_id(&conn, id).map(|row| row.is_some())
            })?;
        }
        Ok(Arc::new(TeraRelayFs {
            db: db_pool.clone(),
            cache_pruner: crate::cache_cleanup::DriveCachePruner::new(
                state.cache_root.clone(),
                CACHE_MAX_BYTES,
            )?,
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
            telegram: telegram_state.clone(),
            mount_state: state.clone(),
            emitted_uploads: Mutex::new(std::collections::HashSet::new()),
            writer_counts: Mutex::new(HashMap::new()),
            #[cfg(target_os = "linux")]
            inode_index: Mutex::new(HashMap::new()),
        }))
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub(super) fn build_core(
        app_handle: &AppHandle,
        state: &DriveMountState,
        db_pool: &DbConnection,
        telegram_state: &crate::commands::TelegramState,
    ) -> Result<DriveFsCore, String> {
        Ok(DriveFsCore::new(build_backend(
            app_handle,
            state,
            db_pool,
            telegram_state,
        )?))
    }

    fn visible_snapshot_revision(snapshot: &DriveSnapshot) -> Result<u64, String> {
        // File Provider sync anchors must describe the filesystem view, not only
        // the cloud manifest. Pending copy-on-write/new-file rows are intentionally
        // local until finalization and therefore must not dirty Device Continuity,
        // but they still participate in native-provider enumeration consistency.
        let mut normalized = snapshot.clone();
        normalized
            .directories
            .sort_by(|left, right| left.id.cmp(&right.id));
        normalized
            .files
            .sort_by(|left, right| left.entry_id.cmp(&right.entry_id));
        normalized
            .pending
            .sort_by(|left, right| left.id.cmp(&right.id));
        let encoded = serde_json::to_vec(&normalized)
            .map_err(|error| format!("Could not fingerprint Drive provider snapshot: {error}"))?;
        let digest = Sha256::digest(encoded);
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&digest[..8]);
        Ok(u64::from_be_bytes(prefix))
    }

    impl TeraRelayFs {
        fn snapshot(&self) -> Result<DriveSnapshot, String> {
            let snapshot = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_snapshot(&conn)?
            };
            #[cfg(target_os = "linux")]
            {
                // Assemble a replacement index privately. If even one
                // identity collides, leave the old live mapping unchanged.
                let nodes = snapshot
                    .directories
                    .iter()
                    .cloned()
                    .map(Node::Directory)
                    .chain(snapshot.files.iter().cloned().map(Node::Remote))
                    .chain(snapshot.pending.iter().cloned().map(Node::Pending))
                    .chain(snapshot.symlinks.iter().map(symlink_node))
                    .map(|node| {
                        (
                            node.inode(),
                            IndexedInode {
                                identity: Self::inode_identity(&node),
                                kind: indexed_node_kind(&node),
                            },
                        )
                    });
                let mut replacement = HashMap::new();
                checked_inode_index_batch(&mut replacement, nodes)?;
                *self
                    .inode_index
                    .lock()
                    .map_err(|_| "Drive inode index poisoned".to_string())? = replacement;
            }
            Ok(snapshot)
        }

        #[cfg(target_os = "linux")]
        fn inode_identity(node: &Node) -> DriveNodeId {
            match node {
                Node::Pending(pending) => DriveNodeId::File(
                    pending
                        .base_entry_id
                        .clone()
                        .unwrap_or_else(|| pending.id.clone()),
                ),
                _ => core_node_id(node),
            }
        }

        #[cfg(target_os = "linux")]
        fn remember_node(&self, node: &Node) -> Result<(), String> {
            let mut index = self
                .inode_index
                .lock()
                .map_err(|_| "Drive inode index poisoned".to_string())?;
            checked_inode_index_batch(
                &mut index,
                std::iter::once((
                    node.inode(),
                    IndexedInode {
                        identity: Self::inode_identity(node),
                        kind: indexed_node_kind(node),
                    },
                )),
            )
        }

        #[cfg(target_os = "linux")]
        fn node_for_inode(&self, ino: u64) -> Result<Option<Node>, String> {
            if ino == ROOT_INODE {
                return Ok(Some(Node::Root));
            }
            let cached = self
                .inode_index
                .lock()
                .map_err(|_| "Drive inode index poisoned".to_string())?
                .get(&ino)
                .map(|entry| entry.identity.clone());
            let id = if let Some(id) = cached {
                id
            } else {
                self.snapshot()?;
                let Some(id) = self
                    .inode_index
                    .lock()
                    .map_err(|_| "Drive inode index poisoned".to_string())?
                    .get(&ino)
                    .map(|entry| entry.identity.clone())
                else {
                    return Ok(None);
                };
                id
            };
            match self.core_node(&id) {
                Ok(node) => Ok(Some(node)),
                Err(DriveFsError::NotFound) => {
                    let mut index = self
                        .inode_index
                        .lock()
                        .map_err(|_| "Drive inode index poisoned".to_string())?;
                    if index.get(&ino).is_some_and(|entry| entry.identity == id) {
                        index.remove(&ino);
                    }
                    Ok(None)
                }
                Err(error) => Err(error.to_string()),
            }
        }

        #[cfg(target_os = "linux")]
        fn directory_id_for_inode(&self, ino: u64) -> Result<Option<Option<String>>, String> {
            match self.node_for_inode(ino)? {
                Some(Node::Root) => Ok(Some(None)),
                Some(Node::Directory(directory)) => Ok(Some(Some(directory.id))),
                Some(_) => Ok(None),
                None => Ok(None),
            }
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
                .map_err(|e| {
                    format!(
                        "TeraRelay Drive could not fetch file data: {}",
                        e.without_url()
                    )
                })?;
            read_range_response(response, start, end)
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
            // Even cached ciphertext must be authenticated with the unlocked
            // master key. Locking the Drive must prevent plaintext reads.
            let key = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                crate::drive_crypto::load_verified_master_key_for_db(&self.app, &conn)?
            }
            .ok_or_else(|| {
                "TeraRelay Drive is locked. Unlock Drive encryption in Settings.".to_string()
            })?;
            let (encoded, plaintext, fetched) = authenticated_legacy_cache_read(
                read_cached_block(&cache_path, cipher_len),
                || {
                    // Drop bad or truncated ciphertext before the remote
                    // retry. A failure leaves the cache absent for next time.
                    let _ = std::fs::remove_file(&cache_path);
                    self.fetch_stream_range(file, cipher_start, cipher_start + cipher_len - 1)
                },
                |bytes| {
                    crate::drive_crypto::decrypt_chunk(
                        &key,
                        crypto_id,
                        crypto_chunk,
                        plain_len,
                        bytes,
                    )
                },
            )?;
            if fetched && write_cached_block_atomic(&cache_path, &encoded) {
                self.evict_cache_best_effort();
            }
            Ok(plaintext)
        }

        fn fetch_v2_plain_crypto_chunk(
            &self,
            file: &DriveFileRecord,
            crypto_chunk: u64,
            #[cfg(target_os = "linux")] locations: Option<&FrozenV2Locations>,
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
            let live_location = || {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_object_block_location_preinitialized(&conn, &file.file_id, crypto_chunk)?
                    .ok_or_else(|| "Drive V2 block location is missing".to_string())
            };
            #[cfg(target_os = "linux")]
            let (block, part) = match locations {
                Some(frozen) => frozen.location(&file.file_id, crypto_chunk)?,
                None => live_location()?,
            };
            #[cfg(not(target_os = "linux"))]
            let (block, part) = live_location()?;
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
            let encoded = match read_cached_block(&cache_path, block.ciphertext_size) {
                Some(bytes) if format!("{:x}", Sha256::digest(&bytes)) == block.sha256 => bytes,
                _ => {
                    // The cache is disposable. Recover corruption in this same
                    // read, while keeping pending write staging untouched.
                    let _ = std::fs::remove_file(&cache_path);
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
                        .map_err(|e| {
                            format!("Could not fetch Drive V2 block: {}", e.without_url())
                        })?;
                    let bytes = read_range_response(response, block.part_offset, range_end)?;
                    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
                    if actual_hash != block.sha256 {
                        return Err("Drive V2 block checksum mismatch".to_string());
                    }
                    if write_cached_block_atomic(&cache_path, &bytes) {
                        self.evict_cache_best_effort();
                    }
                    bytes
                }
            };

            // Both branches verified the encoded hash before serving or caching bytes.
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
                    &key,
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
            #[cfg(target_os = "linux")] locations: Option<&FrozenV2Locations>,
        ) -> Result<Vec<u8>, String> {
            if file.storage_version == 3 {
                return Err("Symlink contents require kernel path resolution".into());
            }
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
                let plaintext = self.fetch_v2_plain_crypto_chunk(
                    file,
                    crypto_chunk,
                    #[cfg(target_os = "linux")]
                    locations,
                )?;
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
            if let Some(bytes) = read_cached_block(&path, end - start + 1) {
                return Ok(bytes);
            }
            let bytes = self.fetch_stream_range(file, start, end)?;
            if write_cached_block_atomic(&path, &bytes) {
                self.evict_cache_best_effort();
            }
            Ok(bytes)
        }

        fn evict_cache_best_effort(&self) {
            // A full cache walk may take seconds on a large Drive cache.
            // The FUSE read dispatcher is serial; never do that work inline.
            self.cache_pruner.schedule();
        }

        fn read_remote_range(
            &self,
            file: &DriveFileRecord,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>, String> {
            self.read_remote_range_with_locations(
                file,
                offset,
                size,
                #[cfg(target_os = "linux")]
                None,
            )
        }

        fn read_remote_range_with_locations(
            &self,
            file: &DriveFileRecord,
            offset: u64,
            size: u32,
            #[cfg(target_os = "linux")] locations: Option<&FrozenV2Locations>,
        ) -> Result<Vec<u8>, String> {
            if file.storage_version == 3 {
                return Err("Symlink contents require kernel path resolution".into());
            }
            let visible_size = file.plaintext_size.unwrap_or(file.total_size);
            if offset >= visible_size || size == 0 {
                return Ok(Vec::new());
            }
            let end_exclusive = std::cmp::min(visible_size, offset.saturating_add(size as u64));
            let first_block = offset / CACHE_BLOCK_SIZE;
            let last_block = (end_exclusive.saturating_sub(1)) / CACHE_BLOCK_SIZE;
            let mut output = Vec::with_capacity((end_exclusive - offset) as usize);

            for block in first_block..=last_block {
                // The serial FUSE loop can be held by a blocking remote
                // cache miss. Attribute its time without logging filenames.
                #[cfg(target_os = "linux")]
                let _remote_phase = SlowFusePhase::new("remote-block-fetch");
                let bytes = self.fetch_remote_block(
                    file,
                    block,
                    #[cfg(target_os = "linux")]
                    locations,
                )?;
                let block_start = block * CACHE_BLOCK_SIZE;
                let slice = checked_remote_block_slice(&bytes, block_start, offset, end_exclusive)?;
                output.extend_from_slice(slice);
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
            let staging_dir = create_owned_staging_directory(&self.staging_root, &pending_id)?;
            let marker = staging_dir.join(".pending");
            if let Err(error) = create_private_file(&marker, false) {
                cleanup_failed_replacement_staging(&staging_dir, None);
                return Err(format!("Could not create Drive edit marker: {error}"));
            }
            let marker_string = marker.to_string_lossy().into_owned();
            let result = (|| {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                create_drive_pending_replacement(&conn, &pending_id, &file.entry_id, &marker_string)
            })();
            match result {
                Ok(pending) => {
                    self.mount_state.set_pending_active(&pending.id, true);
                    Ok(pending)
                }
                Err(error) => {
                    cleanup_failed_replacement_staging(&staging_dir, Some(&marker));
                    Err(error)
                }
            }
        }

        // Removing pending metadata is not permission to recursively remove
        // an arbitrary staging_path from SQLite. Use one guarded cleanup path
        // for both explicit cancellation and failed copy-on-write truncation.
        fn cleanup_removed_pending_stage(
            &self,
            pending: &DrivePendingRecord,
            deleted_path: Option<&str>,
        ) {
            let validated = self
                .app
                .path()
                .app_cache_dir()
                .map_err(|_| "Drive cache root is unavailable".to_string())
                .and_then(|cache_root| {
                    crate::drive_stream::owned_cancelled_staging_dir(
                        &cache_root,
                        pending,
                        deleted_path,
                    )
                    .map_err(str::to_string)
                });
            match validated {
                Ok(directory) => {
                    if let Err(error) = std::fs::remove_dir_all(directory) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            log::warn!("Could not clean removed Drive staging: {error}");
                        }
                    }
                }
                Err(error) => log::warn!("Skipped unsafe Drive staging cleanup: {error}"),
            }
        }

        fn cancel_pending_sync(
            &self,
            pending: &DrivePendingRecord,
            emit_cancel_event: bool,
        ) -> Result<(), String> {
            self.cancel_pending_sync_inner(pending, emit_cancel_event, true)
        }

        fn cancel_pending_sync_inner(
            &self,
            pending: &DrivePendingRecord,
            emit_cancel_event: bool,
            remove_staging: bool,
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

            if remove_staging {
                self.cleanup_removed_pending_stage(pending, path.as_deref());
            }

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

        #[cfg(target_os = "linux")]
        fn pending_for_indexed_identity(
            conn: &sqlite::Connection,
            identity: &DriveNodeId,
        ) -> Result<Option<DrivePendingRecord>, String> {
            match identity {
                DriveNodeId::Pending(id) => drive_pending_by_id(conn, id),
                DriveNodeId::File(id) => crate::drive_lookup::pending_for_file(conn, id),
                DriveNodeId::Root | DriveNodeId::Directory(_) => Ok(None),
            }
        }

        #[cfg(target_os = "linux")]
        fn indexed_regular_node_locked(
            conn: &sqlite::Connection,
            identity: &DriveNodeId,
        ) -> Result<Option<Node>, String> {
            let DriveNodeId::File(id) = identity else {
                return Ok(None);
            };
            #[cfg(test)]
            INDEXED_REGULAR_NODE_RESOLUTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::drive_lookup::regular_file_entry(conn, id).map(|entry| {
                entry.map(|entry| match entry {
                    crate::drive_lookup::RegularFileEntry::Pending(pending) => {
                        Node::Pending(pending)
                    }
                    crate::drive_lookup::RegularFileEntry::File(file) => Node::Remote(file),
                })
            })
        }

        #[cfg(target_os = "linux")]
        fn indexed_regular_attr_locked(
            conn: &sqlite::Connection,
            identity: &DriveNodeId,
        ) -> Result<Option<FileAttr>, String> {
            let id = match identity {
                DriveNodeId::File(id) | DriveNodeId::Pending(id) => id,
                DriveNodeId::Root | DriveNodeId::Directory(_) => return Ok(None),
            };
            crate::drive_lookup::regular_file_metadata(conn, id).map(|metadata| {
                metadata.map(|metadata| {
                    let node = match metadata.entry {
                        crate::drive_lookup::RegularFileEntry::Pending(pending) => {
                            Node::Pending(pending)
                        }
                        crate::drive_lookup::RegularFileEntry::File(file) => Node::Remote(file),
                    };
                    let mut attr = attr_for(&node);
                    if let Some(mode) = metadata.mode {
                        attr.perm = mode;
                    }
                    attr
                })
            })
        }

        #[cfg(target_os = "linux")]
        fn pending_for_inode(&self, ino: u64) -> Result<Option<DrivePendingRecord>, String> {
            if ino == ROOT_INODE {
                return Ok(None);
            }
            let mut identity = self
                .inode_index
                .lock()
                .map_err(|_| "Drive inode index poisoned".to_string())?
                .get(&ino)
                .map(|entry| entry.identity.clone());
            if identity.is_none() {
                self.snapshot()?;
                identity = self
                    .inode_index
                    .lock()
                    .map_err(|_| "Drive inode index poisoned".to_string())?
                    .get(&ino)
                    .map(|entry| entry.identity.clone());
            }
            let Some(identity) = identity else {
                return Ok(None);
            };
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            Self::pending_for_indexed_identity(&conn, &identity)
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
            // Bound a single encoded block, including legacy standalone chunks.
            // The logical pending file can contain any number of these blocks.
            let max_block = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE
                + crate::drive_stream::DRIVE_ENCRYPTED_CHUNK_OVERHEAD;
            if record.chunk_size == 0 || record.chunk_size > max_block {
                return Err("Drive pending chunk size is invalid".to_string());
            }
            let end = part_offset
                .checked_add(record.chunk_size - 1)
                .ok_or_else(|| "Drive pending part byte range overflow".to_string())?;
            let response = self
                .http
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes={part_offset}-{end}"))
                .send()
                .map_err(|e| format!("Could not fetch pending Drive chunk: {}", e.without_url()))?;
            let encoded = read_range_response(response, part_offset, end)?;
            if format!("{:x}", Sha256::digest(&encoded)) != record.sha256 {
                return Err("Pending Drive chunk checksum mismatch".to_string());
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
                    &key,
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
            verified_pending_staging(&self.staging_root, pending)?;
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
                let local = open_private_read(&local_path)?;
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
                    let read = positioned_read(&local, &mut bytes, local_offset)
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
            verified_pending_staging(&self.staging_root, pending)?;
            if offset >= pending.size || size == 0 {
                return Ok(Vec::new());
            }
            let end = std::cmp::min(pending.size, offset.saturating_add(size as u64));
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let first = offset / chunk_size;
            let last = (end - 1) / chunk_size;
            let mut output = Vec::with_capacity((end - offset) as usize);
            let mut provenance = None;
            for index in first..=last {
                let chunk_start = index * chunk_size;
                let slice_start = offset.max(chunk_start);
                let slice_end = end.min(chunk_start.saturating_add(chunk_size));
                let local_path = crate::drive_stream::local_chunk_path(pending, index);
                // Cleanly uploaded chunks may no longer have a local file;
                // preserve their old remote/base resolution. If local bytes
                // exist, read only a fully covered dirty slice directly.
                let local_exists = match std::fs::symlink_metadata(&local_path) {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(format!("Could not inspect Drive staging: {error}")),
                };
                if local_exists {
                    let metadata = pending_read_provenance_once(&mut provenance, || {
                        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                        let uploaded_chunks = drive_pending_chunks(&conn, &pending.id)?
                            .into_iter()
                            .map(|chunk| chunk.chunk_index)
                            .collect();
                        let ranges = drive_pending_ranges(&conn, &pending.id)?;
                        Ok(PendingReadProvenance {
                            uploaded_chunks,
                            ranges,
                        })
                    })?;
                    // An encrypted uploaded chunk or COW base must still take
                    // the old materialization path to verify the Drive key,
                    // source checksums and retained remote provenance first.
                    if staged_read_can_skip_remote_provenance(
                        pending.base_entry_id.is_some(),
                        metadata.uploaded_chunks.contains(&index),
                    ) {
                        if let Some(bytes) = read_covered_staged_slice(
                            &local_path,
                            &metadata.ranges,
                            chunk_start,
                            slice_start,
                            slice_end,
                        )? {
                            output.extend_from_slice(&bytes);
                            continue;
                        }
                    }
                }
                let chunk = self.materialize_pending_chunk(pending, index)?;
                let from = (slice_start - chunk_start) as usize;
                let to = (slice_end - chunk_start).min(chunk.len() as u64) as usize;
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
            verified_pending_staging(&self.staging_root, pending)?;
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
                let file = open_or_create_private_file(&path)?;
                positioned_write_all(&file, &data[consumed..consumed + take], chunk_offset)
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
            verified_pending_staging(&self.staging_root, pending)?;
            if new_size == pending.size {
                return Ok(pending.clone());
            }

            // The isolated QA profile deliberately stages a complete local
            // file so its existing no-Telegram upload-queue harness can
            // operate on a real path. It must resize that same local file,
            // rather than changing only the production chunk/range metadata.
            if crate::commands::qa_feature_a::enabled() {
                let file = open_verified_qa_pending_file(&self.staging_root, pending, true)?;
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
                new_last.filter(|_| !new_size.is_multiple_of(chunk_size))
            } else if old_size > 0 && !old_size.is_multiple_of(chunk_size) {
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
                } else if new_size.is_multiple_of(chunk_size) {
                    Some(new_size / chunk_size)
                } else {
                    new_last
                }
            } else {
                changing_boundary
                    .filter(|index| uploaded.iter().any(|chunk| chunk.chunk_index == *index))
            };

            // Commit no changed references until the complete replacement is
            // durable. The newly installed file retains any previous local
            // tail, so old ranges remain readable if SQL rolls back or a
            // crash occurs before the savepoint is released.
            if let Some((index, bytes)) = preserved_boundary.as_ref() {
                let path = crate::drive_stream::local_chunk_path(pending, *index);
                // Atomic pre-staging temporarily holds a second bounded chunk.
                // Count the worst-case copy, including a previous sparse tail,
                // before creating it so resize cannot bypass the 256 MiB cap.
                let previous_size = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata.len(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                    Err(error) => {
                        return Err(format!("Could not inspect previous Drive stage: {error}"));
                    }
                };
                let extra = previous_size.max(bytes.len() as u64);
                staging_can_accept(allocated_stage_bytes(&self.staging_root)?, extra)?;
                stage_resized_boundary(&path, bytes)?;
            }

            let (removed_chunks, removed_parts, had_v2_parts) = {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                conn.execute("SAVEPOINT drive_resize_staged_boundary")
                    .map_err(|e| e.to_string())?;
                let result = (|| {
                    if new_size < old_size {
                        clear_drive_pending_ranges_for_span_in_transaction(
                            &conn,
                            &pending.id,
                            new_size,
                            old_size,
                        )?;
                    }
                    let had_v2_parts = !drive_pending_parts(&conn, &pending.id)?.is_empty();
                    let removed_chunks = match remove_from {
                        Some(index) => take_drive_pending_chunks_from(&conn, &pending.id, index)?,
                        None => Vec::new(),
                    };
                    // An overlapping old V2 part remains as rewrite source.
                    let removed_parts = if had_v2_parts {
                        match remove_from {
                            Some(index) => take_drive_pending_parts_starting_at_chunk(
                                &conn,
                                &pending.id,
                                index,
                            )?,
                            None => Vec::new(),
                        }
                    } else {
                        Vec::new()
                    };
                    update_drive_pending_size(&conn, &pending.id, new_size)?;
                    if let Some((index, bytes)) = preserved_boundary.as_ref() {
                        let start = index * chunk_size;
                        record_drive_pending_range_in_transaction(
                            &conn,
                            &pending.id,
                            start,
                            start + bytes.len() as u64,
                        )?;
                    }
                    Ok((removed_chunks, removed_parts, had_v2_parts))
                })();
                crate::drive_posix::finish_savepoint(&conn, "drive_resize_staged_boundary", result)?
            };

            if new_size < old_size {
                let first_removed_local = new_size.div_ceil(chunk_size);
                let old_chunks = old_size.div_ceil(chunk_size);
                for index in first_removed_local..old_chunks {
                    let _ =
                        std::fs::remove_file(crate::drive_stream::local_chunk_path(pending, index));
                }
                if new_size > 0 && !new_size.is_multiple_of(chunk_size) {
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
            verified_pending_staging(&self.staging_root, pending)?;
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

    fn core_error(error: String) -> DriveFsError {
        let lower = error.to_ascii_lowercase();
        if lower.contains("not found") || lower.contains("disappeared") {
            DriveFsError::NotFound
        } else if lower.contains("already exists") {
            DriveFsError::AlreadyExists
        } else if lower.contains("not a directory") {
            DriveFsError::NotDirectory
        } else if lower.contains("not empty") {
            DriveFsError::DirectoryNotEmpty
        } else if lower.contains("locked") {
            DriveFsError::Locked(error)
        } else if lower.contains("staging limit reached") || lower.contains("no space") {
            DriveFsError::NoSpace(error)
        } else if lower.contains("denied") || lower.contains("permission") {
            DriveFsError::AccessDenied(error)
        } else if lower.contains("invalid") || lower.contains("overflow") {
            DriveFsError::InvalidInput(error)
        } else {
            DriveFsError::Internal(error)
        }
    }

    #[cfg(target_os = "linux")]
    fn errno_for_core(error: &DriveFsError) -> i32 {
        match error {
            DriveFsError::NotFound => ENOENT,
            DriveFsError::AlreadyExists => EEXIST,
            DriveFsError::NotDirectory => ENOTDIR,
            DriveFsError::IsDirectory => EISDIR,
            DriveFsError::DirectoryNotEmpty => ENOTEMPTY,
            DriveFsError::InvalidInput(_) => EINVAL,
            DriveFsError::AccessDenied(_) | DriveFsError::Locked(_) => EACCES,
            DriveFsError::NoSpace(_) => ENOSPC,
            DriveFsError::Cancelled | DriveFsError::Unavailable(_) | DriveFsError::Internal(_) => {
                EIO
            }
        }
    }

    fn core_parent_id(parent: &DriveNodeId) -> Result<Option<String>, DriveFsError> {
        match parent {
            DriveNodeId::Root => Ok(None),
            DriveNodeId::Directory(id) => Ok(Some(id.clone())),
            DriveNodeId::File(_) | DriveNodeId::Pending(_) => Err(DriveFsError::NotDirectory),
        }
    }

    fn core_node_id(node: &Node) -> DriveNodeId {
        match node {
            Node::Root => DriveNodeId::Root,
            Node::Directory(directory) => DriveNodeId::Directory(directory.id.clone()),
            Node::Remote(file) => DriveNodeId::File(file.entry_id.clone()),
            Node::Pending(pending) => DriveNodeId::Pending(pending.id.clone()),
        }
    }

    fn core_parent_node(parent_id: Option<&str>) -> DriveNodeId {
        parent_id
            .map(|id| DriveNodeId::Directory(id.to_string()))
            .unwrap_or(DriveNodeId::Root)
    }

    fn core_metadata(node: &Node) -> DriveNodeMetadata {
        match node {
            Node::Root => DriveNodeMetadata {
                id: DriveNodeId::Root,
                parent: DriveNodeId::Root,
                name: "TeraRelay Drive".to_string(),
                kind: DriveNodeKind::Directory,
                size: 0,
                created_at_ms: 0,
                updated_at_ms: 0,
                content_version: "root".to_string(),
                pending: false,
            },
            Node::Directory(directory) => DriveNodeMetadata {
                id: DriveNodeId::Directory(directory.id.clone()),
                parent: core_parent_node(directory.parent_id.as_deref()),
                name: directory.name.clone(),
                kind: DriveNodeKind::Directory,
                size: 0,
                created_at_ms: directory.created_at,
                updated_at_ms: directory.updated_at,
                content_version: directory.id.clone(),
                pending: false,
            },
            Node::Remote(file) => DriveNodeMetadata {
                id: DriveNodeId::File(file.entry_id.clone()),
                parent: core_parent_node(file.directory_id.as_deref()),
                name: file.display_name.clone(),
                kind: DriveNodeKind::File,
                size: file.plaintext_size.unwrap_or(file.total_size),
                created_at_ms: file.created_at.saturating_mul(1000),
                updated_at_ms: file.updated_at,
                content_version: file.file_id.clone(),
                pending: false,
            },
            Node::Pending(pending) => DriveNodeMetadata {
                id: DriveNodeId::File(
                    pending
                        .base_entry_id
                        .clone()
                        .unwrap_or_else(|| pending.id.clone()),
                ),
                parent: core_parent_node(pending.directory_id.as_deref()),
                name: pending.display_name.clone(),
                kind: DriveNodeKind::File,
                size: pending.size,
                created_at_ms: pending.created_at,
                updated_at_ms: pending.updated_at,
                content_version: pending.id.clone(),
                pending: true,
            },
        }
    }

    impl TeraRelayFs {
        fn core_node_locked(
            conn: &sqlite::Connection,
            id: &DriveNodeId,
        ) -> Result<Node, DriveFsError> {
            #[cfg(test)]
            CORE_NODE_LOCKED_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match id {
                DriveNodeId::Root => Ok(Node::Root),
                DriveNodeId::Directory(id) => crate::drive_lookup::directory(conn, id)
                    .map_err(core_error)?
                    .map(Node::Directory)
                    .ok_or(DriveFsError::NotFound),
                DriveNodeId::Pending(id) => drive_pending_by_id(conn, id)
                    .map_err(core_error)?
                    .map(Node::Pending)
                    .ok_or(DriveFsError::NotFound),
                DriveNodeId::File(id) => {
                    #[cfg(target_os = "linux")]
                    if let Some(link) =
                        crate::drive_posix::symlink_by_id(conn, id).map_err(core_error)?
                    {
                        return Ok(symlink_node(&link));
                    }
                    if let Some(pending) =
                        crate::drive_lookup::pending_for_file(conn, id).map_err(core_error)?
                    {
                        return Ok(Node::Pending(pending));
                    }
                    crate::drive_lookup::file(conn, id)
                        .map_err(core_error)?
                        .map(Node::Remote)
                        .ok_or(DriveFsError::NotFound)
                }
            }
        }

        fn core_node(&self, id: &DriveNodeId) -> Result<Node, DriveFsError> {
            if matches!(id, DriveNodeId::Root) {
                return Ok(Node::Root);
            }
            let conn = self
                .db
                .lock()
                .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
            Self::core_node_locked(&conn, id)
        }

        fn core_lookup_node(&self, parent: &DriveNodeId, name: &str) -> Result<Node, DriveFsError> {
            let parent_id = core_parent_id(parent)?;
            let conn = self
                .db
                .lock()
                .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
            let id = crate::drive_lookup::child(&conn, parent_id.as_deref(), name)
                .map_err(core_error)?
                .ok_or(DriveFsError::NotFound)?;
            Self::core_node_locked(&conn, &id)
        }

        fn core_create_pending(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DrivePendingRecord, DriveFsError> {
            require_missing_for_creation(self.core_lookup_node(parent, name))?;
            self.ensure_write_key_ready().map_err(core_error)?;
            let parent_id = core_parent_id(parent)?;
            let pending_id = crate::commands::drive_metadata::new_drive_id();
            let staging_dir = create_owned_staging_directory(&self.staging_root, &pending_id)
                .map_err(core_error)?;
            let staging_path = if crate::commands::qa_feature_a::enabled() {
                staging_dir.join(name)
            } else {
                staging_dir.join(".pending")
            };
            let staging_string = staging_path.to_string_lossy().into_owned();
            let pending = (|| {
                let conn = self
                    .db
                    .lock()
                    .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                create_drive_pending_file(
                    &conn,
                    &pending_id,
                    parent_id.as_deref(),
                    name,
                    &staging_string,
                )
                .map_err(core_error)
            })();
            let pending = match pending {
                Ok(pending) => pending,
                Err(error) => {
                    let _ = std::fs::remove_dir(&staging_dir);
                    return Err(error);
                }
            };
            if let Err(error) = create_private_file(&staging_path, false) {
                if let Ok(conn) = self.db.lock() {
                    if delete_drive_pending(&conn, &pending.id).is_ok() {
                        let _ = std::fs::remove_dir(&staging_dir);
                    }
                }
                return Err(DriveFsError::Internal(format!(
                    "Drive staging create failed: {error}"
                )));
            }
            self.mount_state.set_pending_active(&pending.id, true);
            Ok(pending)
        }

        fn read_resolved_node(
            &self,
            node: &Node,
            offset: u64,
            length: u64,
        ) -> Result<Vec<u8>, DriveFsError> {
            if length == 0 {
                return Ok(Vec::new());
            }
            match node {
                Node::Remote(file) => {
                    let size = file.plaintext_size.unwrap_or(file.total_size);
                    if offset >= size {
                        return Ok(Vec::new());
                    }
                    let take = std::cmp::min(length, size - offset);
                    let take = u32::try_from(take).map_err(|_| {
                        DriveFsError::InvalidInput("Drive read request exceeds u32 range".into())
                    })?;
                    self.read_remote_range(file, offset, take)
                        .map_err(core_error)
                }
                Node::Pending(pending) => {
                    if offset >= pending.size {
                        return Ok(Vec::new());
                    }
                    let take = std::cmp::min(length, pending.size - offset);
                    let take = u32::try_from(take).map_err(|_| {
                        DriveFsError::InvalidInput("Drive read request exceeds u32 range".into())
                    })?;
                    if crate::commands::qa_feature_a::enabled() {
                        let local =
                            open_verified_qa_pending_file(&self.staging_root, pending, false)
                                .map_err(core_error)?;
                        let mut data = vec![0u8; take as usize];
                        let read = positioned_read(&local, &mut data, offset).map_err(|error| {
                            DriveFsError::Internal(format!("Drive QA staging read failed: {error}"))
                        })?;
                        data.truncate(read);
                        Ok(data)
                    } else {
                        self.read_pending_range(pending, offset, take)
                            .map_err(core_error)
                    }
                }
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn core_flush_pending(&self, pending: &DrivePendingRecord) -> Result<(), DriveFsError> {
            verified_pending_staging(&self.staging_root, pending).map_err(core_error)?;
            if !crate::commands::qa_feature_a::enabled() {
                let count = pending
                    .size
                    .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
                if count > 0 {
                    self.flush_completed_chunks(pending, 0, count - 1)
                        .map_err(core_error)?;
                }
            }
            sync_local_staging_for_pending(&self.staging_root, pending).map_err(core_error)
        }
    }

    impl DriveFsBackend for TeraRelayFs {
        fn revision(&self) -> Result<u64, DriveFsError> {
            let snapshot = self.snapshot().map_err(core_error)?;
            visible_snapshot_revision(&snapshot).map_err(core_error)
        }

        fn metadata(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            let node = self.core_node(id)?;
            if matches!(&node, Node::Remote(file) if file.storage_version==3) {
                return Err(DriveFsError::InvalidInput(
                    "This provider does not project symlinks as files".into(),
                ));
            }
            Ok(core_metadata(&node))
        }

        fn lookup(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            self.core_lookup_node(parent, name)
                .map(|node| core_metadata(&node))
        }

        fn list_directory(
            &self,
            parent: &DriveNodeId,
        ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
            let parent_id = core_parent_id(parent)?;
            let snapshot = self.snapshot().map_err(core_error)?;
            let same_parent = |candidate: Option<&str>| candidate == parent_id.as_deref();
            let replaced: std::collections::HashSet<&str> = snapshot
                .pending
                .iter()
                .filter_map(|pending| pending.base_entry_id.as_deref())
                .collect();
            let mut items = Vec::new();
            for directory in snapshot.directories {
                if same_parent(directory.parent_id.as_deref()) {
                    items.push(core_metadata(&Node::Directory(directory)));
                }
            }
            for file in snapshot.files {
                if same_parent(file.directory_id.as_deref())
                    && !replaced.contains(file.entry_id.as_str())
                {
                    items.push(core_metadata(&Node::Remote(file)));
                }
            }
            for pending in snapshot.pending {
                if same_parent(pending.directory_id.as_deref()) {
                    items.push(core_metadata(&Node::Pending(pending)));
                }
            }
            items.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(items)
        }

        fn read(
            &self,
            id: &DriveNodeId,
            offset: u64,
            length: u64,
        ) -> Result<Vec<u8>, DriveFsError> {
            let node = self.core_node(id)?;
            self.read_resolved_node(&node, offset, bounded_provider_read_length(length))
        }

        fn create_file(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            self.core_create_pending(parent, name)
                .map(|pending| core_metadata(&Node::Pending(pending)))
        }

        fn create_directory(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            require_missing_for_creation(self.core_lookup_node(parent, name))?;
            let parent_id = core_parent_id(parent)?;
            let directory = {
                let conn = self
                    .db
                    .lock()
                    .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                create_drive_directory(&conn, parent_id.as_deref(), name).map_err(core_error)?
            };
            Ok(core_metadata(&Node::Directory(directory)))
        }

        fn begin_write(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            match self.core_node(id)? {
                Node::Pending(pending) => Ok(core_metadata(&Node::Pending(pending))),
                Node::Remote(file) => self
                    .create_replacement_pending(&file)
                    .map(|pending| core_metadata(&Node::Pending(pending)))
                    .map_err(core_error),
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn write(&self, id: &DriveNodeId, offset: u64, data: &[u8]) -> Result<u64, DriveFsError> {
            let pending = match self.core_node(id)? {
                Node::Pending(pending) => pending,
                Node::Remote(file) => self.create_replacement_pending(&file).map_err(core_error)?,
                Node::Directory(_) | Node::Root => return Err(DriveFsError::IsDirectory),
            };
            if crate::commands::qa_feature_a::enabled() {
                let file = open_verified_qa_pending_file(&self.staging_root, &pending, true)
                    .map_err(core_error)?;
                let written = positioned_write(&file, data, offset)
                    .map_err(|error| DriveFsError::Internal(error.to_string()))?;
                let size = file
                    .metadata()
                    .map_err(|e| DriveFsError::Internal(e.to_string()))?
                    .len();
                let conn = self
                    .db
                    .lock()
                    .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                update_drive_pending_size(&conn, &pending.id, size).map_err(core_error)?;
                return Ok(written as u64);
            }
            let updated = self
                .write_pending_bytes(&pending, offset, data)
                .map_err(core_error)?;
            if !data.is_empty() {
                let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
                let first = offset / chunk_size;
                let last = offset.saturating_add(data.len().saturating_sub(1) as u64) / chunk_size;
                self.flush_completed_chunks(&updated, first, last)
                    .map_err(core_error)?;
            }
            Ok(data.len() as u64)
        }

        fn resize(&self, id: &DriveNodeId, size: u64) -> Result<(), DriveFsError> {
            match self.core_node(id)? {
                Node::Pending(pending) => {
                    self.resize_pending(&pending, size).map_err(core_error)?;
                    Ok(())
                }
                Node::Remote(file) => {
                    let visible = file.plaintext_size.unwrap_or(file.total_size);
                    if size == visible {
                        return Ok(());
                    }
                    let pending = self.create_replacement_pending(&file).map_err(core_error)?;
                    if let Err(error) = self.resize_pending(&pending, size) {
                        let _ = self.cancel_pending_sync(&pending, false);
                        return Err(core_error(error));
                    }
                    Ok(())
                }
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn flush(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            match self.core_node(id)? {
                Node::Pending(pending) => self.core_flush_pending(&pending),
                Node::Remote(_) => Ok(()),
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn close_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            let mut pending = match self.core_node(id)? {
                Node::Pending(pending) => pending,
                Node::Remote(_) => return Ok(()),
                Node::Directory(_) | Node::Root => return Err(DriveFsError::IsDirectory),
            };
            if crate::commands::qa_feature_a::enabled() {
                if pending.base_entry_id.is_some() {
                    // Copy-on-write QA edits are kept local, not enqueued.
                    let conn = self
                        .db
                        .lock()
                        .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                    mark_drive_pending_closed(&conn, &pending.id).map_err(core_error)?;
                    drop(conn);
                    if !self
                        .pending_has_content_changes(&pending)
                        .map_err(core_error)?
                    {
                        return self
                            .cancel_pending_sync(&pending, false)
                            .map_err(core_error);
                    }
                    self.mount_state.set_pending_active(&pending.id, false);
                    return Ok(());
                }
                pending = record_verified_qa_pending_size(&self.staging_root, &self.db, &pending)
                    .map_err(core_error)?;
                self.emit_upload_once(&pending);
                self.mount_state.set_pending_active(&pending.id, false);
                return Ok(());
            }
            if !self
                .pending_has_content_changes(&pending)
                .map_err(core_error)?
            {
                return self
                    .cancel_pending_sync(&pending, false)
                    .map_err(core_error);
            }
            let result = self.finalize_pending_sync(&pending.id).map_err(core_error);
            self.mount_state.set_pending_active(&pending.id, false);
            result
        }

        fn abort_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            match self.core_node(id)? {
                Node::Pending(pending) => self
                    .cancel_pending_sync(&pending, false)
                    .map_err(core_error),
                // The write may have finalized just before an OS-side error was
                // reported. Never delete a completed remote file during cleanup.
                Node::Remote(_) => Ok(()),
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn rename(
            &self,
            id: &DriveNodeId,
            new_parent: &DriveNodeId,
            new_name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            let node = self.core_node(id)?;
            let parent_id = core_parent_id(new_parent)?;
            {
                let conn = self
                    .db
                    .lock()
                    .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                match &node {
                    Node::Directory(directory) => {
                        rename_drive_directory(&conn, &directory.id, parent_id.as_deref(), new_name)
                    }
                    Node::Remote(file) => {
                        rename_drive_file(&conn, &file.entry_id, parent_id.as_deref(), new_name)
                    }
                    Node::Pending(file) => {
                        rename_drive_pending(&conn, &file.id, parent_id.as_deref(), new_name)
                    }
                    Node::Root => Err("Cannot rename the Drive root".to_string()),
                }
                .map_err(core_error)?;
            }
            self.core_lookup_node(new_parent, new_name)
                .map(|node| core_metadata(&node))
        }

        fn remove_file(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            match self.core_node(id)? {
                Node::Remote(file) => {
                    let conn = self
                        .db
                        .lock()
                        .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
                    trash_drive_file(&conn, &file.entry_id).map_err(core_error)
                }
                Node::Pending(pending) => {
                    self.cancel_pending_sync(&pending, true).map_err(core_error)
                }
                Node::Directory(_) | Node::Root => Err(DriveFsError::IsDirectory),
            }
        }

        fn remove_directory(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            let Node::Directory(directory) = self.core_node(id)? else {
                return Err(DriveFsError::NotDirectory);
            };
            let conn = self
                .db
                .lock()
                .map_err(|_| DriveFsError::Internal("DB poisoned".to_string()))?;
            trash_drive_directory(&conn, &directory.id).map_err(core_error)
        }

        fn recover_pending(&self) -> Result<DriveRecoverySummary, DriveFsError> {
            if crate::commands::qa_feature_a::enabled() {
                return Ok(DriveRecoverySummary {
                    recovered: 0,
                    remaining: 0,
                    failures: 0,
                });
            }
            let closed_ids: Vec<String> = self
                .snapshot()
                .map_err(core_error)?
                .pending
                .into_iter()
                .filter(|pending| pending.closed_at.is_some())
                .map(|pending| pending.id)
                .collect();
            let mut recovered = 0u32;
            let mut failures = 0u32;
            for pending_id in closed_ids {
                if self.mount_state.is_pending_active(&pending_id) {
                    continue;
                }
                self.mount_state.set_pending_active(&pending_id, true);
                match self.finalize_pending_sync(&pending_id) {
                    Ok(()) => recovered = recovered.saturating_add(1),
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        log::warn!("Deferred Drive recovery for {pending_id}: {error}");
                    }
                }
                self.mount_state.set_pending_active(&pending_id, false);
            }
            let remaining = self
                .snapshot()
                .map_err(core_error)?
                .pending
                .iter()
                .filter(|pending| pending.closed_at.is_some())
                .count() as u32;
            Ok(DriveRecoverySummary {
                recovered,
                remaining,
                failures,
            })
        }

        fn capacity(&self) -> DriveVirtualCapacity {
            let values = virtual_statfs_values();
            let total_bytes = values.blocks.saturating_mul(values.block_size as u64);
            let available_bytes = values
                .blocks_available
                .saturating_mul(values.block_size as u64);
            DriveVirtualCapacity {
                total_bytes,
                available_bytes,
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn read_retained_pending(
        pending: &DrivePendingRecord,
        ranges: &[(u64, u64)],
        whole_stage: bool,
        offset: u64,
        size: u32,
        read_base: impl FnOnce(u64, u32) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        let take = pending.size.saturating_sub(offset).min(size as u64) as usize;
        if take == 0 {
            return Ok(Vec::new());
        }
        if whole_stage {
            let file = open_private_read(Path::new(&pending.staging_path))?;
            let mut bytes = vec![0; take];
            if positioned_read(&file, &mut bytes, offset).map_err(|e| e.to_string())? != take {
                return Err("Retained Drive staging file is incomplete".into());
            }
            return Ok(bytes);
        }
        let mut bytes = read_base(offset, take as u32)?;
        bytes.resize(take, 0);
        overlay_ranges(&mut bytes, offset, ranges, |start, output| {
            let mut consumed = 0;
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            while consumed < output.len() {
                let absolute = start
                    .checked_add(consumed as u64)
                    .ok_or("Retained Drive offset overflow")?;
                let index = absolute / chunk_size;
                let local_offset = absolute % chunk_size;
                let length = (output.len() - consumed).min((chunk_size - local_offset) as usize);
                let file =
                    open_private_read(&crate::drive_stream::local_chunk_path(pending, index))?;
                let read = positioned_read(
                    &file,
                    &mut output[consumed..consumed + length],
                    local_offset,
                )
                .map_err(|e| e.to_string())?;
                if read != length {
                    return Err("Retained Drive dirty bytes are incomplete".into());
                }
                consumed += read;
            }
            Ok(consumed)
        })?;
        Ok(bytes)
    }

    #[cfg(target_os = "linux")]
    struct RetiredRead {
        node: Node,
        attr: FileAttr,
        symlink_target: Option<String>,
        ranges: Vec<(u64, u64)>,
        base: Option<DriveFileRecord>,
        locations: Option<FrozenV2Locations>,
        stage: Option<RetainedStage>,
        whole_stage: bool,
    }

    #[cfg(target_os = "linux")]
    impl RetiredRead {
        fn read(&self, backend: &TeraRelayFs, offset: u64, size: u32) -> Result<Vec<u8>, String> {
            self.read_with(offset, size, |file, start, take, locations| {
                backend.read_remote_range_with_locations(file, start, take, locations)
            })
        }

        fn read_with(
            &self,
            offset: u64,
            size: u32,
            mut read_remote: impl FnMut(
                &DriveFileRecord,
                u64,
                u32,
                Option<&FrozenV2Locations>,
            ) -> Result<Vec<u8>, String>,
        ) -> Result<Vec<u8>, String> {
            match &self.node {
                Node::Remote(file) => read_remote(file, offset, size, self.locations.as_ref()),
                Node::Pending(pending) => read_retained_pending(
                    pending,
                    &self.ranges,
                    self.whole_stage,
                    offset,
                    size,
                    |start, take| match &self.base {
                        Some(base) => read_remote(base, start, take, self.locations.as_ref()),
                        None => Ok(Vec::new()),
                    },
                ),
                _ => Err("Retained Drive node is not a readable file".into()),
            }
        }
    }

    /// fuser 0.16 dispatches callbacks serially. A blocking HTTP read must
    /// not monopolize that dispatcher while unrelated paths need lookup or
    /// getattr. Keep the number of remote workers and queued requests bounded.
    /// If saturated or disconnected, execute inline rather than losing a FUSE
    /// reply or manufacturing a short/failed read for the caller.
    #[cfg(target_os = "linux")]
    type FuseRemoteReadJob = Box<dyn FnOnce() + Send + 'static>;

    #[cfg(target_os = "linux")]
    struct FuseRemoteReadPool {
        sender: std::sync::mpsc::SyncSender<FuseRemoteReadJob>,
    }

    #[cfg(target_os = "linux")]
    impl FuseRemoteReadPool {
        fn new(workers: usize, queue: usize) -> Result<Self, String> {
            if workers == 0 || queue == 0 {
                return Err("Drive remote read pool requires positive bounds".into());
            }
            let (sender, receiver) = std::sync::mpsc::sync_channel::<FuseRemoteReadJob>(queue);
            let receiver = Arc::new(Mutex::new(receiver));
            for index in 0..workers {
                let receiver = Arc::clone(&receiver);
                std::thread::Builder::new()
                    .name(format!("terarelay-drive-read-{index}"))
                    .spawn(move || loop {
                        let task = match receiver.lock() {
                            Ok(receiver) => receiver.recv(),
                            Err(_) => return,
                        };
                        match task {
                            Ok(task) => task(),
                            Err(_) => return,
                        }
                    })
                    .map_err(|error| format!("Could not start Drive read worker: {error}"))?;
            }
            Ok(Self { sender })
        }

        fn dispatch(&self, job: impl FnOnce() + Send + 'static) {
            match self.sender.try_send(Box::new(job)) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(job)) => {
                    log::warn!("Drive remote read queue saturated; preserving read inline");
                    job();
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(job)) => {
                    log::warn!("Drive remote read workers unavailable; preserving read inline");
                    job();
                }
            }
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    mod fuse_remote_read_pool_tests {
        use super::FuseRemoteReadPool;
        use std::sync::{mpsc, Arc, Condvar, Mutex};
        use std::time::{Duration, Instant};

        #[test]
        fn bounded_fuse_remote_read_pool_keeps_metadata_dispatch_responsive() {
            let pool = FuseRemoteReadPool::new(2, 8).unwrap();
            let (started_tx, started_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            for _ in 0..2 {
                let gate = Arc::clone(&gate);
                let started_tx = started_tx.clone();
                pool.dispatch(move || {
                    started_tx.send(()).unwrap();
                    let (lock, notify) = &*gate;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        ready = notify.wait(ready).unwrap();
                    }
                });
            }
            for _ in 0..2 {
                started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            let before = Instant::now();
            pool.dispatch(move || finished_tx.send(()).unwrap());
            // Scheduling another HTTP read must not wait for either blocked
            // worker: the FUSE callback can immediately handle lookup/getattr.
            assert!(before.elapsed() < Duration::from_secs(1));
            assert!(finished_rx.try_recv().is_err());
            {
                let (lock, notify) = &*gate;
                *lock.lock().unwrap() = true;
                notify.notify_all();
            }
            finished_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }

        #[test]
        fn saturated_fuse_remote_read_pool_completes_every_reply_once() {
            let pool = FuseRemoteReadPool::new(1, 1).unwrap();
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let first_done = finished_tx.clone();
            pool.dispatch(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                first_done.send(1).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let second_done = finished_tx.clone();
            pool.dispatch(move || second_done.send(2).unwrap());
            // The bounded queue is full; this job runs inline rather than
            // discarding the FUSE reply or pretending its contents are empty.
            pool.dispatch(move || finished_tx.send(3).unwrap());
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(3)), Ok(3));
            release_tx.send(()).unwrap();
            let mut remaining = [
                finished_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                finished_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            ];
            remaining.sort_unstable();
            assert_eq!(remaining, [1, 2]);
            assert!(finished_rx.try_recv().is_err());
        }
    }

    #[cfg(target_os = "linux")]
    struct LinuxFuseAdapter {
        backend: Arc<TeraRelayFs>,
        remote_reads: FuseRemoteReadPool,
        generation: u64,
        lifetimes: InodeLifetimes<RetiredRead>,
    }

    #[cfg(target_os = "linux")]
    impl std::ops::Deref for LinuxFuseAdapter {
        type Target = TeraRelayFs;

        fn deref(&self) -> &Self::Target {
            self.backend.as_ref()
        }
    }

    #[cfg(target_os = "linux")]
    impl LinuxFuseAdapter {
        // A retired inode has an immutable identity even after the hot inode
        // index is cleared. Resolve that identity directly, without rebuilding
        // a project-wide snapshot on every held-descriptor read.
        fn indexed_regular_identity_for_getattr(
            &self,
            ino: u64,
        ) -> Result<Option<DriveNodeId>, String> {
            if self.lifetimes.retired(ino).is_some() {
                return Ok(None);
            }
            let index = self
                .backend
                .inode_index
                .lock()
                .map_err(|_| "Drive inode index poisoned".to_string())?;
            let Some(entry) = index.get(&ino) else {
                return Ok(None);
            };
            if entry.kind != FileType::RegularFile {
                return Ok(None);
            }
            match &entry.identity {
                DriveNodeId::File(_) | DriveNodeId::Pending(_) => Ok(Some(entry.identity.clone())),
                DriveNodeId::Root | DriveNodeId::Directory(_) => Ok(None),
            }
        }

        fn live_node_for_inode(&self, ino: u64) -> Result<Option<Node>, String> {
            if let Some(view) = self.lifetimes.retired(ino) {
                return match self
                    .backend
                    .core_node(&TeraRelayFs::inode_identity(&view.node))
                {
                    Ok(node) => Ok(Some(node)),
                    Err(DriveFsError::NotFound) => Ok(None),
                    Err(error) => Err(error.to_string()),
                };
            }
            let identity = {
                let index = self
                    .backend
                    .inode_index
                    .lock()
                    .map_err(|_| "Drive inode index poisoned".to_string())?;
                indexed_regular_file_identity(&index, ino)
            };
            if let Some(identity) = identity {
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                return TeraRelayFs::indexed_regular_node_locked(&conn, &identity);
            }
            self.backend.node_for_inode(ino)
        }

        // FUSE supplies parent inodes from live lookups; the rename transaction
        // revalidates the destination directory before applying any mutation.
        fn directory_id_for_inode_fast(&self, ino: u64) -> Result<Option<Option<String>>, String> {
            let indexed = {
                let index = self
                    .backend
                    .inode_index
                    .lock()
                    .map_err(|_| "Drive inode index poisoned".to_string())?;
                indexed_directory_id(&index, &self.lifetimes, ino)
            };
            match indexed {
                Some(id) => Ok(Some(id)),
                None => self.backend.directory_id_for_inode(ino),
            }
        }

        fn reply_directory_plus_listing(
            &mut self,
            ino: u64,
            offset: i64,
            mut reply: ReplyDirectoryPlus,
            listing: IndexedDirectoryPlusListing,
        ) {
            let start = match checked_fuse_directory_offset(offset) {
                Ok(start) => start,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
            let IndexedDirectoryPlusListing {
                node,
                parent_inode,
                current_attr,
                parent_attr,
                entries,
            } = listing;
            {
                let _index_phase = SlowFusePhase::new("readdirplus-inode-index");
                let mut index = match self.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                let parent_entry = match &node {
                    Node::Directory(directory) => directory.parent_id.as_ref().map(|id| {
                        (
                            parent_inode,
                            IndexedInode {
                                identity: DriveNodeId::Directory(id.clone()),
                                kind: FileType::Directory,
                            },
                        )
                    }),
                    _ => None,
                };
                let child_entries = entries.iter().map(|child| {
                    (
                        child.ino,
                        IndexedInode {
                            identity: child.identity.clone(),
                            kind: child.kind,
                        },
                    )
                });
                if let Err(error) = checked_inode_index_batch(
                    &mut index,
                    parent_entry.into_iter().chain(child_entries),
                ) {
                    log::warn!("Drive directory-plus refused conflicting FUSE inode: {error}");
                    reply.error(EIO);
                    return;
                }
            }

            let total = entries.len() + 2;
            for index in start..total {
                let full = match index {
                    0 => reply.add(ino, 1, ".", &TTL, &current_attr, 0),
                    1 => reply.add(parent_inode, 2, "..", &TTL, &parent_attr, 0),
                    _ => {
                        let child = &entries[index - 2];
                        debug_assert_eq!(child.kind, child.attr.kind);
                        let full = reply.add(
                            child.ino,
                            (index + 1) as i64,
                            &child.name,
                            &TTL,
                            &child.attr,
                            0,
                        );
                        if !full {
                            self.lifetimes.lookup(child.ino);
                        }
                        full
                    }
                };
                if full {
                    break;
                }
            }
            reply.ok();
        }

        fn lookup_child(&self, parent: u64, name: &OsStr) -> Result<Option<Node>, String> {
            let Some(name) = name.to_str() else {
                return Ok(None);
            };
            let Some(parent_id) = self.directory_id_for_inode_fast(parent)? else {
                return Ok(None);
            };
            let node = {
                let started = Instant::now();
                let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
                report_slow_drive_db_wait("lookup-child", started);
                let _sql_phase = SlowFusePhase::new("lookup-child-sql");
                lookup_child_node_locked(&conn, parent_id.as_deref(), name)?
            };
            // A directory lookup must not hold the DB mutex while waiting
            // for the inode-index mutex. In particular, readdir and getattr
            // should be able to take a short SQLite read independently of
            // an unrelated concurrent index update.
            if let Some(node) = node {
                let _index_phase = SlowFusePhase::new("lookup-child-inode-index");
                self.remember_node(&node)?;
                Ok(Some(node))
            } else {
                Ok(None)
            }
        }

        fn lookup_child_identity(
            &self,
            parent: u64,
            name: &OsStr,
        ) -> Result<Option<(DriveNodeId, bool)>, String> {
            let Some(name) = name.to_str() else {
                return Ok(None);
            };
            let Some(parent_id) = self.directory_id_for_inode_fast(parent)? else {
                return Ok(None);
            };
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            lookup_child_identity_locked(&conn, parent_id.as_deref(), name)
        }

        fn node_for_inode(&self, ino: u64) -> Result<Option<Node>, String> {
            match self.live_node_for_inode(ino)? {
                Some(node) => Ok(Some(node)),
                None => Ok(self.lifetimes.retired(ino).map(|view| view.node.clone())),
            }
        }

        fn capture_retired(
            &self,
            conn: &sqlite::Connection,
            node: Node,
        ) -> Result<RetiredRead, String> {
            if self.lifetimes.retired(node.inode()).is_some() {
                return Err("Destination has an earlier retained reader".into());
            }
            let mut attr = Self::attributes_locked(conn, &node)?;
            attr.nlink = 0;
            let symlink_target = capture_symlink_target(conn, &node)?;
            let mut view = RetiredRead {
                node,
                attr,
                symlink_target,
                ranges: Vec::new(),
                base: None,
                locations: None,
                stage: None,
                whole_stage: crate::commands::qa_feature_a::enabled(),
            };
            let remote = match &view.node {
                Node::Remote(file) => Some(file.clone()),
                Node::Pending(pending) => {
                    if pending.closed_at.is_none()
                        || self.mount_state.is_pending_active(&pending.id)
                        || !drive_pending_chunks(conn, &pending.id)?.is_empty()
                        || !drive_pending_parts(conn, &pending.id)?.is_empty()
                    {
                        return Err("Destination has an active or uploaded pending write".into());
                    }
                    view.ranges = drive_pending_ranges(conn, &pending.id)?;
                    if let Some(id) = pending.base_entry_id.as_deref() {
                        view.base = Some(
                            drive_file_by_entry_id(conn, id)?
                                .ok_or("Retained Drive base file is missing")?,
                        );
                    }
                    view.stage = Some(RetainedStage::prepare(
                        &self.staging_root,
                        &pending.id,
                        Path::new(&pending.staging_path),
                    )?);
                    view.base.clone()
                }
                _ => None,
            };
            if let Some(file) = remote {
                if file.storage_version == 2 {
                    view.locations = Some(FrozenV2Locations::capture(conn, &file.file_id)?);
                }
            }
            Ok(view)
        }

        fn restore_native_entry(
            &self,
            req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            kind: crate::drive_restore::RestoreKind,
            target: Option<&str>,
        ) -> Option<Result<FileAttr, String>> {
            use crate::drive_restore::{self, RestoreKind};
            match self.mount_state.inner.native_restores.contains(req.pid()) {
                Ok(false) => return None,
                Err(error) => return Some(Err(error)),
                Ok(true) => {}
            }
            let Some(name) = name.to_str() else {
                return Some(Err("Invalid native restore name".into()));
            };
            let parent_id = match self.directory_id_for_inode(parent) {
                Ok(Some(id)) => id,
                _ => return Some(Err("Native restore parent is unavailable".into())),
            };
            let request = match self.mount_state.inner.native_restores.take(
                req.pid(),
                self.generation,
                parent_id.as_deref(),
                name,
                kind,
                target,
            )? {
                Ok(request) => request,
                Err(error) => return Some(Err(error)),
            };
            let result = (|| {
                // Sign-out/unmount cannot invalidate ownership halfway through
                // the metadata transaction. No native syscall runs under these locks.
                let session = self
                    .mount_state
                    .inner
                    .session
                    .lock()
                    .map_err(|_| "Drive session poisoned")?;
                if session.generation != self.generation
                    || session.cancelled
                    || !session.worker_running
                    || !self.mount_state.inner.mounted.load(Ordering::SeqCst)
                {
                    return Err("Native restore mount was cancelled or superseded".to_string());
                }
                let conn = self.db.lock().map_err(|_| "DB poisoned")?;
                conn.execute("SAVEPOINT native_restore_materialize")
                    .map_err(|e| e.to_string())?;
                let result = (|| {
                    drive_restore::commit(&conn, &request.plan)?;
                    let node = match kind {
                        RestoreKind::Folder => Node::Directory(
                            crate::drive_lookup::directory(&conn, &request.plan.id)?
                                .ok_or("Restored Drive folder is unavailable")?,
                        ),
                        RestoreKind::File => Node::Remote(
                            crate::drive_lookup::file(&conn, &request.plan.id)?
                                .ok_or("Restored Drive file is unavailable")?,
                        ),
                        RestoreKind::Symlink => symlink_node(
                            &crate::drive_posix::symlink_by_id(&conn, &request.plan.id)?
                                .ok_or("Restored Drive symlink is unavailable")?,
                        ),
                    };
                    let attr = Self::attributes_locked(&conn, &node)?;
                    self.remember_node(&node)?;
                    Ok(attr)
                })();
                crate::drive_posix::finish_savepoint(&conn, "native_restore_materialize", result)
            })();
            request.complete(
                result
                    .as_ref()
                    .map(|_| request.plan.name.clone())
                    .map_err(Clone::clone),
            );
            Some(result)
        }

        fn attributes(&self, node: &Node) -> Result<FileAttr, String> {
            if let Some(view) = self.lifetimes.retired(node.inode()) {
                if self.live_node_for_inode(node.inode())?.is_none() {
                    return Ok(view.attr);
                }
            }
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            Self::attributes_locked(&conn, node)
        }
        fn attributes_locked(conn: &sqlite::Connection, node: &Node) -> Result<FileAttr, String> {
            let mut attr = attr_for(node);
            match node {
                Node::Root => attr.nlink = directory_link_count(conn, None)?,
                Node::Directory(dir) => {
                    attr.nlink = directory_link_count(conn, Some(&dir.id))?;
                }
                _ => {}
            }
            let mode = match node {
                Node::Directory(dir) => crate::drive_posix::permission(conn, "directory", &dir.id)?,
                Node::Remote(file) if file.storage_version != 3 => {
                    crate::drive_posix::permission(conn, "file", &file.entry_id)?
                }
                Node::Pending(file) => {
                    let mode = crate::drive_posix::permission(conn, "pending", &file.id)?;
                    match (mode, file.base_entry_id.as_deref()) {
                        (None, Some(base)) => crate::drive_posix::permission(conn, "file", base)?,
                        _ => mode,
                    }
                }
                _ => None,
            };
            if let Some(mode) = mode {
                attr.perm = mode;
            }
            Ok(attr)
        }
        fn reply_entry(&mut self, node: &Node, reply: ReplyEntry) {
            match self.attributes(node) {
                Ok(attr) => {
                    self.lifetimes.lookup(attr.ino);
                    reply.entry(&TTL, &attr, 0);
                }
                Err(_) => reply.error(EIO),
            }
        }
        fn reply_attr(&self, node: &Node, reply: ReplyAttr) {
            match self.attributes(node) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(_) => reply.error(EIO),
            }
        }
        fn permission_change(&self, node: &Node, mode: u32) -> Result<(), String> {
            let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
            match node {
                Node::Directory(dir) => {
                    crate::drive_posix::set_permission(&conn, "directory", &dir.id, mode)
                }
                Node::Remote(file) if file.storage_version != 3 => {
                    crate::drive_posix::set_permission(&conn, "file", &file.entry_id, mode)
                }
                Node::Pending(file) => {
                    crate::drive_posix::set_permission(&conn, "pending", &file.id, mode)
                }
                _ => Err("Permission changes are unsupported for this node".into()),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Filesystem for LinuxFuseAdapter {
        fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
            self.lifetimes.forget(ino, nlookup);
        }

        fn getxattr(
            &mut self,
            req: &Request<'_>,
            ino: u64,
            name: &OsStr,
            size: u32,
            reply: fuser::ReplyXattr,
        ) {
            if ino != ROOT_INODE || name != OsStr::new(crate::drive_restore::IDENTITY_XATTR) {
                reply.error(libc::ENODATA);
                return;
            }
            let value = format!(
                "{:032x}:{}:{}",
                self.mount_state.inner.mount_identity,
                self.generation,
                req.pid()
            );
            if size == 0 {
                reply.size(value.len() as u32);
            } else if (size as usize) < value.len() {
                reply.error(libc::ERANGE);
            } else {
                reply.data(value.as_bytes());
            }
        }

        fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
            let names = listable_drive_xattrs(ino);
            if size == 0 {
                reply.size(names.len() as u32);
            } else if (size as usize) < names.len() {
                reply.error(libc::ERANGE);
            } else {
                reply.data(&names);
            }
        }

        fn mknod(
            &mut self,
            req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            mode: u32,
            _umask: u32,
            _rdev: u32,
            reply: ReplyEntry,
        ) {
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            if mode & libc::S_IFMT != libc::S_IFREG {
                reply.error(libc::ENOSYS);
                return;
            }
            match self.restore_native_entry(
                req,
                parent,
                name,
                crate::drive_restore::RestoreKind::File,
                None,
            ) {
                Some(Ok(attr)) => {
                    self.lifetimes.lookup(attr.ino);
                    reply.entry(&TTL, &attr, 0);
                }
                Some(Err(error)) => {
                    log::warn!("Drive native file restore failed: {error}");
                    reply.error(EIO);
                }
                None => reply.error(libc::ENOSYS),
            }
        }

        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let _slow = SlowFuseRequest::new("lookup");
            match self.lookup_child(parent, name) {
                Ok(Some(node)) => self.reply_entry(&node, reply),
                Ok(None) => reply.error(ENOENT),
                Err(error) => {
                    log::warn!("Drive lookup failed: {}", error);
                    reply.error(EIO);
                }
            }
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
            let _slow = SlowFuseRequest::new("getattr");
            match self.indexed_regular_identity_for_getattr(ino) {
                Ok(Some(identity)) => {
                    let started = Instant::now();
                    let result = self
                        .db
                        .lock()
                        .map_err(|_| "DB poisoned".to_string())
                        .and_then(|conn| {
                            report_slow_drive_db_wait("getattr-indexed", started);
                            let _query_phase = SlowFusePhase::new("getattr-indexed-sql");
                            TeraRelayFs::indexed_regular_attr_locked(&conn, &identity)
                        });
                    match result {
                        Ok(Some(attr)) => reply.attr(&TTL, &attr),
                        Ok(None) => reply.error(ENOENT),
                        Err(error) => {
                            log::warn!("Drive indexed getattr failed: {}", error);
                            reply.error(EIO);
                        }
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!("Drive inode index lookup failed for getattr: {}", error);
                    reply.error(EIO);
                    return;
                }
            }
            match self.node_for_inode(ino) {
                Ok(Some(node)) => self.reply_attr(&node, reply),
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
            let _slow = SlowFuseRequest::new("readdir");
            let start = match checked_fuse_directory_offset(offset) {
                Ok(start) => start,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
            let indexed_directory = {
                let index = match self.backend.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                indexed_directory_id(&index, &self.lifetimes, ino)
            };
            let directory_id = match indexed_directory {
                Some(id) => id,
                None => {
                    let node = match self.node_for_inode(ino) {
                        Ok(Some(node)) => node,
                        Ok(None) => {
                            reply.error(ENOENT);
                            return;
                        }
                        Err(error) => {
                            log::warn!("Drive readdir inode lookup failed: {error}");
                            reply.error(EIO);
                            return;
                        }
                    };
                    match node {
                        Node::Root => None,
                        Node::Directory(directory) => Some(directory.id),
                        _ => {
                            reply.error(ENOTDIR);
                            return;
                        }
                    }
                }
            };
            let (parent_id, children) = {
                let lock_started = Instant::now();
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                report_slow_drive_db_wait("readdir", lock_started);
                let _query_phase = SlowFusePhase::new("readdir-sql");
                match indexed_directory_listing(&conn, directory_id.as_deref()) {
                    Ok(Some(listing)) => listing,
                    Ok(None) => {
                        reply.error(ENOENT);
                        return;
                    }
                    Err(error) => {
                        log::warn!("Drive readdir failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                }
            };
            let parent_inode = parent_id
                .as_deref()
                .map(|id| inode_for("dir", id))
                .unwrap_or(ROOT_INODE);
            {
                let _index_phase = SlowFusePhase::new("readdir-inode-index");
                let mut index = match self.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                let parent_entries = parent_id.as_ref().map(|id| {
                    (
                        parent_inode,
                        IndexedInode {
                            identity: DriveNodeId::Directory(id.clone()),
                            kind: FileType::Directory,
                        },
                    )
                });
                let children_entries = children.iter().map(|child| {
                    (
                        child.ino,
                        IndexedInode {
                            identity: child.identity.clone(),
                            kind: child.kind,
                        },
                    )
                });
                if let Err(error) = checked_inode_index_batch(
                    &mut index,
                    parent_entries.into_iter().chain(children_entries),
                ) {
                    log::warn!("Drive readdir refused conflicting FUSE inode: {error}");
                    reply.error(EIO);
                    return;
                }
            }

            let mut entries: Vec<(u64, FileType, String)> = Vec::with_capacity(children.len() + 2);
            entries.push((ino, FileType::Directory, ".".to_string()));
            entries.push((parent_inode, FileType::Directory, "..".to_string()));
            entries.extend(
                children
                    .into_iter()
                    .map(|entry| (entry.ino, entry.kind, entry.name)),
            );
            for (index, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(start) {
                if reply.add(child_ino, (index + 1) as i64, kind, name) {
                    break;
                }
            }
            reply.ok();
        }

        fn readdirplus(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectoryPlus,
        ) {
            let _slow = SlowFuseRequest::new("readdirplus");
            let start = match checked_fuse_directory_offset(offset) {
                Ok(start) => start,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
            let indexed_directory = {
                let index = match self.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                indexed_directory_id(&index, &self.lifetimes, ino)
            };
            if let Some(current_id) = indexed_directory {
                let lock_started = Instant::now();
                let listing = self
                    .db
                    .lock()
                    .map_err(|_| "DB poisoned".to_string())
                    .and_then(|conn| {
                        report_slow_drive_db_wait("readdirplus", lock_started);
                        let _query_phase = SlowFusePhase::new("readdirplus-sql");
                        indexed_directory_plus_listing(&conn, current_id.as_deref())
                    });
                match listing {
                    Ok(Some(listing)) => {
                        self.reply_directory_plus_listing(ino, offset, reply, listing);
                        return;
                    }
                    Ok(None) => {
                        reply.error(ENOENT);
                        return;
                    }
                    Err(error) => {
                        log::warn!("Drive indexed readdirplus failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                }
            }

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
                Err(error) => {
                    log::warn!("Drive readdirplus inode lookup failed: {error}");
                    reply.error(EIO);
                    return;
                }
            };
            let parent_id = match &node {
                Node::Root => None,
                Node::Directory(directory) => Some(directory.id.as_str()),
                _ => unreachable!(),
            };
            let (parent_inode, current_attr, parent_attr, children) = {
                let lock_started = Instant::now();
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                report_slow_drive_db_wait("readdirplus-fallback", lock_started);
                let _query_phase = SlowFusePhase::new("readdirplus-fallback-sql");
                let current_attr = match Self::attributes_locked(&conn, &node) {
                    Ok(attr) => attr,
                    Err(error) => {
                        log::warn!("Drive readdirplus current attributes failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                };
                let (parent_inode, parent_attr) = match &node {
                    Node::Root => (ROOT_INODE, attr_for(&Node::Root)),
                    Node::Directory(directory) => match directory.parent_id.as_ref() {
                        Some(parent_id) => {
                            let parent_node = match TeraRelayFs::core_node_locked(
                                &conn,
                                &DriveNodeId::Directory(parent_id.clone()),
                            ) {
                                Ok(parent) => parent,
                                Err(error) => {
                                    log::warn!("Drive readdirplus parent lookup failed: {error}");
                                    reply.error(EIO);
                                    return;
                                }
                            };
                            let parent_attr = match Self::attributes_locked(&conn, &parent_node) {
                                Ok(attr) => attr,
                                Err(error) => {
                                    log::warn!(
                                        "Drive readdirplus parent attributes failed: {error}"
                                    );
                                    reply.error(EIO);
                                    return;
                                }
                            };
                            (parent_attr.ino, parent_attr)
                        }
                        None => (ROOT_INODE, attr_for(&Node::Root)),
                    },
                    _ => unreachable!(),
                };
                let children = match indexed_directory_plus_entries(&conn, parent_id) {
                    Ok(entries) => entries,
                    Err(error) => {
                        log::warn!("Drive readdirplus failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                };
                (parent_inode, current_attr, parent_attr, children)
            };
            {
                let _index_phase = SlowFusePhase::new("readdirplus-fallback-inode-index");
                let mut index = match self.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                let parent_entry = match &node {
                    Node::Directory(directory) => directory.parent_id.as_ref().map(|id| {
                        (
                            parent_inode,
                            IndexedInode {
                                identity: DriveNodeId::Directory(id.clone()),
                                kind: FileType::Directory,
                            },
                        )
                    }),
                    _ => None,
                };
                let child_entries = children.iter().map(|child| {
                    (
                        child.ino,
                        IndexedInode {
                            identity: child.identity.clone(),
                            kind: child.kind,
                        },
                    )
                });
                if let Err(error) = checked_inode_index_batch(
                    &mut index,
                    parent_entry.into_iter().chain(child_entries),
                ) {
                    log::warn!("Drive readdirplus refused conflicting FUSE inode: {error}");
                    reply.error(EIO);
                    return;
                }
            }

            let total = children.len() + 2;
            for index in start..total {
                let full = match index {
                    0 => reply.add(ino, 1, ".", &TTL, &current_attr, 0),
                    1 => reply.add(parent_inode, 2, "..", &TTL, &parent_attr, 0),
                    _ => {
                        let child = &children[index - 2];
                        debug_assert_eq!(child.kind, child.attr.kind);
                        let full = reply.add(
                            child.ino,
                            (index + 1) as i64,
                            &child.name,
                            &TTL,
                            &child.attr,
                            0,
                        );
                        if !full {
                            self.lifetimes.lookup(child.ino);
                        }
                        full
                    }
                };
                if full {
                    break;
                }
            }
            reply.ok();
        }

        fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
            let _slow = SlowFuseRequest::new("open");
            if self.mount_state.safe_mount_read_only() && safe_mode::writable_open(flags) {
                reply.error(EROFS);
                return;
            }
            if flags & libc::O_ACCMODE == libc::O_RDONLY {
                let indexed_kind =
                    self.inode_index.lock().ok().and_then(|index| {
                        indexed_read_only_open_kind(&index, &self.lifetimes, ino)
                    });
                match indexed_kind {
                    Some(FileType::Directory) => {
                        reply.error(EISDIR);
                        return;
                    }
                    Some(FileType::Symlink) => {
                        reply.error(libc::ELOOP);
                        return;
                    }
                    Some(FileType::RegularFile) => {
                        self.lifetimes.open(ino);
                        reply.opened(0, 0);
                        return;
                    }
                    _ => {}
                }
            }
            if flags & libc::O_ACCMODE != libc::O_RDONLY && self.lifetimes.retired(ino).is_some() {
                match self.live_node_for_inode(ino) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        reply.error(ENOENT);
                        return;
                    }
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                }
            }
            match self.node_for_inode(ino) {
                Ok(Some(Node::Directory(_))) | Ok(Some(Node::Root)) => reply.error(EISDIR),
                Ok(Some(Node::Remote(file))) => {
                    if file.storage_version == 3 {
                        reply.error(libc::ELOOP);
                        return;
                    }
                    if flags & libc::O_ACCMODE == libc::O_RDONLY {
                        self.lifetimes.open(ino);
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
                    if native_open_requests_truncate(flags) {
                        if let Err(error) = self.resize_pending(&pending, 0) {
                            self.mount_state.set_pending_active(&pending.id, false);
                            let deleted_path = match self.db.lock() {
                                Ok(conn) => match delete_drive_pending(&conn, &pending.id) {
                                    Ok(path) => path,
                                    Err(db_error) => {
                                        log::warn!("Could not remove failed Drive replacement metadata: {db_error}");
                                        None
                                    }
                                },
                                Err(_) => {
                                    log::warn!("Could not lock Drive database for failed replacement cleanup");
                                    None
                                }
                            };
                            self.cleanup_removed_pending_stage(&pending, deleted_path.as_deref());
                            log::warn!("Drive truncate-on-open failed: {}", error);
                            reply.error(EIO);
                            return;
                        }
                    }
                    self.writer_opened(ino);
                    self.lifetimes.open(ino);
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
                        // Atomically exclude native automatic recovery before
                        // touching even a truncation of this pending stage.
                        let Some(already_active) =
                            self.mount_state.try_open_pending_writer(&pending.id)
                        else {
                            reply.error(libc::EBUSY);
                            return;
                        };
                        // A failed truncation must not unlock another writer's
                        // existing stage ownership.
                        if native_open_requests_truncate(flags) {
                            if let Err(error) = self.resize_pending(&pending, 0) {
                                if !already_active {
                                    self.mount_state.set_pending_active(&pending.id, false);
                                }
                                log::warn!("Drive pending truncate-on-open failed: {error}");
                                reply.error(EIO);
                                return;
                            }
                        }
                        self.writer_opened(ino);
                    }
                    self.lifetimes.open(ino);
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
            let _slow = SlowFuseRequest::new("read");
            if offset < 0 {
                reply.error(EINVAL);
                return;
            }
            let node = match self.live_node_for_inode(ino) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    if let Some(view) = self.lifetimes.retired(ino) {
                        match view.read(&self.backend, offset as u64, size) {
                            Ok(data) => reply.data(&data),
                            Err(error) => {
                                log::warn!("Retained Drive read failed: {error}");
                                reply.error(EIO);
                            }
                        }
                    } else {
                        reply.error(ENOENT);
                    }
                    return;
                }
                Err(error) => {
                    log::warn!("Drive inode lookup failed before read: {error}");
                    reply.error(EIO);
                    return;
                }
            };
            if matches!(&node, Node::Remote(_)) {
                // No inode index or SQLite mutex crosses this boundary.
                // Authenticated HTTP can block for seconds: complete the FUSE
                // reply on a bounded worker, leaving the serial dispatcher
                // free for unrelated lookup, getattr and readdir requests.
                let backend = Arc::clone(&self.backend);
                self.remote_reads.dispatch(move || {
                    let _slow = SlowFuseRequest::new("remote-read-worker");
                    match backend.read_resolved_node(&node, offset as u64, size as u64) {
                        Ok(data) => reply.data(&data),
                        Err(error) => {
                            log::warn!("TeraRelay Drive remote read failed: {error}");
                            reply.error(errno_for_core(&error));
                        }
                    }
                });
                return;
            }
            // Pending and retained reads retain their established staging
            // lifetime rules until those paths can be offloaded separately.
            match self
                .backend
                .read_resolved_node(&node, offset as u64, size as u64)
            {
                Ok(data) => reply.data(&data),
                Err(error) => {
                    log::warn!("TeraRelay Drive resolved read failed: {error}");
                    reply.error(errno_for_core(&error));
                }
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
            let _slow = SlowFuseRequest::new("create");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            let Some(name) = name.to_str() else {
                reply.error(EIO);
                return;
            };
            if let Err(errno) =
                require_missing_fuse_child(self.lookup_child(parent, OsStr::new(name)))
            {
                reply.error(errno);
                return;
            }
            let directory_id =
                match require_fuse_directory_parent(self.directory_id_for_inode(parent)) {
                    Ok(directory_id) => directory_id,
                    Err(errno) => {
                        reply.error(errno);
                        return;
                    }
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
            let staging_dir = match create_owned_staging_directory(&self.staging_root, &pending_id)
            {
                Ok(path) => path,
                Err(error) => {
                    log::warn!("Drive staging directory create failed: {error}");
                    reply.error(EIO);
                    return;
                }
            };
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
                        let _ = std::fs::remove_dir(&staging_dir);
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
                    Ok(record) => {
                        if let Err(error) = crate::drive_posix::set_permission(
                            &conn,
                            "pending",
                            &record.id,
                            (_mode & !_umask) & 0o777,
                        ) {
                            if delete_drive_pending(&conn, &record.id).is_ok() {
                                let _ = std::fs::remove_dir(&staging_dir);
                            }
                            log::warn!("Drive create mode failed: {error}");
                            reply.error(EIO);
                            return;
                        }
                        record
                    }
                    Err(error) => {
                        let _ = std::fs::remove_dir(&staging_dir);
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
                    if delete_drive_pending(&conn, &pending.id).is_ok() {
                        let _ = std::fs::remove_dir(&staging_dir);
                    }
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
            match self.attributes(&node) {
                Ok(attr) => {
                    self.lifetimes.lookup(attr.ino);
                    self.lifetimes.open(attr.ino);
                    reply.created(&TTL, &attr, 0, created_fuse_handle(flags), 0);
                }
                Err(_) => reply.error(EIO),
            };
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
            let _slow = SlowFuseRequest::new("write");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            let end = match checked_fuse_write_end(offset, data.len()) {
                Ok(end) => end,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
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
            if data.is_empty() {
                reply.written(0);
                return;
            }
            if crate::commands::qa_feature_a::enabled() {
                if let Err(error) = verified_pending_staging(&self.backend.staging_root, &pending) {
                    log::warn!("QA Drive write staging validation failed: {error}");
                    reply.error(EIO);
                    return;
                }
                let file = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
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
                    let last = (end - 1) / chunk_size;
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
            let _slow = SlowFuseRequest::new("flush");
            match self.pending_for_inode(ino) {
                Ok(Some(pending)) => {
                    if let Err(error) =
                        verified_pending_staging(&self.backend.staging_root, &pending)
                    {
                        log::warn!("Drive flush staging validation failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                    if self.mount_state.safe_mount_read_only() {
                        match sync_local_staging_for_pending(&self.backend.staging_root, &pending) {
                            Ok(()) => reply.ok(),
                            Err(error) => {
                                log::warn!("Drive safe-mode local flush failed: {error}");
                                reply.error(EIO);
                            }
                        }
                        return;
                    }
                    if crate::commands::qa_feature_a::enabled() {
                        match sync_local_staging_for_pending(&self.backend.staging_root, &pending) {
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
                    if let Err(error) =
                        sync_local_staging_for_pending(&self.backend.staging_root, &pending)
                    {
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
            let _slow = SlowFuseRequest::new("fsync");
            match self.pending_for_inode(ino) {
                Ok(Some(pending)) => {
                    if let Err(error) =
                        verified_pending_staging(&self.backend.staging_root, &pending)
                    {
                        log::warn!("Drive fsync staging validation failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                    if self.mount_state.safe_mount_read_only() {
                        match sync_local_staging_for_pending(&self.backend.staging_root, &pending) {
                            Ok(()) => reply.ok(),
                            Err(error) => {
                                log::warn!("Drive safe-mode local sync failed: {error}");
                                reply.error(EIO);
                            }
                        }
                        return;
                    }
                    if crate::commands::qa_feature_a::enabled() {
                        match sync_local_staging_for_pending(&self.backend.staging_root, &pending) {
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
                    if let Err(error) =
                        sync_local_staging_for_pending(&self.backend.staging_root, &pending)
                    {
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
            fh: u64,
            flags: i32,
            _lock_owner: Option<u64>,
            _flush: bool,
            reply: ReplyEmpty,
        ) {
            let _slow = SlowFuseRequest::new("release");
            self.lifetimes.close(ino);
            // Only the initial read-only CREATE handle owns a writer.
            if !release_closes_pending_writer(flags, fh) {
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
                        if let Err(error) =
                            verified_pending_staging(&self.backend.staging_root, &pending)
                        {
                            log::warn!("QA Drive release staging validation failed: {error}");
                            reply.error(EIO);
                            return;
                        }
                        if pending.base_entry_id.is_some() {
                            // Replacement edits remain local in QA, unlike
                            // new files whose size+closed marker is atomic.
                            let closed = self
                                .db
                                .lock()
                                .map_err(|_| "DB poisoned".to_string())
                                .and_then(|conn| mark_drive_pending_closed(&conn, &pending.id));
                            if let Err(error) = closed {
                                log::error!("QA Drive could not record closed staging: {}", error);
                                reply.error(EIO);
                                return;
                            }
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

                        pending = match record_verified_qa_pending_size(
                            &self.backend.staging_root,
                            &self.db,
                            &pending,
                        ) {
                            Ok(updated) => updated,
                            Err(error) => {
                                log::error!("QA Drive could not verify queued file size: {error}");
                                reply.error(EIO);
                                return;
                            }
                        };
                        if !self.mount_state.safe_mount_read_only() {
                            self.emit_upload_once(&pending);
                        }
                        self.mount_state.set_pending_active(&pending.id, false);
                        reply.ok();
                        return;
                    }

                    if self.mount_state.safe_mount_read_only() {
                        // A writer opened before Safe Mode was enabled must
                        // still close cleanly. Validate ownership and fsync
                        // its private stage before it becomes retryable. Never
                        // begin a Telegram finalize while protected.
                        let retained =
                            verified_pending_staging(&self.backend.staging_root, &pending)
                                .and_then(|_| {
                                    sync_local_staging_for_pending(
                                        &self.backend.staging_root,
                                        &pending,
                                    )
                                });
                        if let Err(error) = retained {
                            self.mount_state.set_pending_active(&pending.id, false);
                            log::warn!(
                                "Drive safe-mode close could not retain staged bytes: {error}"
                            );
                            reply.error(EIO);
                            return;
                        }
                        let closed = self
                            .db
                            .lock()
                            .map_err(|_| "DB poisoned".to_string())
                            .and_then(|conn| mark_drive_pending_closed(&conn, &pending.id));
                        self.mount_state.set_pending_active(&pending.id, false);
                        match closed {
                            Ok(_) => reply.ok(),
                            Err(error) => {
                                log::warn!(
                                    "Drive safe-mode close could not record pending stage: {error}"
                                );
                                reply.error(EIO);
                            }
                        }
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
            let _slow = SlowFuseRequest::new("setattr");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            let (uid, gid) = current_ids();
            if _uid.is_some_and(|id| id != uid)
                || _gid.is_some_and(|id| id != gid)
                || _mode.is_some_and(|mode| mode & 0o7000 != 0)
            {
                reply.error(libc::EPERM);
                return;
            }
            // Validate truncation against the actual node and signed POSIX
            // file-size limit before chmod or any staging/database mutation.
            // Directory truncation must not silently succeed (or chmod first).
            if let Some(new_size) = size {
                let node = match self.node_for_inode(ino) {
                    Ok(Some(node)) => node,
                    Ok(None) => {
                        reply.error(ENOENT);
                        return;
                    }
                    Err(error) => {
                        log::warn!("Drive setattr target lookup failed: {error}");
                        reply.error(EIO);
                        return;
                    }
                };
                if let Err(errno) = checked_native_setattr_size(attr_for(&node).kind, new_size) {
                    reply.error(errno);
                    return;
                }
            }
            if let Some(mode) = _mode {
                let node = match self.node_for_inode(ino) {
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
                if matches!(&node, Node::Root)
                    || matches!(&node,Node::Remote(file) if file.storage_version==3)
                {
                    reply.error(libc::EPERM);
                    return;
                }
                if let Err(error) = self.permission_change(&node, mode & 0o777) {
                    log::warn!("Drive chmod failed: {error}");
                    reply.error(EIO);
                    return;
                }
            }
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
                    self.reply_attr(&Node::Pending(pending), reply);
                }
                Ok(Some(Node::Remote(file))) => {
                    if file.storage_version == 3 {
                        if size.is_some() {
                            reply.error(EINVAL);
                        } else {
                            self.reply_attr(&Node::Remote(file), reply);
                        }
                        return;
                    }
                    let Some(new_size) = size else {
                        self.reply_attr(&Node::Remote(file), reply);
                        return;
                    };
                    let visible_size = file.plaintext_size.unwrap_or(file.total_size);
                    if new_size == visible_size {
                        self.reply_attr(&Node::Remote(file), reply);
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
                        self.reply_attr(&Node::Pending(pending), reply);
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
                        Ok(Some(file)) => self.reply_attr(&Node::Remote(file), reply),
                        _ => reply.error(EIO),
                    }
                }
                Ok(Some(node @ Node::Directory(_))) | Ok(Some(node @ Node::Root)) => {
                    self.reply_attr(&node, reply);
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
            let _slow = SlowFuseRequest::new("mkdir");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            if let Some(result) = self.restore_native_entry(
                _req,
                parent,
                name,
                crate::drive_restore::RestoreKind::Folder,
                None,
            ) {
                match result {
                    Ok(attr) => {
                        self.lifetimes.lookup(attr.ino);
                        reply.entry(&TTL, &attr, 0);
                    }
                    Err(error) => {
                        log::warn!("Drive native folder restore failed: {error}");
                        reply.error(EIO);
                    }
                }
                return;
            }
            let Some(name) = name.to_str() else {
                reply.error(EIO);
                return;
            };
            let parent_id = match require_fuse_directory_parent(self.directory_id_for_inode(parent))
            {
                Ok(directory_id) => directory_id,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
            let directory = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                match crate::commands::drive_metadata::create_drive_directory_with_mode(
                    &conn,
                    parent_id.as_deref(),
                    name,
                    (_mode & !_umask) & 0o777,
                ) {
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
            self.reply_entry(&node, reply);
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
            let _slow = SlowFuseRequest::new("rename");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            // Ordinary rename replaces its destination atomically; explicit
            // NOREPLACE moves preserve it. Exchange/whiteout remain unsupported.
            let unsupported = flags & !libc::RENAME_NOREPLACE;
            if unsupported != 0 {
                reply.error(EINVAL);
                return;
            }
            let Some(new_name) = newname.to_str() else {
                reply.error(EIO);
                return;
            };
            use crate::commands::drive_metadata::NativeRenameKind;
            let source_identity = match self.lookup_child_identity(parent, name) {
                Ok(Some(source)) => source,
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let fast_source = {
                let index = match self.inode_index.lock() {
                    Ok(index) => index,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                rename_source_fields(&index, &source_identity.0, source_identity.1)
            };
            let (kind, id, source_ino) = match fast_source {
                Some((kind, id, ino, file_type)) => {
                    let mut index = match self.inode_index.lock() {
                        Ok(index) => index,
                        Err(_) => {
                            reply.error(EIO);
                            return;
                        }
                    };
                    if let Err(error) = checked_inode_index_batch(
                        &mut index,
                        [(
                            ino,
                            IndexedInode {
                                identity: source_identity.0.clone(),
                                kind: file_type,
                            },
                        )],
                    ) {
                        log::warn!("Drive rename refused conflicting FUSE inode: {error}");
                        reply.error(EIO);
                        return;
                    }
                    (kind, id, ino)
                }
                None => {
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
                    let ino = node.inode();
                    match node {
                        Node::Directory(directory) => {
                            (NativeRenameKind::Directory, directory.id, ino)
                        }
                        Node::Remote(file) => (
                            if file.storage_version == 3 {
                                NativeRenameKind::Symlink
                            } else {
                                NativeRenameKind::File
                            },
                            file.entry_id,
                            ino,
                        ),
                        Node::Pending(file) => (NativeRenameKind::Pending, file.id, ino),
                        Node::Root => {
                            reply.error(EACCES);
                            return;
                        }
                    }
                }
            };
            let new_parent_id = match self.directory_id_for_inode_fast(newparent) {
                Ok(Some(parent_id)) => parent_id,
                Ok(None) | Err(_) => {
                    reply.error(ENOTDIR);
                    return;
                }
            };
            let result = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                use crate::commands::drive_metadata::{
                    rename_drive_item_native_with_destination, NativeRenameKind,
                };
                (|| {
                    let mut retained = None;
                    let removed = rename_drive_item_native_with_destination(
                        &conn,
                        kind,
                        &id,
                        new_parent_id.as_deref(),
                        new_name,
                        flags & libc::RENAME_NOREPLACE != 0,
                        |pending_id| self.mount_state.is_pending_active(pending_id),
                        |destination_kind, destination_id| {
                            let destination_identity = match destination_kind {
                                NativeRenameKind::Directory => {
                                    DriveNodeId::Directory(destination_id.to_string())
                                }
                                NativeRenameKind::File | NativeRenameKind::Symlink => {
                                    DriveNodeId::File(destination_id.to_string())
                                }
                                NativeRenameKind::Pending => {
                                    DriveNodeId::Pending(destination_id.to_string())
                                }
                            };
                            let indexed_inode = {
                                let index = self
                                    .inode_index
                                    .lock()
                                    .map_err(|_| "Drive inode index poisoned".to_string())?;
                                rename_identity_inode(&index, &destination_identity)
                            };
                            let destination_ino = match indexed_inode {
                                Some(ino) => ino,
                                None => TeraRelayFs::core_node_locked(&conn, &destination_identity)
                                    .map_err(|error| error.to_string())?
                                    .inode(),
                            };
                            if destination_ino != source_ino {
                                if let Some(view) = capture_retained_if_referenced(
                                    &self.lifetimes,
                                    destination_ino,
                                    || {
                                        let destination = TeraRelayFs::core_node_locked(
                                            &conn,
                                            &destination_identity,
                                        )
                                        .map_err(|error| error.to_string())?;
                                        self.capture_retired(&conn, destination)
                                    },
                                )? {
                                    retained = Some(view);
                                }
                            }
                            Ok(())
                        },
                    )?;
                    Ok::<_, String>((removed, retained))
                })()
            };
            match result {
                Ok((removed_pending, retained)) => {
                    let keep_stage = retained.as_ref().is_some_and(|view| view.stage.is_some());
                    if let Some(mut view) = retained {
                        if let Some(stage) = view.stage.as_mut() {
                            stage.committed();
                        }
                        self.lifetimes.retire(view.attr.ino, view);
                    }
                    if let Some(pending) = removed_pending {
                        if let Err(error) =
                            self.cancel_pending_sync_inner(&pending, true, !keep_stage)
                        {
                            log::warn!("Deferred cleanup of replaced Drive staging: {}", error);
                        }
                    }
                    reply.ok();
                }
                Err(error) => {
                    log::warn!("Drive rename failed: {}", error);
                    reply.error(if let Some(errno) = shared_inode_mutation_errno(&error) {
                        errno
                    } else if error.contains("already exists") {
                        EEXIST
                    } else if error.contains("Destination is a directory") {
                        EISDIR
                    } else if error.contains("Destination is not a directory") {
                        ENOTDIR
                    } else if error.contains("not empty") {
                        ENOTEMPTY
                    } else if error.contains("pending write") || error.contains("retained reader") {
                        libc::EBUSY
                    } else if error.contains("does not exist") {
                        ENOENT
                    } else {
                        EIO
                    });
                }
            }
        }

        fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let _slow = SlowFuseRequest::new("unlink");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
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
                        .and_then(|conn| {
                            if file.storage_version != 3 {
                                // Capture of a retained read can materialize file
                                // data and attach staging lifetime state. Do not
                                // do any of that for an inode we cannot unlink
                                // without full shared-inode semantics.
                                crate::commands::drive_metadata::reject_unimplemented_shared_inode_mutation(
                                    &conn,
                                    &file.entry_id,
                                )?;
                            }
                            // An inode can still be opened after its name was looked up.
                            // Preserve the bytes/metadata while kernel references exist,
                            // just as for rename-overwrite and pending-COW unlink.
                            let retained = if file.storage_version != 3 {
                                capture_retained_if_referenced(
                                    &self.lifetimes,
                                    Node::Remote(file.clone()).inode(),
                                    || self.capture_retired(&conn, Node::Remote(file.clone())),
                                )?
                            } else {
                                None
                            };
                            if file.storage_version == 3 {
                                crate::commands::drive_metadata::trash_drive_symlink(
                                    &conn,
                                    &file.entry_id,
                                )?;
                            } else {
                                trash_drive_file(&conn, &file.entry_id)?;
                            }
                            Ok::<_, String>(retained)
                        });
                    match result {
                        Ok(retained) => {
                            if let Some(view) = retained {
                                self.lifetimes.retire(view.attr.ino, view);
                            }
                            reply.ok();
                        }
                        Err(error) => {
                            log::warn!("Drive remote unlink failed: {error}");
                            reply.error(shared_inode_mutation_errno(&error).unwrap_or(EIO));
                        }
                    }
                }
                Node::Pending(file) if file.base_entry_id.is_some() => {
                    let result = {
                        let conn = match self.db.lock() {
                            Ok(conn) => conn,
                            Err(_) => {
                                reply.error(EIO);
                                return;
                            }
                        };
                        (|| {
                            if file.closed_at.is_none()
                                || self.mount_state.is_pending_active(&file.id)
                                || !drive_pending_chunks(&conn, &file.id)?.is_empty()
                                || !drive_pending_parts(&conn, &file.id)?.is_empty()
                            {
                                return Err(
                                    "Drive pending unlink has an active or uploaded pending write"
                                        .to_string(),
                                );
                            }
                            if let Some(base) = file.base_entry_id.as_deref() {
                                crate::commands::drive_metadata::reject_unimplemented_shared_inode_mutation(
                                    &conn,
                                    base,
                                )?;
                            }
                            let ino = Node::Pending(file.clone()).inode();
                            let retained = if self.lifetimes.referenced(ino) {
                                Some(self.capture_retired(&conn, Node::Pending(file.clone()))?)
                            } else {
                                None
                            };
                            let removed =
                                crate::commands::drive_metadata::unlink_drive_pending_native(
                                    &conn, &file.id,
                                )?;
                            Ok::<_, String>((removed, retained))
                        })()
                    };
                    match result {
                        Ok((removed, retained)) => {
                            let keep_stage =
                                retained.as_ref().is_some_and(|view| view.stage.is_some());
                            if let Some(mut view) = retained {
                                if let Some(stage) = view.stage.as_mut() {
                                    stage.committed();
                                }
                                self.lifetimes.retire(view.attr.ino, view);
                            }
                            if let Err(error) =
                                self.cancel_pending_sync_inner(&removed, true, !keep_stage)
                            {
                                log::warn!("Deferred cleanup of unlinked Drive staging: {}", error);
                            }
                            reply.ok();
                        }
                        Err(error) => {
                            log::warn!("Drive pending unlink failed: {}", error);
                            reply.error(if let Some(errno) = shared_inode_mutation_errno(&error) {
                                errno
                            } else if error.contains("active or uploaded pending write")
                                || error.contains("retained reader")
                            {
                                libc::EBUSY
                            } else if error.contains("does not exist") {
                                ENOENT
                            } else {
                                EIO
                            });
                        }
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
            let _slow = SlowFuseRequest::new("rmdir");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
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

        fn link(
            &mut self,
            _req: &Request<'_>,
            _ino: u64,
            _newparent: u64,
            _newname: &OsStr,
            reply: ReplyEntry,
        ) {
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            // A V2 content-object alias is not a POSIX hardlink: writes,
            // truncation, unlink, rename-overwrite and restore require one
            // shared inode with atomic alias-wide updates, link counts, and
            // a compatible synced manifest version. Do not create an alias
            // that merely *looks* linked but can diverge after a write.
            // Refuse without touching private metadata or staged content.
            reply.error(libc::EPERM);
        }

        fn symlink(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            target: &Path,
            reply: ReplyEntry,
        ) {
            let _slow = SlowFuseRequest::new("symlink");
            if self.mount_state.safe_mount_read_only() {
                reply.error(EROFS);
                return;
            }
            if let Some(result) = self.restore_native_entry(
                _req,
                parent,
                name,
                crate::drive_restore::RestoreKind::Symlink,
                target.to_str(),
            ) {
                match result {
                    Ok(attr) => {
                        self.lifetimes.lookup(attr.ino);
                        reply.entry(&TTL, &attr, 0);
                    }
                    Err(error) => {
                        log::warn!("Drive native symlink restore failed: {error}");
                        reply.error(EIO);
                    }
                }
                return;
            }
            let (Some(name), Some(target)) = (name.to_str(), target.to_str()) else {
                reply.error(EINVAL);
                return;
            };
            let parent = match self.directory_id_for_inode(parent) {
                Ok(Some(parent)) => parent,
                Ok(None) => {
                    reply.error(ENOTDIR);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let record = {
                let conn = match self.db.lock() {
                    Ok(conn) => conn,
                    Err(_) => {
                        reply.error(EIO);
                        return;
                    }
                };
                crate::commands::drive_metadata::create_drive_symlink(
                    &conn,
                    parent.as_deref(),
                    name,
                    target,
                )
            };
            match record {
                Ok(link) => {
                    let node = symlink_node(&link);
                    if self.remember_node(&node).is_err() {
                        reply.error(EIO);
                    } else {
                        self.reply_entry(&node, reply);
                    }
                }
                Err(error) => {
                    log::warn!("Drive symlink failed: {error}");
                    reply.error(if error.contains("already exists") {
                        EEXIST
                    } else if error.contains("does not exist") {
                        ENOENT
                    } else {
                        EINVAL
                    });
                }
            }
        }

        fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
            let _slow = SlowFuseRequest::new("readlink");
            let node = match self.node_for_inode(ino) {
                Ok(Some(Node::Remote(file))) if file.storage_version == 3 => file,
                Ok(None) => {
                    reply.error(ENOENT);
                    return;
                }
                Ok(_) => {
                    reply.error(EINVAL);
                    return;
                }
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            let target = resolve_symlink_target(
                self.lifetimes
                    .retired(ino)
                    .and_then(|view| view.symlink_target.as_deref()),
                || {
                    self.db
                        .lock()
                        .map_err(|_| "DB poisoned".to_string())
                        .and_then(|conn| {
                            crate::drive_posix::symlink_by_id(&conn, &node.entry_id)
                                .map(|link| link.map(|link| link.target))
                        })
                },
            );
            match target {
                Ok(Some(target)) => reply.data(target.as_bytes()),
                Ok(None) => reply.error(ENOENT),
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
    #[cfg(target_os = "linux")]
    enum ExistingMountDisposition {
        Ready,
        Healthy,
        Stale,
        InspectionError(i32),
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    fn encode_mountinfo_path(path: &Path) -> String {
        normalize_mount_path_lexically(path)
            .to_string_lossy()
            .replace('\\', "\\134")
            .replace(' ', "\\040")
            .replace('\t', "\\011")
            .replace('\n', "\\012")
    }

    #[cfg(target_os = "linux")]
    pub(super) fn mountpoint_is_active(path: &Path) -> bool {
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    struct MountAttemptGuard<'a>(&'a AtomicBool);

    #[cfg(target_os = "linux")]
    impl<'a> MountAttemptGuard<'a> {
        fn acquire(mounting: &'a AtomicBool) -> Option<Self> {
            if mounting.swap(true, Ordering::SeqCst) {
                return None;
            }
            Some(Self(mounting))
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for MountAttemptGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn mount_impl(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
    ) -> Result<DriveMountStatus, String> {
        let request_epoch = state.inner.request_mount()?;
        mount_for_epoch(app_handle, state, db_pool, telegram_state, request_epoch)
    }

    #[cfg(target_os = "linux")]
    pub fn recover_requested_mount(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
        request_epoch: u64,
    ) -> Result<DriveMountStatus, String> {
        // A monitor retry is not a new user request. Never revive a request
        // cancelled by sign-out, explicit unmount or shutdown.
        if state.inner.requested_epoch()? != Some(request_epoch) {
            return Ok(state.status());
        }
        mount_for_epoch(app_handle, state, db_pool, telegram_state, request_epoch)
    }

    #[cfg(target_os = "linux")]
    fn mount_for_epoch(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
        request_epoch: u64,
    ) -> Result<DriveMountStatus, String> {
        if state.status().mounted {
            return Ok(state.status());
        }
        let Some(_attempt) = MountAttemptGuard::acquire(&state.inner.mounting) else {
            return Ok(state.status());
        };

        // A worker may still be establishing its mount after the caller's
        // startup wait expires. Let that worker publish its eventual result;
        // starting another worker here could stack mounts at the same path.
        if state.inner.session_is_starting()? {
            return Ok(state.status());
        }
        let mount_point = default_mount_point(&app_handle)?;
        prepare_mount_point(&mount_point)?;

        {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::drive_metadata::init_drive_schema(&conn)?;
            crate::commands::drive_metadata::ensure_imported_layout(&conn)?;
        }

        let backend = build_backend(
            &app_handle,
            state.inner(),
            db_pool.inner(),
            telegram_state.inner(),
        )?;
        // Set up bounded HTTP-read workers before publishing the mount.
        let remote_reads = FuseRemoteReadPool::new(2, 32)?;
        let generation = state.inner.begin_session(request_epoch)?;
        let filesystem = LinuxFuseAdapter {
            backend,
            remote_reads,
            generation,
            lifetimes: InodeLifetimes::default(),
        };
        // fuser 0.16 can attempt a second path-based unmount when an old
        // externally detached session is dropped. Consume that old guard
        // before mounting a replacement, while attempts are serialized.
        if let Some(mut unmounter) = state.inner.take_unmounter()? {
            if let Err(error) = unmounter.unmount() {
                state.inner.finish_session(generation);
                return Err(format!(
                    "Could not release the previous Drive mount: {error}"
                ));
            }
        }
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
                let result = fuser::Session::new(filesystem, &mount_for_thread, &options).and_then(
                    |mut session| {
                        inner
                            .register_unmounter(
                                generation,
                                session.unmount_callable(),
                                session.notifier(),
                            )
                            .map_err(std::io::Error::other)?;
                        match inner.publish_session(generation, &mount_for_thread) {
                            Ok(true) => session.run(),
                            Ok(false) => Err(std::io::Error::other(
                                "Drive mount session was cancelled or superseded",
                            )),
                            Err(error) => Err(std::io::Error::other(error)),
                        }
                    },
                );
                if let Err(error) = result {
                    log::error!("TeraRelay Drive mount exited: {}", error);
                }
                inner.finish_session(generation);
            })
            .map_err(|e| {
                state.inner.finish_session(generation);
                format!("Failed to start TeraRelay Drive mount thread: {e}")
            })?;

        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            let status = state.status();
            if status.mounted {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        Err("TeraRelay Drive did not mount within the startup timeout".to_string())
    }

    #[cfg(target_os = "linux")]
    pub fn recover_pending_impl(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
    ) -> Result<DriveRecoveryResult, String> {
        recover_pending_selected_impl(app_handle, state, db_pool, telegram_state, None, None)
            .map(|(summary, _)| summary)
    }

    /// Selected IDs are for the background rate-limited worker; None keeps
    /// the existing user-initiated full recovery behavior.
    pub(super) fn recover_pending_selected_impl(
        app_handle: AppHandle,
        state: State<'_, DriveMountState>,
        db_pool: State<'_, DbConnection>,
        telegram_state: State<'_, crate::commands::TelegramState>,
        selected_ids: Option<&[String]>,
        required_mount_epoch: Option<u64>,
    ) -> Result<(DriveRecoveryResult, Vec<(String, bool)>), String> {
        if state.safe_mount_read_only() {
            return Err("Drive mount is in Safe Read-Only Mode. Switch back to writable mode to retry uploads.".into());
        }
        if crate::commands::qa_feature_a::enabled() {
            return Ok((
                DriveRecoveryResult {
                    recovered: 0,
                    remaining: 0,
                    failures: 0,
                },
                Vec::new(),
            ));
        }

        let closed_ids: Vec<String> = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            closed_pending_ids(&conn)?
                .into_iter()
                .filter(|id| selected_ids.is_none_or(|ids| ids.contains(id)))
                .collect()
        };
        if closed_ids.is_empty() {
            return Ok((
                DriveRecoveryResult {
                    recovered: 0,
                    remaining: 0,
                    failures: 0,
                },
                Vec::new(),
            ));
        }

        let helper = TeraRelayFs {
            db: db_pool.inner().clone(),
            cache_pruner: crate::cache_cleanup::DriveCachePruner::new(
                state.cache_root.clone(),
                CACHE_MAX_BYTES,
            )?,
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
            inode_index: Mutex::new(HashMap::new()),
        };

        let mut recovered = 0u32;
        let mut failures = 0u32;
        let mut attempted = Vec::new();
        for pending_id in closed_ids {
            if state.safe_mount_read_only() {
                break;
            }
            // Unmount or superseding mount must not initiate another upload.
            if let Some(epoch) = required_mount_epoch {
                if state.inner.requested_epoch()? != Some(epoch) || !state.status().mounted {
                    break;
                }
            }
            // The owned claim excludes another recovery command *and* native
            // writable reopens. Re-check the durable DB while holding it.
            let Some(_claim) = state.claim_pending_recovery(&pending_id) else {
                continue;
            };
            let still_closed = {
                let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
                drive_pending_by_id(&conn, &pending_id)?
                    .is_some_and(|pending| pending.closed_at.is_some())
            };
            if !still_closed {
                continue;
            }
            let success = match helper.finalize_pending_sync(&pending_id) {
                Ok(()) => {
                    recovered = recovered.saturating_add(1);
                    true
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    log::warn!(
                        "Deferred TeraRelay Drive recovery for {}: {}",
                        pending_id,
                        error
                    );
                    false
                }
            };
            attempted.push((pending_id, success));
        }

        let remaining = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            closed_pending_count(&conn)?
        };
        Ok((
            DriveRecoveryResult {
                recovered,
                remaining,
                failures,
            },
            attempted,
        ))
    }

    #[cfg(target_os = "linux")]
    pub fn unmount_impl(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
        state.inner.cancel_session();
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

    #[cfg(target_os = "linux")]
    fn should_lazy_detach_after_unmount(normal_succeeded: bool, still_active: bool) -> bool {
        !normal_succeeded && still_active
    }

    #[cfg(target_os = "linux")]
    pub fn unmount_best_effort(state: &DriveMountState) {
        state.inner.cancel_session();
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

    #[cfg(all(test, target_os = "linux"))]
    mod staging_budget_tests {
        use super::{
            classify_existing_mount, encode_mountinfo_path, ensure_private_staging_directory,
            pending_storage_format, recorded_part_needs_reupload, should_lazy_detach_after_unmount,
            staging_can_accept, virtual_statfs_values, visible_snapshot_revision,
            ExistingMountDisposition, PendingStorageFormat, STAGING_BUDGET_BYTES,
        };
        use crate::commands::drive_metadata::{DrivePendingRecord, DriveSnapshot};
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn native_pending_shortcut_refuses_remote_or_cow_encryption_provenance() {
            use super::staged_read_can_skip_remote_provenance;
            // Even fully covered plaintext dirty ranges must NOT bypass the
            // existing remote V2 key verification or remote checksum checks.
            assert!(
                !staged_read_can_skip_remote_provenance(true, false),
                "COW base may be encrypted or locked"
            );
            assert!(
                !staged_read_can_skip_remote_provenance(false, true),
                "uploaded pending chunk may be encrypted or locked"
            );
            assert!(!staged_read_can_skip_remote_provenance(true, true));
            assert!(
                staged_read_can_skip_remote_provenance(false, false),
                "fresh local-only pending chunks previously need no remote key"
            );
        }

        #[test]
        fn native_pending_dirty_slice_reads_only_covered_local_bytes() {
            use super::{read_covered_staged_slice, staged_ranges_cover};
            let root = std::env::temp_dir().join(format!(
                "tr-small-dirty-read-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("private.part");
            std::fs::write(&path, b"abcdefghijklmnop").unwrap();

            assert!(staged_ranges_cover(&[(0, 4), (4, 10)], 2, 9));
            assert!(!staged_ranges_cover(&[(0, 4), (5, 10)], 2, 9));
            assert!(!staged_ranges_cover(&[(0, 4), (4, 8)], 2, 9));
            assert!(!staged_ranges_cover(&[(0, 10)], 4, 4));
            assert_eq!(
                read_covered_staged_slice(&path, &[(0, 4), (4, 10)], 0, 2, 9).unwrap(),
                Some(b"cdefghi".to_vec())
            );
            assert!(
                read_covered_staged_slice(&path, &[(0, 4), (5, 10)], 0, 2, 9)
                    .unwrap()
                    .is_none()
            );
            assert!(read_covered_staged_slice(&path, &[(0, 16)], 16, 2, 9).is_err());
            assert!(read_covered_staged_slice(&path, &[(0, 40)], 0, 12, 20).is_err());

            let linked = root.join("linked.part");
            std::os::unix::fs::symlink(&path, &linked).unwrap();
            assert!(
                read_covered_staged_slice(&linked, &[(0, 16)], 0, 2, 9).is_err(),
                "private staging reads must never follow a symlink"
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_pending_dirty_slice_stays_within_each_crypto_chunk() {
            use super::read_covered_staged_slice;
            let root = std::env::temp_dir()
                .join(format!("tr-dirty-boundary-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&root).unwrap();
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let first = root.join("first.part");
            let file = std::fs::File::create(&first).unwrap();
            file.set_len(chunk_size).unwrap();
            super::positioned_write_all(&file, b"xy", chunk_size - 2).unwrap();
            let second = root.join("second.part");
            std::fs::write(&second, b"ab").unwrap();

            let a = read_covered_staged_slice(
                &first,
                &[(chunk_size - 2, chunk_size)],
                0,
                chunk_size - 2,
                chunk_size,
            )
            .unwrap()
            .unwrap();
            let b = read_covered_staged_slice(
                &second,
                &[(chunk_size, chunk_size + 2)],
                chunk_size,
                chunk_size,
                chunk_size + 2,
            )
            .unwrap()
            .unwrap();
            assert_eq!([a, b].concat(), b"xyab");
            assert!(
                read_covered_staged_slice(
                    &first,
                    &[(chunk_size - 2, chunk_size + 2)],
                    0,
                    chunk_size - 2,
                    chunk_size + 2,
                )
                .is_err(),
                "a direct staged read must not cross its crypto chunk"
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_stage_positioned_write_retries_short_and_interrupted_writes() {
            use std::io::{Error, ErrorKind};
            let mut calls = Vec::new();
            let mut stored = Vec::<u8>::new();
            let mut interrupted = false;
            super::positioned_write_all_with(b"abcdefgh", 100, |bytes, offset| {
                calls.push(offset);
                if !interrupted {
                    interrupted = true;
                    return Err(Error::from(ErrorKind::Interrupted));
                }
                let take = bytes.len().min(3);
                stored.extend_from_slice(&bytes[..take]);
                Ok(take)
            })
            .unwrap();
            assert_eq!(stored, b"abcdefgh");
            assert_eq!(calls, vec![100, 100, 103, 106]);
        }

        #[test]
        fn native_stage_positioned_write_never_acknowledges_zero_or_overflow() {
            use std::io::ErrorKind;
            let error = super::positioned_write_all_with(b"x", 0, |_, _| Ok(0)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::WriteZero);
            let mut called = false;
            let error = super::positioned_write_all_with(b"xy", u64::MAX, |_, _| {
                called = true;
                Ok(1)
            })
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(!called, "overflow must fail before writing any bytes");
        }

        #[test]
        fn native_resized_boundary_stage_failure_preserves_original_chunk() {
            let root = std::env::temp_dir().join(format!(
                "tr-resize-rollback-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&root).unwrap();
            let final_path = root.join("plain-0000000000000000.part");
            std::fs::write(&final_path, b"previous-dirty-bytes").unwrap();
            let error = super::stage_resized_boundary_with(
                &final_path,
                b"replacement-bytes",
                |file, bytes| {
                    super::positioned_write_all_with(bytes, 0, |remaining, offset| {
                        if offset == 0 {
                            super::positioned_write(file, &remaining[..3], offset)
                        } else {
                            Ok(0)
                        }
                    })
                },
            )
            .unwrap_err();
            assert!(error.contains("zero"), "{error}");
            assert_eq!(std::fs::read(&final_path).unwrap(), b"previous-dirty-bytes");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_resized_boundary_preparation_keeps_old_tail_for_crash_rollback() {
            let root = std::env::temp_dir()
                .join(format!("tr-resize-tail-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("plain-0000000000000000.part");
            std::fs::write(&path, b"previous-dirty-bytes").unwrap();
            super::stage_resized_boundary(&path, b"previous").unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"previous-dirty-bytes",
                "until database commit, the old dirty ranges must remain intact"
            );
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            std::fs::remove_dir_all(root).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn native_resized_boundary_post_publish_sync_failure_does_not_undo_visible_commit() {
            use std::io::{Error, ErrorKind, Read};

            let root = std::env::temp_dir().join(format!(
                "tr-resize-post-commit-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("plain-0000000000000000.part");
            std::fs::write(&path, b"previous-dirty-bytes").unwrap();
            let mut old_reader = std::fs::File::open(&path).unwrap();
            let mut sync_calls = 0;
            super::stage_resized_boundary_with_directory_sync(
                &path,
                b"REPLACED",
                |file, bytes| super::positioned_write_all(file, bytes, 0),
                |parent| {
                    assert_eq!(parent, root.as_path());
                    sync_calls += 1;
                    Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "injected post-rename directory fsync failure",
                    ))
                },
            )
            .expect("failure after publishing a new inode must not reject a visible write");

            assert_eq!(sync_calls, 1);
            let mut previous = Vec::new();
            old_reader.read_to_end(&mut previous).unwrap();
            assert_eq!(previous, b"previous-dirty-bytes");
            assert_eq!(std::fs::read(&path).unwrap(), b"REPLACED-dirty-bytes");
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                1,
                "post-publication sync failure must leave no hidden staging files"
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn resized_boundary_rejects_live_and_dangling_symlink_sources_without_touching_targets() {
            use std::os::unix::fs::symlink;
            let root = std::env::temp_dir().join(format!(
                "tr-stage-unsafe-link-{:032x}",
                rand::random::<u128>()
            ));
            let private = root.join("private");
            std::fs::create_dir_all(&private).unwrap();
            let outside = root.join("outside-sentinel");
            std::fs::write(&outside, b"never-modify").unwrap();
            let stage = private.join("plain-0000000000000000.part");
            for target in [outside.clone(), root.join("missing-target")] {
                symlink(&target, &stage).unwrap();
                let result = super::stage_resized_boundary(&stage, b"must-not-write");
                assert!(
                    result.is_err(),
                    "untrusted symlink must fail even when dangling"
                );
                assert!(std::fs::symlink_metadata(&stage)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(std::fs::read_dir(&private).unwrap().count(), 1);
                assert_eq!(std::fs::read(&outside).unwrap(), b"never-modify");
                std::fs::remove_file(&stage).unwrap();
            }
            std::fs::remove_dir_all(root).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn staging_directory_rejects_symlinks_without_chmodding_outside_target() {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let root = std::env::temp_dir().join(format!(
                "tr-private-dir-symlink-{:032x}",
                rand::random::<u128>()
            ));
            let target = root.join("external");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
            let link = root.join("untrusted-staging");
            symlink(&target, &link).unwrap();
            assert!(
                super::ensure_private_staging_directory(&link).is_err(),
                "a symlink to a directory must not be used as private staging"
            );
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o750,
                "refusing the path must not chmod the symlink target"
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn native_staging_budget_rejects_symlink_entries_without_following_them() {
            use std::os::unix::fs::symlink;

            let root = std::env::temp_dir().join(format!(
                "tr-stage-budget-link-{:032x}",
                rand::random::<u128>()
            ));
            let private = root.join("private");
            std::fs::create_dir_all(&private).unwrap();
            let outside = root.join("outside");
            std::fs::write(&outside, b"sentinel").unwrap();
            symlink(&outside, private.join("untrusted.part")).unwrap();
            let result = super::allocated_stage_bytes(&private);
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.contains("unsafe symlink")),
                "budget scan must reject an unsafe staging entry: {result:?}"
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_staging_quota_scan_profiles_bounded_working_sets() {
            use std::time::Instant;
            // This is an isolated direct-Rust scan profile, never a mounted
            // FUSE/local-SSD comparison or a timing acceptance threshold.
            let root = std::env::temp_dir().join(format!(
                "tr-stage-quota-scan-{:032x}",
                rand::random::<u128>()
            ));
            let staging = root.join("private");
            std::fs::create_dir_all(&staging).unwrap();
            let mut previous_count = 0;
            for count in [64usize, 512, 2048] {
                for index in previous_count..count {
                    std::fs::write(staging.join(format!("chunk-{index:06}.bin")), b"x").unwrap();
                }
                previous_count = count;
                let expected_bytes: u64 = std::fs::read_dir(&staging)
                    .unwrap()
                    .map(|entry| super::allocated_file_bytes(&entry.unwrap().metadata().unwrap()))
                    .sum();
                let mut samples_us = Vec::new();
                for _ in 0..5 {
                    let start = Instant::now();
                    let observed = super::allocated_stage_bytes(&root).unwrap();
                    samples_us.push(start.elapsed().as_micros());
                    assert_eq!(observed, expected_bytes);
                }
                samples_us.sort_unstable();
                eprintln!(
                    "staging_quota_scan: entries={count}, median_us={}, scope=isolated",
                    samples_us[2]
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }

        fn retained_reader_fixture() -> (std::path::PathBuf, DrivePendingRecord) {
            let root = std::env::temp_dir()
                .join(format!("tr-retained-range-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&root).unwrap();
            let staging = root.join("0123456789abcdef0123456789abcdef");
            std::fs::create_dir(&staging).unwrap();
            let marker = staging.join(".pending");
            std::fs::write(&marker, b"").unwrap();
            let pending = DrivePendingRecord {
                id: "0123456789abcdef0123456789abcdef".into(),
                directory_id: None,
                display_name: "edited.bin".into(),
                staging_path: marker.to_string_lossy().into_owned(),
                size: crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE + 5,
                format_version: 2,
                backing_channel_id: 0,
                base_entry_id: Some("old-base".into()),
                encryption_version: 0,
                crypto_id: None,
                closed_at: Some(100),
                created_at: 100,
                updated_at: 100,
            };
            (root, pending)
        }

        #[test]
        fn native_reader_production_chunks_preserve_base_and_cross_boundary_dirty_bytes() {
            use std::os::unix::fs::FileExt;
            let (root, pending) = retained_reader_fixture();
            let chunk = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let first = super::open_or_create_private_file(&crate::drive_stream::local_chunk_path(
                &pending, 0,
            ))
            .unwrap();
            first.write_at(b"XY", chunk - 2).unwrap();
            let second = super::open_or_create_private_file(
                &crate::drive_stream::local_chunk_path(&pending, 1),
            )
            .unwrap();
            second.write_at(b"Z12", 0).unwrap();
            let base = super::open_or_create_private_file(&root.join("base.bin")).unwrap();
            base.write_at(b"abcdefghi", chunk - 4).unwrap();
            let actual = super::read_retained_pending(
                &pending,
                &[(chunk - 2, chunk + 3)],
                false,
                chunk - 4,
                16,
                |offset, size| {
                    let mut bytes = vec![0; size as usize];
                    let count = base
                        .read_at(&mut bytes, offset)
                        .map_err(|e| e.to_string())?;
                    bytes.truncate(count);
                    Ok(bytes)
                },
            )
            .unwrap();
            assert_eq!(actual, b"abXYZ12hi");
            assert_eq!(
                super::read_retained_pending(
                    &pending,
                    &[],
                    false,
                    pending.size,
                    10,
                    |_, _| panic!("EOF must not fetch the base")
                )
                .unwrap(),
                Vec::<u8>::new()
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_reader_production_missing_dirty_chunk_never_serves_base_in_its_place() {
            let (root, pending) = retained_reader_fixture();
            let chunk = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let error = super::read_retained_pending(
                &pending,
                &[(chunk, chunk + 3)],
                false,
                chunk,
                3,
                |_, _| Ok(b"old".to_vec()),
            )
            .unwrap_err();
            assert!(
                error.contains("Could not open private Drive file"),
                "{error}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn native_reader_retained_stage_remains_in_the_write_budget_until_cleanup() {
            let (root, pending) = retained_reader_fixture();
            std::fs::write(&pending.staging_path, b"pinned source bytes").unwrap();
            let before = super::allocated_stage_bytes(&root).unwrap();
            assert!(before >= 19);
            let mut stage = super::super::read_lifetime::RetainedStage::prepare(
                &root,
                &pending.id,
                std::path::Path::new(&pending.staging_path),
            )
            .unwrap();
            stage.committed();
            assert!(super::allocated_stage_bytes(&root).unwrap() >= before);
            drop(stage);
            assert_eq!(super::allocated_stage_bytes(&root).unwrap(), 0);
            std::fs::remove_dir_all(root).unwrap();
        }

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
        fn native_remote_unlink_retains_lookup_referenced_inode_for_followup_open() {
            use super::super::read_lifetime::InodeLifetimes;
            use super::{
                attr_for, capture_retained_if_referenced, indexed_read_only_open_kind,
                IndexedInode, Node, RetiredRead,
            };
            use crate::commands::drive_metadata::DriveFileRecord;
            use crate::drive_fs_core::DriveNodeId;
            use fuser::FileType;
            use std::collections::HashMap;

            let ino = 79;
            let original = b"remote bytes retained across unlink".to_vec();
            let node = Node::Remote(DriveFileRecord {
                entry_id: "unlinked-remote-entry".into(),
                file_id: "remote-file".into(),
                inode_id: None,
                directory_id: None,
                display_name: "original.bin".into(),
                total_size: original.len() as u64,
                mime_type: Some("application/octet-stream".into()),
                first_message_id: 99,
                backing_channel_id: 42,
                logical_channel_id: "test-channel".into(),
                created_at: 1,
                updated_at: 1,
                encryption_version: 0,
                plaintext_size: Some(original.len() as u64),
                crypto_id: None,
                content_fingerprint: None,
                storage_version: 1,
            });
            let mut index = HashMap::new();
            index.insert(
                ino,
                IndexedInode {
                    identity: DriveNodeId::File("unlinked-remote-entry".into()),
                    kind: FileType::RegularFile,
                },
            );
            let mut lifetimes: InodeLifetimes<RetiredRead> = InodeLifetimes::default();
            lifetimes.lookup(ino);

            let retained = capture_retained_if_referenced(&lifetimes, ino, || {
                let mut attr = attr_for(&node);
                attr.nlink = 0;
                Ok(RetiredRead {
                    node: node.clone(),
                    attr,
                    ranges: Vec::new(),
                    base: None,
                    locations: None,
                    stage: None,
                    whole_stage: false,
                    symlink_target: None,
                })
            })
            .unwrap()
            .expect("unlink must capture a view while lookup refs remain");
            lifetimes.retire(ino, retained);

            let mut reads = 0;
            let bytes = lifetimes
                .retired(ino)
                .unwrap()
                .read_with(0, original.len() as u32, |file, offset, size, _| {
                    reads += 1;
                    assert_eq!(file.entry_id, "unlinked-remote-entry");
                    let start = offset.min(original.len() as u64) as usize;
                    let end = offset
                        .saturating_add(size as u64)
                        .min(original.len() as u64) as usize;
                    Ok(original[start..end].to_vec())
                })
                .unwrap();
            assert_eq!(
                bytes, original,
                "retained reads must return the pre-unlink bytes"
            );
            assert_eq!(reads, 1);
            assert_eq!(
                indexed_read_only_open_kind(&index, &lifetimes, ino),
                Some(FileType::RegularFile)
            );

            lifetimes.open(ino);
            lifetimes.forget(ino, 1);
            assert!(lifetimes.retired(ino).is_some());
            lifetimes.close(ino);
            assert!(lifetimes.retired(ino).is_none());

            let unreferenced: InodeLifetimes<RetiredRead> = InodeLifetimes::default();
            let mut capture_called = false;
            let retained = capture_retained_if_referenced(&unreferenced, ino, || {
                capture_called = true;
                Err("unreferenced inode should not be captured".into())
            })
            .unwrap();
            assert!(retained.is_none());
            assert!(!capture_called);
            assert_eq!(
                indexed_read_only_open_kind(&index, &unreferenced, ino),
                None,
                "stale index identity without a kernel reference must not fast-open"
            );
        }

        #[test]
        fn indexed_getattr_uses_fused_file_mode_and_pending_mode_fallback() {
            use super::TeraRelayFs;
            use crate::commands::drive_metadata::{
                create_drive_pending_replacement, init_drive_schema,
            };
            use crate::drive_fs_core::DriveNodeId;
            use crate::drive_posix::{upsert_permission, DrivePermissionRecord};

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels
                    (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
                VALUES ('getattr-channel',73,'getattr-channel','owner',1,1,1);
                INSERT INTO logical_files
                    (file_id,logical_channel_id,first_message_id,display_name,total_size,mime_type,
                     chunk_count,whole_sha256,storage_version,created_at,status)
                VALUES ('getattr-file','getattr-channel',99,'getattr.txt',2048,'text/plain',
                        1,NULL,1,1,'complete');",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_file_entries
                    (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at,
                     encryption_version,plaintext_size,crypto_id,content_fingerprint)
                 VALUES ('getattr-entry','getattr-file',NULL,'getattr.txt',1,1,NULL,0,2048,NULL,NULL);",
            )
            .unwrap();
            let permission = |kind: &str, id: &str, mode: u32| {
                upsert_permission(
                    &conn,
                    &DrivePermissionRecord {
                        node_kind: kind.into(),
                        node_id: id.into(),
                        mode,
                        updated_at: 1,
                    },
                )
                .unwrap();
            };
            permission("file", "getattr-entry", 0o640);

            crate::drive_posix::reset_permission_call_count();
            let attr = TeraRelayFs::indexed_regular_attr_locked(
                &conn,
                &DriveNodeId::File("getattr-entry".into()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(attr.kind, fuser::FileType::RegularFile);
            assert_eq!(attr.size, 2048);
            assert_eq!(attr.perm, 0o640);
            assert_eq!(
                crate::drive_posix::permission_call_count(),
                0,
                "indexed getattr must not execute a second permission query"
            );

            let pending = create_drive_pending_replacement(
                &conn,
                "getattr-pending",
                "getattr-entry",
                "/tmp/getattr-pending/.pending",
            )
            .unwrap();
            permission("pending", &pending.id, 0o600);
            crate::drive_posix::reset_permission_call_count();
            let attr = TeraRelayFs::indexed_regular_attr_locked(
                &conn,
                &DriveNodeId::File("getattr-entry".into()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(attr.perm, 0o600);
            assert_eq!(crate::drive_posix::permission_call_count(), 0);

            conn.execute(
                "DELETE FROM drive_permissions WHERE node_kind='pending' AND node_id='getattr-pending'",
            )
            .unwrap();
            crate::drive_posix::reset_permission_call_count();
            let attr = TeraRelayFs::indexed_regular_attr_locked(
                &conn,
                &DriveNodeId::File("getattr-entry".into()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(attr.perm, 0o640);
            assert_eq!(crate::drive_posix::permission_call_count(), 0);
        }

        #[test]
        fn retired_symlink_readlink_uses_captured_target_after_replacement() {
            use super::super::read_lifetime::InodeLifetimes;
            use super::{
                attr_for, capture_retained_if_referenced, capture_symlink_target,
                resolve_symlink_target, symlink_node, RetiredRead,
            };
            use crate::drive_posix::DriveSymlinkRecord;

            let conn = sqlite::open(":memory:").unwrap();
            crate::drive_posix::init_schema(&conn).unwrap();
            let link = DriveSymlinkRecord {
                id: "retired-link".into(),
                directory_id: None,
                display_name: "shortcut".into(),
                target: "../original-target.txt".into(),
                created_at: 1000,
                updated_at: 1000,
                trashed_at: None,
            };
            crate::drive_posix::upsert_symlink(&conn, &link).unwrap();

            let node = symlink_node(&link);
            let ino = node.inode();
            let mut lifetimes: InodeLifetimes<RetiredRead> = InodeLifetimes::default();
            lifetimes.lookup(ino);
            let retained = capture_retained_if_referenced(&lifetimes, ino, || {
                let mut attr = attr_for(&node);
                attr.nlink = 0;
                Ok(RetiredRead {
                    symlink_target: capture_symlink_target(&conn, &node)?,
                    node: node.clone(),
                    attr,
                    ranges: Vec::new(),
                    base: None,
                    locations: None,
                    stage: None,
                    whole_stage: false,
                })
            })
            .unwrap()
            .expect("referenced symlink target must be retained before replacement");
            lifetimes.retire(ino, retained);

            let live_target = resolve_symlink_target(None, || {
                crate::drive_posix::symlink_by_id(&conn, "retired-link")
                    .map(|link| link.map(|link| link.target))
            })
            .unwrap();
            assert_eq!(live_target.as_deref(), Some("../original-target.txt"));

            conn.execute("UPDATE drive_symlinks SET trashed_at=2000 WHERE id='retired-link'")
                .unwrap();

            let mut live_lookup_called = false;
            let target = resolve_symlink_target(
                lifetimes
                    .retired(ino)
                    .and_then(|view| view.symlink_target.as_deref()),
                || {
                    live_lookup_called = true;
                    crate::drive_posix::symlink_by_id(&conn, "retired-link")
                        .map(|link| link.map(|link| link.target))
                },
            )
            .unwrap();
            assert_eq!(target.as_deref(), Some("../original-target.txt"));
            assert!(
                !live_lookup_called,
                "a retained symlink must not consult the trashed live row"
            );

            lifetimes.forget(ino, 1);
            assert!(lifetimes.retired(ino).is_none());
        }

        #[test]
        fn linux_rename_source_lookup_preserves_identity_and_symlink_kind() {
            use super::{lookup_child_identity_locked, lookup_child_node_locked};
            use crate::commands::drive_metadata::{create_drive_pending_file, init_drive_schema};
            use crate::drive_fs_core::DriveNodeId;

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels
                    (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
                VALUES ('lookup-channel',73,'lookup-channel','owner',1,1,1);
                INSERT INTO logical_files
                    (file_id,logical_channel_id,first_message_id,display_name,total_size,mime_type,
                     chunk_count,whole_sha256,storage_version,created_at,status)
                VALUES ('legacy-file','lookup-channel',99,'legacy.txt',2048,'text/plain',
                        1,NULL,1,1,'complete');",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_posix::init_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_file_entries
                    (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at,
                     encryption_version,plaintext_size,crypto_id,content_fingerprint)
                 VALUES ('legacy-entry','legacy-file',NULL,'legacy.txt',1,1,NULL,0,2048,NULL,NULL);
                 INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                 VALUES ('lookup-folder',NULL,'folder',NULL,1,1,NULL);
                 INSERT INTO drive_objects
                    (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,
                     block_count,block_hashes_b64,created_at)
                 VALUES ('v2-object',4096,1,'v2-crypto','v2-fingerprint',0,NULL,2);
                 INSERT INTO drive_entries_v2
                    (entry_id,object_id,directory_id,display_name,created_at,updated_at,trashed_at)
                 VALUES ('v2-entry','v2-object',NULL,'v2.txt',2,2,NULL);
                 INSERT INTO drive_symlinks
                    (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
                 VALUES ('link-entry',NULL,'link-name','target.txt',1,1,NULL);",
            )
            .unwrap();
            create_drive_pending_file(
                &conn,
                "pending-entry",
                None,
                "draft.txt",
                "/tmp/pending-entry/.pending",
            )
            .unwrap();

            assert_eq!(
                lookup_child_identity_locked(&conn, None, "link-name").unwrap(),
                Some((DriveNodeId::File("link-entry".into()), true))
            );
            assert_eq!(
                lookup_child_identity_locked(&conn, None, "folder").unwrap(),
                Some((DriveNodeId::Directory("lookup-folder".into()), false))
            );
            assert_eq!(
                lookup_child_identity_locked(&conn, None, "draft.txt").unwrap(),
                Some((DriveNodeId::Pending("pending-entry".into()), false))
            );
            assert_eq!(
                lookup_child_identity_locked(&conn, None, "legacy.txt").unwrap(),
                Some((DriveNodeId::File("legacy-entry".into()), false))
            );
            assert_eq!(
                lookup_child_identity_locked(&conn, None, "v2.txt").unwrap(),
                Some((DriveNodeId::File("v2-entry".into()), false))
            );
            assert_eq!(
                lookup_child_identity_locked(&conn, None, "missing").unwrap(),
                None
            );

            let symlink = lookup_child_node_locked(&conn, None, "link-name")
                .unwrap()
                .unwrap();
            assert!(matches!(
                symlink,
                super::Node::Remote(ref file) if file.storage_version == 3 && file.entry_id == "link-entry"
            ));

            let directory = lookup_child_node_locked(&conn, None, "folder")
                .unwrap()
                .unwrap();
            assert!(matches!(
                directory,
                super::Node::Directory(ref directory) if directory.id == "lookup-folder"
            ));

            let pending = lookup_child_node_locked(&conn, None, "draft.txt")
                .unwrap()
                .unwrap();
            assert!(matches!(
                pending,
                super::Node::Pending(ref pending) if pending.id == "pending-entry"
            ));

            let legacy = lookup_child_node_locked(&conn, None, "legacy.txt")
                .unwrap()
                .unwrap();
            assert!(matches!(
                legacy,
                super::Node::Remote(ref file) if file.storage_version == 1 && file.entry_id == "legacy-entry"
            ));

            let v2 = lookup_child_node_locked(&conn, None, "v2.txt")
                .unwrap()
                .unwrap();
            assert!(matches!(
                v2,
                super::Node::Remote(ref file) if file.storage_version == 2 && file.entry_id == "v2-entry"
            ));

            assert!(lookup_child_node_locked(&conn, None, "missing")
                .unwrap()
                .is_none());
        }

        #[test]
        fn native_read_only_open_uses_referenced_indexed_type_without_metadata_resolution() {
            use super::super::read_lifetime::InodeLifetimes;
            use super::{indexed_read_only_open_kind, IndexedInode};
            use crate::drive_fs_core::DriveNodeId;
            use fuser::FileType;
            use std::collections::HashMap;

            let ino = 77;
            let mut index = HashMap::new();
            index.insert(
                ino,
                IndexedInode {
                    identity: DriveNodeId::File("indexed-file".into()),
                    kind: FileType::RegularFile,
                },
            );
            let mut lifetimes: InodeLifetimes<()> = InodeLifetimes::default();
            assert_eq!(indexed_read_only_open_kind(&index, &lifetimes, ino), None);
            lifetimes.lookup(ino);
            assert_eq!(
                indexed_read_only_open_kind(&index, &lifetimes, ino),
                Some(FileType::RegularFile)
            );

            index.get_mut(&ino).unwrap().kind = FileType::Symlink;
            assert_eq!(
                indexed_read_only_open_kind(&index, &lifetimes, ino),
                Some(FileType::Symlink)
            );
            index.get_mut(&ino).unwrap().kind = FileType::Directory;
            assert_eq!(
                indexed_read_only_open_kind(&index, &lifetimes, ino),
                Some(FileType::Directory)
            );
        }

        #[test]
        fn native_rename_identity_uses_stable_inode_or_indexed_pending_alias() {
            use super::{inode_for, rename_identity_inode, rename_source_fields, IndexedInode};
            use crate::drive_fs_core::DriveNodeId;
            use fuser::FileType;
            use std::collections::HashMap;

            let pending = DriveNodeId::Pending("pending-overlay".into());
            let pending_inode = inode_for("file", "base-entry");
            let mut index = HashMap::new();
            index.insert(
                pending_inode,
                IndexedInode {
                    identity: pending.clone(),
                    kind: FileType::RegularFile,
                },
            );

            assert_eq!(
                rename_identity_inode(&index, &DriveNodeId::Directory("folder".into())),
                Some(inode_for("dir", "folder"))
            );
            assert_eq!(
                rename_identity_inode(&index, &DriveNodeId::File("file-entry".into())),
                Some(inode_for("file", "file-entry"))
            );
            assert_eq!(rename_identity_inode(&index, &pending), Some(pending_inode));
            assert_eq!(
                rename_identity_inode(
                    &HashMap::new(),
                    &DriveNodeId::Pending("unindexed-pending".into())
                ),
                None,
                "pending entries need the indexed base-entry inode alias"
            );

            let regular =
                rename_source_fields(&index, &DriveNodeId::File("file-entry".into()), false)
                    .unwrap();
            assert!(matches!(
                regular.0,
                crate::commands::drive_metadata::NativeRenameKind::File
            ));
            assert_eq!(regular.1, "file-entry");
            assert_eq!(regular.2, inode_for("file", "file-entry"));
            assert_eq!(regular.3, FileType::RegularFile);

            let symlink =
                rename_source_fields(&index, &DriveNodeId::File("link-entry".into()), true)
                    .unwrap();
            assert!(matches!(
                symlink.0,
                crate::commands::drive_metadata::NativeRenameKind::Symlink
            ));
            assert_eq!(symlink.3, FileType::Symlink);

            let directory =
                rename_source_fields(&index, &DriveNodeId::Directory("folder".into()), false)
                    .unwrap();
            assert!(matches!(
                directory.0,
                crate::commands::drive_metadata::NativeRenameKind::Directory
            ));
            assert_eq!(directory.3, FileType::Directory);

            let pending_fields = rename_source_fields(&index, &pending, false).unwrap();
            assert!(matches!(
                pending_fields.0,
                crate::commands::drive_metadata::NativeRenameKind::Pending
            ));
            assert_eq!(pending_fields.1, "pending-overlay");
            assert_eq!(pending_fields.2, pending_inode);
            assert_eq!(pending_fields.3, FileType::RegularFile);
            assert!(rename_source_fields(
                &HashMap::new(),
                &DriveNodeId::Pending("unindexed-pending".into()),
                false
            )
            .is_none());
        }

        #[test]
        fn native_indexed_directory_id_requires_a_referenced_live_directory_identity() {
            use super::{indexed_directory_id, inode_for, IndexedInode, ROOT_INODE};
            use crate::drive_fs_core::DriveNodeId;
            use fuser::FileType;
            use std::collections::HashMap;

            let directory_ino = inode_for("dir", "folder");
            let file_ino = inode_for("file", "file-entry");
            let retired_ino = inode_for("dir", "retired-folder");
            let unreferenced_ino = inode_for("dir", "unreferenced-folder");
            let mut index = HashMap::new();
            index.insert(
                directory_ino,
                IndexedInode {
                    identity: DriveNodeId::Directory("folder".into()),
                    kind: FileType::Directory,
                },
            );
            index.insert(
                file_ino,
                IndexedInode {
                    identity: DriveNodeId::File("file-entry".into()),
                    kind: FileType::RegularFile,
                },
            );
            index.insert(
                retired_ino,
                IndexedInode {
                    identity: DriveNodeId::Directory("retired-folder".into()),
                    kind: FileType::Directory,
                },
            );
            index.insert(
                unreferenced_ino,
                IndexedInode {
                    identity: DriveNodeId::Directory("unreferenced-folder".into()),
                    kind: FileType::Directory,
                },
            );

            let mut lifetimes = super::InodeLifetimes::<()>::default();
            lifetimes.lookup(directory_ino);
            lifetimes.lookup(file_ino);
            lifetimes.lookup(retired_ino);
            lifetimes.retire(retired_ino, ());

            assert_eq!(
                indexed_directory_id(&index, &lifetimes, ROOT_INODE),
                Some(None)
            );
            assert_eq!(
                indexed_directory_id(&index, &lifetimes, directory_ino),
                Some(Some("folder".into()))
            );
            assert_eq!(indexed_directory_id(&index, &lifetimes, file_ino), None);
            assert_eq!(indexed_directory_id(&index, &lifetimes, retired_ino), None);
            assert_eq!(
                indexed_directory_id(&index, &lifetimes, unreferenced_ino),
                None
            );
            assert_eq!(indexed_directory_id(&index, &lifetimes, u64::MAX), None);
        }

        #[test]
        fn native_read_uses_indexed_regular_file_identity_hint_only_for_regular_files() {
            use super::{indexed_regular_file_identity, IndexedInode};
            use crate::drive_fs_core::DriveNodeId;
            use fuser::FileType;
            use std::collections::HashMap;

            let mut index = HashMap::new();
            index.insert(
                81,
                IndexedInode {
                    identity: DriveNodeId::File("regular-entry".into()),
                    kind: FileType::RegularFile,
                },
            );
            index.insert(
                82,
                IndexedInode {
                    identity: DriveNodeId::File("symlink-entry".into()),
                    kind: FileType::Symlink,
                },
            );
            index.insert(
                83,
                IndexedInode {
                    identity: DriveNodeId::Directory("directory-entry".into()),
                    kind: FileType::Directory,
                },
            );

            assert_eq!(
                indexed_regular_file_identity(&index, 81),
                Some(DriveNodeId::File("regular-entry".into()))
            );
            assert_eq!(indexed_regular_file_identity(&index, 82), None);
            assert_eq!(indexed_regular_file_identity(&index, 83), None);
            assert_eq!(indexed_regular_file_identity(&index, 84), None);
        }

        #[test]
        fn native_resolved_read_entry_point_avoids_a_second_metadata_lookup() {
            use super::{Node, TeraRelayFs};
            use crate::drive_fs_core::DriveFsError;

            type ResolvedReader =
                fn(&TeraRelayFs, &Node, u64, u64) -> Result<Vec<u8>, DriveFsError>;
            let _read_resolved: ResolvedReader = TeraRelayFs::read_resolved_node;
        }

        #[test]
        fn native_pending_hot_lookup_resolves_from_indexed_identity() {
            use super::TeraRelayFs;
            use crate::commands::drive_metadata::{create_drive_pending_file, init_drive_schema};
            use crate::drive_fs_core::DriveNodeId;

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            let pending = create_drive_pending_file(
                &conn,
                "hot-pending-id",
                None,
                "hot.txt",
                "/tmp/hot-pending-id/.pending",
            )
            .unwrap();

            let identity = DriveNodeId::File(pending.id.clone());
            let resolved = TeraRelayFs::pending_for_indexed_identity(&conn, &identity)
                .unwrap()
                .unwrap();
            assert_eq!(resolved.id, pending.id);

            super::reset_core_node_locked_call_count();
            let node = TeraRelayFs::indexed_regular_node_locked(&conn, &identity)
                .unwrap()
                .unwrap();
            assert!(matches!(node, super::Node::Pending(_)));
            assert_eq!(
                super::core_node_locked_call_count(),
                0,
                "indexed regular-file resolution must not re-enter the generic type lookup"
            );

            // The current live read path re-resolves one indexed file through
            // regular_file_entry for every FUSE read callback. Count the
            // resolution boundary deterministically, without inferring elapsed
            // time or promising a performance improvement.
            super::reset_indexed_regular_node_resolution_count();
            for _ in 0..64 {
                let current = TeraRelayFs::indexed_regular_node_locked(&conn, &identity)
                    .unwrap()
                    .unwrap();
                assert!(matches!(current, super::Node::Pending(_)));
            }
            assert_eq!(
                super::indexed_regular_node_resolution_count(),
                64,
                "64 independent live-read resolutions currently request 64 SQL lookups"
            );
            assert_eq!(super::core_node_locked_call_count(), 0);

            assert!(TeraRelayFs::indexed_regular_node_locked(
                &conn,
                &DriveNodeId::File("missing-regular-file".into())
            )
            .unwrap()
            .is_none());

            assert!(TeraRelayFs::pending_for_indexed_identity(
                &conn,
                &DriveNodeId::Directory("unrelated".into()),
            )
            .unwrap()
            .is_none());
        }

        #[test]
        fn native_directory_listing_hot_path_does_not_build_drive_snapshot() {
            use super::{
                indexed_directory_listing, indexed_directory_listing_query_count, inode_for,
                reset_indexed_directory_listing_query_count,
            };
            use crate::commands::drive_metadata::{
                create_drive_pending_file, create_drive_pending_replacement,
                drive_snapshot_call_count, init_drive_schema, reset_drive_snapshot_calls,
            };

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels VALUES
                    ('listing-channel', 42, 'Listing', 'owner', 1, 1, 1);
                INSERT INTO logical_files VALUES
                    ('base-file', 'listing-channel', 7, 'base.txt', 8, 'text/plain', 1, NULL, 1, 1, 'complete');",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                 VALUES ('child-dir',NULL,'child-dir',NULL,1,1,NULL);
                 INSERT INTO drive_file_entries
                    (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at,
                     encryption_version,plaintext_size,crypto_id,content_fingerprint)
                 VALUES ('base-entry','base-file',NULL,'base.txt',1,1,NULL,0,8,NULL,NULL);
                 INSERT INTO drive_objects
                    (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,
                     block_count,block_hashes_b64,created_at)
                 VALUES ('object-v2',16,0,NULL,'fingerprint-v2',0,NULL,1);
                 INSERT INTO drive_entries_v2
                    (entry_id,object_id,directory_id,display_name,created_at,updated_at,trashed_at)
                 VALUES ('entry-v2','object-v2',NULL,'v2.txt',1,1,NULL);
                 INSERT INTO drive_symlinks
                    (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
                 VALUES ('link-id',NULL,'tool-link','../bin/tool',1,1,NULL);",
            )
            .unwrap();
            create_drive_pending_replacement(
                &conn,
                "replacement-pending",
                "base-entry",
                "/tmp/replacement-pending/.pending",
            )
            .unwrap();
            create_drive_pending_file(
                &conn,
                "new-pending",
                None,
                "draft.txt",
                "/tmp/new-pending/.pending",
            )
            .unwrap();

            reset_drive_snapshot_calls();
            reset_indexed_directory_listing_query_count();
            let (parent, entries) = indexed_directory_listing(&conn, None)
                .unwrap()
                .expect("root listing should always exist");
            assert_eq!(parent, None);
            assert_eq!(indexed_directory_listing_query_count(), 1);
            assert_eq!(drive_snapshot_call_count(), 0);

            let mut names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["base.txt", "child-dir", "draft.txt", "tool-link", "v2.txt"]
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.name == "base.txt")
                    .count(),
                1,
                "the pending COW overlay must hide its base entry"
            );
            let base = entries
                .iter()
                .find(|entry| entry.name == "base.txt")
                .unwrap();
            assert_eq!(base.ino, inode_for("file", "base-entry"));
            let link = entries
                .iter()
                .find(|entry| entry.name == "tool-link")
                .unwrap();
            assert_eq!(link.ino, inode_for("file", "link-id"));
        }

        #[test]
        fn indexed_fuse_directory_sees_new_completed_upload_without_remount() {
            use super::{indexed_directory_listing, inode_for};
            use crate::commands::drive_metadata::{
                ensure_imported_layout, init_drive_schema, publish_completed_upload_to_drive,
            };
            use crate::commands::logical_files::{LogicalFileManifestV1, ManifestChunkV1};

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL, joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY, logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL, display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL, mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1, whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete'
                );
                CREATE TABLE logical_file_chunks (
                    file_id TEXT NOT NULL, chunk_index INTEGER NOT NULL,
                    message_id INTEGER NOT NULL, chunk_size INTEGER NOT NULL,
                    sha256 TEXT, PRIMARY KEY(file_id,chunk_index)
                );
                INSERT INTO logical_channels VALUES
                    ('hot-directory',45,'Hot','owner',1,1,1);",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            ensure_imported_layout(&conn).unwrap();
            let id = "channel-hot-directory";
            assert!(indexed_directory_listing(&conn, Some(id))
                .unwrap()
                .unwrap()
                .1
                .is_empty());
            let manifest = LogicalFileManifestV1 {
                schema_version: crate::commands::logical_files::MANIFEST_SCHEMA_VERSION,
                file_id: "0123456789abcdef0123456789abcdef".into(),
                logical_channel_id: "hot-directory".into(),
                original_name: "newly-uploaded.bin".into(),
                total_size: 4,
                mime_type: None,
                created_at: 1_790_000_000,
                whole_sha256: None,
                chunks: vec![ManifestChunkV1 {
                    index: 1,
                    message_id: 45,
                    size: 4,
                    sha256: None,
                }],
            };
            assert_eq!(
                publish_completed_upload_to_drive(&conn, &manifest).unwrap(),
                Some((
                    id.to_owned(),
                    "newly-uploaded.bin".into(),
                    None,
                    "import-0123456789abcdef0123456789abcdef".into()
                ))
            );
            let entries = indexed_directory_listing(&conn, Some(id))
                .unwrap()
                .unwrap()
                .1;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "newly-uploaded.bin");
            assert_eq!(
                entries[0].ino,
                inode_for("file", "import-0123456789abcdef0123456789abcdef")
            );
        }

        #[test]
        fn indexed_directory_listing_fuses_parent_validation_and_children() {
            use super::{
                indexed_directory_listing, indexed_directory_listing_query_count,
                reset_indexed_directory_listing_query_count,
            };
            use crate::commands::drive_metadata::init_drive_schema;

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels VALUES ('listing-channel',42,'Listing','owner',1,1,1);",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                VALUES ('parent-dir',NULL,'parent',NULL,1,1,NULL),
                       ('child-dir','parent-dir','child',NULL,1,1,NULL),
                       ('empty-dir','parent-dir','empty',NULL,1,1,NULL),
                       ('dead-dir',NULL,'dead',NULL,1,1,10);
                INSERT INTO drive_objects
                    (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,
                     block_count,block_hashes_b64,created_at)
                VALUES ('child-object',16,0,NULL,'child-fingerprint',0,NULL,1),
                       ('dead-object',16,0,NULL,'dead-fingerprint',0,NULL,1);
                INSERT INTO drive_entries_v2
                    (entry_id,object_id,directory_id,display_name,created_at,updated_at,trashed_at)
                VALUES ('child-entry','child-object','child-dir','child.txt',1,1,NULL),
                       ('dead-entry','dead-object','dead-dir','hidden.txt',1,1,NULL);",
            )
            .unwrap();

            reset_indexed_directory_listing_query_count();
            let (parent, entries) = indexed_directory_listing(&conn, Some("child-dir"))
                .unwrap()
                .expect("a live indexed directory should return a listing");
            assert_eq!(indexed_directory_listing_query_count(), 1);
            assert_eq!(parent.as_deref(), Some("parent-dir"));
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "child.txt");

            reset_indexed_directory_listing_query_count();
            let (parent, entries) = indexed_directory_listing(&conn, Some("empty-dir"))
                .unwrap()
                .expect("a live empty directory should return an empty listing");
            assert_eq!(indexed_directory_listing_query_count(), 1);
            assert_eq!(parent.as_deref(), Some("parent-dir"));
            assert!(entries.is_empty());

            reset_indexed_directory_listing_query_count();
            assert!(
                indexed_directory_listing(&conn, Some("dead-dir"))
                    .unwrap()
                    .is_none(),
                "a trashed parent must not expose its children"
            );
            assert_eq!(indexed_directory_listing_query_count(), 1);

            reset_indexed_directory_listing_query_count();
            assert!(
                indexed_directory_listing(&conn, Some("missing-dir"))
                    .unwrap()
                    .is_none(),
                "a missing parent must fail closed"
            );
            assert_eq!(indexed_directory_listing_query_count(), 1);

            reset_indexed_directory_listing_query_count();
            let (parent, entries) = indexed_directory_listing(&conn, None)
                .unwrap()
                .expect("the root listing must always exist");
            assert_eq!(indexed_directory_listing_query_count(), 1);
            assert_eq!(parent, None);
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["parent"]
            );
        }

        #[test]
        fn indexed_directory_listings_suppress_trashed_v2_legacy_shadow_but_preserve_orphan_fallback(
        ) {
            use super::{indexed_directory_listing, indexed_directory_plus_listing};
            use crate::commands::drive_metadata::init_drive_schema;

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels
                    (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
                VALUES ('listing-channel',42,'Listing','owner',1,1,1);
                INSERT INTO logical_files
                    (file_id,logical_channel_id,first_message_id,display_name,total_size,mime_type,
                     chunk_count,whole_sha256,storage_version,created_at,status)
                VALUES ('shadow-file','listing-channel',7,'stale-name.txt',8,'text/plain',1,NULL,1,1,'complete'),
                       ('orphan-file','listing-channel',8,'orphan-visible.txt',8,'text/plain',1,NULL,1,1,'complete');",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_file_entries
                    (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at,
                     encryption_version,plaintext_size,crypto_id,content_fingerprint)
                 VALUES ('shadow-entry','shadow-file',NULL,'stale-name.txt',1,1,NULL,0,8,NULL,NULL),
                        ('orphan-entry','orphan-file',NULL,'orphan-visible.txt',1,1,NULL,0,8,NULL,NULL);
                 INSERT INTO drive_objects
                    (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,
                     block_count,block_hashes_b64,created_at)
                 VALUES ('shadow-object',16,0,NULL,'shadow-fingerprint',0,NULL,1);
                 INSERT INTO drive_entries_v2
                    (entry_id,object_id,directory_id,display_name,created_at,updated_at,trashed_at)
                 VALUES ('shadow-entry','shadow-object',NULL,'current-name.txt',1,2,2),
                        ('orphan-entry','missing-object',NULL,'orphan-v2-name.txt',1,2,NULL);",
            )
            .unwrap();

            let (_, ordinary_entries) = indexed_directory_listing(&conn, None)
                .unwrap()
                .expect("root listing should always exist");
            let ordinary_names: Vec<_> = ordinary_entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                !ordinary_names.contains(&"stale-name.txt"),
                "readdir must not expose a legacy shadow while its real V2 object is trashed"
            );
            assert!(!ordinary_names.contains(&"current-name.txt"));
            assert!(
                ordinary_names.contains(&"orphan-visible.txt"),
                "a V2 row without an object must preserve legacy fallback"
            );

            let plus = indexed_directory_plus_listing(&conn, None)
                .unwrap()
                .expect("root directory-plus listing should always exist");
            let plus_names: Vec<_> = plus
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                !plus_names.contains(&"stale-name.txt"),
                "readdirplus must not expose a legacy shadow while its real V2 object is trashed"
            );
            assert!(!plus_names.contains(&"current-name.txt"));
            assert!(
                plus_names.contains(&"orphan-visible.txt"),
                "directory-plus must preserve orphan V2 legacy fallback"
            );
        }

        #[test]
        fn native_directory_plus_listing_batches_authoritative_attributes_without_snapshot() {
            use super::{
                core_node_locked_call_count, indexed_directory_plus_listing,
                indexed_directory_plus_listing_query_count, inode_for,
                reset_core_node_locked_call_count,
                reset_indexed_directory_plus_listing_query_count, ROOT_INODE,
            };
            use crate::commands::drive_metadata::{
                create_drive_pending_file, create_drive_pending_replacement,
                drive_snapshot_call_count, init_drive_schema, reset_drive_snapshot_calls,
            };
            use fuser::FileType;

            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );
                INSERT INTO logical_channels VALUES
                    ('plus-channel', 42, 'Plus', 'owner', 1, 1, 1);
                INSERT INTO logical_files VALUES
                    ('base-file', 'plus-channel', 7, 'base.txt', 8, 'text/plain', 1, NULL, 1, 1, 'complete');",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                 VALUES ('child-dir',NULL,'child-dir',NULL,1,1,NULL),
                        ('parent-dir',NULL,'parent-dir',NULL,1,1,NULL),
                        ('nested-dir','parent-dir','nested-dir',NULL,1,1,NULL),
                        ('empty-dir','parent-dir','empty-dir',NULL,1,1,NULL),
                        ('dead-dir',NULL,'dead-dir',NULL,1,1,10);
                 INSERT INTO drive_file_entries
                    (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at,
                     encryption_version,plaintext_size,crypto_id,content_fingerprint)
                 VALUES ('base-entry','base-file',NULL,'base.txt',1,1,NULL,0,8,NULL,NULL);
                 INSERT INTO drive_objects
                    (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,
                     block_count,block_hashes_b64,created_at)
                 VALUES ('object-v2',16,0,NULL,'fingerprint-v2',0,NULL,1);
                 INSERT INTO drive_entries_v2
                    (entry_id,object_id,directory_id,display_name,created_at,updated_at,trashed_at)
                 VALUES ('entry-v2','object-v2',NULL,'v2.txt',1,1,NULL);
                 INSERT INTO drive_symlinks
                    (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
                 VALUES ('link-id',NULL,'tool-link','../bin/tool',1,1,NULL);",
            )
            .unwrap();
            create_drive_pending_replacement(
                &conn,
                "replacement-pending",
                "base-entry",
                "/tmp/replacement-pending/.pending",
            )
            .unwrap();
            create_drive_pending_file(
                &conn,
                "new-pending",
                None,
                "draft.txt",
                "/tmp/new-pending/.pending",
            )
            .unwrap();
            crate::drive_posix::set_permission(&conn, "directory", "child-dir", 0o750).unwrap();
            crate::drive_posix::set_permission(&conn, "directory", "parent-dir", 0o700).unwrap();
            crate::drive_posix::set_permission(&conn, "directory", "nested-dir", 0o751).unwrap();
            crate::drive_posix::set_permission(&conn, "file", "base-entry", 0o600).unwrap();
            crate::drive_posix::set_permission(&conn, "pending", "new-pending", 0o640).unwrap();
            crate::drive_posix::set_permission(&conn, "file", "entry-v2", 0o620).unwrap();

            reset_drive_snapshot_calls();
            reset_core_node_locked_call_count();
            reset_indexed_directory_plus_listing_query_count();
            crate::drive_posix::reset_permission_call_count();
            let listing = indexed_directory_plus_listing(&conn, None)
                .unwrap()
                .unwrap();
            assert_eq!(indexed_directory_plus_listing_query_count(), 1);
            assert_eq!(listing.node.inode(), ROOT_INODE);
            assert_eq!(listing.parent_inode, ROOT_INODE);
            assert_eq!(listing.current_attr.perm, 0o755);
            assert_eq!(listing.parent_attr.perm, 0o755);
            assert_eq!(
                listing.current_attr.nlink, 4,
                "root has two live direct child folders"
            );
            assert_eq!(
                listing.parent_attr.nlink, 4,
                "root parent entry must agree with stat"
            );
            let entries = listing.entries;
            assert_eq!(drive_snapshot_call_count(), 0);
            assert_eq!(
                core_node_locked_call_count(),
                0,
                "directory-plus must not re-resolve each child through targeted node queries"
            );
            assert_eq!(
                crate::drive_posix::permission_call_count(),
                0,
                "directory-plus must batch permission modes into the parent-scoped query"
            );
            assert_eq!(entries.len(), 6);

            let base = entries
                .iter()
                .find(|entry| entry.name == "base.txt")
                .unwrap();
            assert_eq!(base.attr.ino, inode_for("file", "base-entry"));
            assert_eq!(base.attr.perm, 0o600, "COW overlay must inherit base mode");
            assert_eq!(base.attr.size, 8);

            let directory = entries
                .iter()
                .find(|entry| entry.name == "child-dir")
                .unwrap();
            assert_eq!(directory.attr.perm, 0o750);
            assert_eq!(directory.attr.kind, FileType::Directory);
            assert_eq!(
                directory.attr.nlink, 2,
                "leaf directory has no child directories"
            );
            let parent_folder = entries
                .iter()
                .find(|entry| entry.name == "parent-dir")
                .unwrap();
            assert_eq!(
                parent_folder.attr.nlink, 4,
                "directory-plus must count both nested folders"
            );

            let draft = entries
                .iter()
                .find(|entry| entry.name == "draft.txt")
                .unwrap();
            assert_eq!(draft.attr.perm, 0o640);
            assert_eq!(draft.attr.kind, FileType::RegularFile);

            let v2 = entries.iter().find(|entry| entry.name == "v2.txt").unwrap();
            assert_eq!(v2.attr.perm, 0o620);
            assert_eq!(v2.attr.size, 16);

            let link = entries
                .iter()
                .find(|entry| entry.name == "tool-link")
                .unwrap();
            assert_eq!(link.attr.ino, inode_for("file", "link-id"));
            assert_eq!(link.attr.kind, FileType::Symlink);
            assert_eq!(link.attr.perm, 0o777);

            reset_indexed_directory_plus_listing_query_count();
            let listing = indexed_directory_plus_listing(&conn, Some("nested-dir"))
                .unwrap()
                .expect("a live non-root directory should return current/parent attributes");
            assert_eq!(indexed_directory_plus_listing_query_count(), 1);
            assert!(matches!(
                listing.node,
                super::Node::Directory(ref directory) if directory.id == "nested-dir"
            ));
            assert_eq!(listing.current_attr.perm, 0o751);
            assert_eq!(listing.current_attr.nlink, 2);
            assert_eq!(listing.parent_inode, inode_for("dir", "parent-dir"));
            assert_eq!(listing.parent_attr.perm, 0o700);
            assert_eq!(listing.parent_attr.nlink, 4);
            assert!(
                listing.entries.is_empty(),
                "empty directory has no child rows"
            );

            let stat_root = super::LinuxFuseAdapter::attributes_locked(&conn, &super::Node::Root)
                .expect("FUSE getattr must expose the root directory link count");
            assert_eq!(stat_root.nlink, 4);
            let parent = crate::drive_lookup::directory(&conn, "parent-dir")
                .unwrap()
                .unwrap();
            let stat_parent =
                super::LinuxFuseAdapter::attributes_locked(&conn, &super::Node::Directory(parent))
                    .expect("FUSE getattr must expose child-directory links");
            assert_eq!(stat_parent.nlink, 4);

            reset_indexed_directory_plus_listing_query_count();
            assert!(indexed_directory_plus_listing(&conn, Some("dead-dir"))
                .unwrap()
                .is_none());
            assert_eq!(indexed_directory_plus_listing_query_count(), 1);
            reset_indexed_directory_plus_listing_query_count();
            assert!(indexed_directory_plus_listing(&conn, Some("missing-dir"))
                .unwrap()
                .is_none());
            assert_eq!(indexed_directory_plus_listing_query_count(), 1);

            conn.execute("UPDATE drive_directories SET parent_id='dead-dir' WHERE id='nested-dir'")
                .unwrap();
            reset_indexed_directory_plus_listing_query_count();
            assert!(
                indexed_directory_plus_listing(&conn, Some("nested-dir")).is_err(),
                "a live directory under a trashed parent must fail closed"
            );
            assert_eq!(indexed_directory_plus_listing_query_count(), 1);
        }

        #[test]
        fn native_directory_plus_profiles_large_project_metadata_scaling() {
            use super::{
                indexed_directory_plus_listing, indexed_directory_plus_listing_query_count,
                reset_indexed_directory_plus_listing_query_count,
            };
            use crate::commands::drive_metadata::init_drive_schema;
            use std::time::Instant;

            // Actual production SQLite+attribute listing logic, not a mocked
            // FUSE mount or a local-SSD-relative throughput measurement.
            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                 VALUES ('profile-parent',NULL,'profile-parent',NULL,1,1,NULL);",
            )
            .unwrap();

            let mut previous_count = 0;
            for count in [64usize, 512, 2048] {
                conn.execute("BEGIN IMMEDIATE").unwrap();
                for index in previous_count..count {
                    conn.execute(format!(
                        "INSERT INTO drive_directories
                         (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                         VALUES ('profile-child-{index:05}','profile-parent','child-{index:05}',
                                 NULL,1,1,NULL);"
                    ))
                    .unwrap();
                }
                conn.execute("COMMIT").unwrap();
                previous_count = count;
                let mut samples_us = Vec::new();
                for _ in 0..5 {
                    reset_indexed_directory_plus_listing_query_count();
                    let start = Instant::now();
                    let listing = indexed_directory_plus_listing(&conn, Some("profile-parent"))
                        .unwrap()
                        .expect("the live parent must resolve");
                    samples_us.push(start.elapsed().as_micros());
                    assert_eq!(listing.entries.len(), count);
                    assert_eq!(indexed_directory_plus_listing_query_count(), 1);
                }
                samples_us.sort_unstable();
                eprintln!(
                    "diagnostic_only_native_readdirplus entries={count} median_us={}                      min_us={} max_us={} scope=sqlite_no_fuse",
                    samples_us[2],
                    samples_us[0],
                    samples_us[4]
                );
            }
        }

        #[test]
        fn native_readdirplus_profiles_pending_source_file_rows_at_scale() {
            use super::{
                indexed_directory_plus_listing, indexed_directory_plus_listing_query_count,
                reset_indexed_directory_plus_listing_query_count,
            };
            use crate::commands::drive_metadata::init_drive_schema;
            use fuser::FileType;
            use std::time::Instant;

            // Isolated in-memory pending-file rows: unlike the directory-only
            // profile, this exercises indexed regular-file attributes and
            // names. It does not reproduce the 32x32 FUSE directory topology.
            let conn = sqlite::open(":memory:").unwrap();
            conn.execute(
                "CREATE TABLE logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete'
                );",
            )
            .unwrap();
            init_drive_schema(&conn).unwrap();
            crate::drive_lookup::init_indexes(&conn).unwrap();
            conn.execute(
                "INSERT INTO drive_directories
                    (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
                 VALUES ('source-file-parent',NULL,'source-file-parent',NULL,1,1,NULL);",
            )
            .unwrap();

            let mut previous_count = 0;
            for count in [64usize, 512, 1024] {
                conn.execute("BEGIN IMMEDIATE").unwrap();
                for index in previous_count..count {
                    conn.execute(format!(
                        "INSERT INTO drive_pending_files
                         (id,directory_id,display_name,staging_path,size,format_version,
                          backing_channel_id,created_at,updated_at)
                         VALUES ('source-{index:05}','source-file-parent',
                                 'file-{index:05}.bin','/tmp/qa-virtual-only',256,2,42,1,1);"
                    ))
                    .unwrap();
                }
                conn.execute("COMMIT").unwrap();
                previous_count = count;

                let mut samples_us = Vec::new();
                for _ in 0..5 {
                    reset_indexed_directory_plus_listing_query_count();
                    let start = Instant::now();
                    let listing = indexed_directory_plus_listing(&conn, Some("source-file-parent"))
                        .unwrap()
                        .expect("the live file parent must resolve");
                    samples_us.push(start.elapsed().as_micros());
                    assert_eq!(listing.entries.len(), count);
                    assert!(listing.entries.iter().all(|entry| {
                        entry.kind == FileType::RegularFile && entry.attr.size == 256
                    }));
                    assert_eq!(indexed_directory_plus_listing_query_count(), 1);
                }
                samples_us.sort_unstable();
                eprintln!(
                    "diagnostic_only_native_readdirplus_pending_files entries={count}                      median_us={} min_us={} max_us={} scope=sqlite_no_fuse",
                    samples_us[2],
                    samples_us[0],
                    samples_us[4]
                );
            }
        }

        #[test]
        fn project_posix_inode_is_stable_across_pending_finalization() {
            use super::{inode_for, Node, TeraRelayFs};
            use crate::drive_fs_core::DriveNodeId;
            let node = Node::Pending(DrivePendingRecord {
                id: "stable-project-file".into(),
                directory_id: None,
                display_name: "tool.sh".into(),
                staging_path: "owned-tool.sh".into(),
                size: 12,
                format_version: 2,
                backing_channel_id: 0,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: Some(100),
                created_at: 100,
                updated_at: 100,
            });
            assert_eq!(
                node.inode(),
                inode_for("file", "stable-project-file"),
                "finalization must retain the kernel inode of a newly created project file"
            );
            assert_eq!(
                TeraRelayFs::inode_identity(&node),
                DriveNodeId::File("stable-project-file".into()),
                "inode lookups must find the finalized entry after the pending row disappears"
            );
        }

        #[test]
        fn provider_snapshot_revision_tracks_pending_metadata_without_cloud_dirty_state() {
            let mut snapshot = DriveSnapshot {
                revision: 42,
                dirty: false,
                symlinks: Vec::new(),
                directories: Vec::new(),
                files: Vec::new(),
                pending: Vec::new(),
            };
            let empty_revision = visible_snapshot_revision(&snapshot).unwrap();
            snapshot.pending.push(DrivePendingRecord {
                id: "pending-a".to_string(),
                directory_id: None,
                display_name: "draft.bin".to_string(),
                staging_path: "test-draft.bin".to_string(),
                size: 0,
                format_version: 2,
                backing_channel_id: 0,
                base_entry_id: None,
                encryption_version: 0,
                crypto_id: None,
                closed_at: None,
                created_at: 100,
                updated_at: 100,
            });
            let created_revision = visible_snapshot_revision(&snapshot).unwrap();
            assert_ne!(created_revision, empty_revision);

            snapshot.pending[0].size = 8192;
            snapshot.pending[0].updated_at = 101;
            let resized_revision = visible_snapshot_revision(&snapshot).unwrap();
            assert_ne!(resized_revision, created_revision);
            assert_eq!(
                visible_snapshot_revision(&snapshot).unwrap(),
                resized_revision
            );
            assert_eq!(
                snapshot.revision, 42,
                "provider revision must not mutate cloud revision"
            );
            assert!(
                !snapshot.dirty,
                "provider revision must not mark Drive metadata dirty"
            );
        }

        #[test]
        fn cached_block_requires_exact_length_before_reading() {
            let root =
                std::env::temp_dir().join(format!("tr-read-cache-{:016x}", rand::random::<u64>()));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("block.bin");
            std::fs::write(&path, b"complete").unwrap();
            assert_eq!(
                super::read_cached_block(&path, 8),
                Some(b"complete".to_vec())
            );
            assert!(super::read_cached_block(&path, 7).is_none());
            assert!(super::read_cached_block(&path, 9).is_none());
            assert!(super::read_cached_block(&path, 0).is_none());
            assert!(super::read_cached_block(&root, 8).is_none());
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn oversized_sparse_cache_never_allocates_its_logical_size() {
            let path = std::env::temp_dir()
                .join(format!("tr-read-cache-huge-{:016x}", rand::random::<u64>()));
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(10_000_000_000_000).unwrap();
            assert!(super::read_cached_block(&path, 4 * 1024 * 1024).is_none());
            assert!(super::read_cached_block(&path, 10_000_000_000_000).is_none());
            drop(file);
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn cached_block_rejects_a_symbolic_link() {
            let root = std::env::temp_dir()
                .join(format!("tr-read-cache-link-{:016x}", rand::random::<u64>()));
            std::fs::create_dir(&root).unwrap();
            let real = root.join("real.bin");
            let link = root.join("linked.bin");
            std::fs::write(&real, b"complete").unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(super::read_cached_block(&link, 8).is_none());
            assert_eq!(std::fs::read(real).unwrap(), b"complete");
            std::fs::remove_dir_all(root).unwrap();
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
        fn virtual_drive_capacity_reports_unknown_without_limiting_streaming_copies() {
            let values = virtual_statfs_values();
            // GIO recognizes zero free+available on FUSE as unknown capacity,
            // so Nemo omits the capacity label and skips the whole-copy precheck.
            assert_eq!(values.blocks, 0);
            assert_eq!(values.blocks_free, 0);
            assert_eq!(values.blocks_available, 0);
            assert_eq!(values.block_size, 4096);
            assert_eq!(values.fragment_size, 4096);
            assert!(values.files > 1_000_000_000);
            assert_eq!(values.files, values.files_free);
        }

        #[test]
        fn mountinfo_path_encoding_matches_linux_single_backslash_escapes() {
            let path = std::path::Path::new("/srv/terarelay/TeraRelay Drive\tQA");
            assert_eq!(
                encode_mountinfo_path(path),
                "/srv/terarelay/TeraRelay\\040Drive\\011QA"
            );
        }

        #[test]
        fn mountinfo_path_encoding_normalizes_dotdot_components() {
            let path = std::path::Path::new("/srv/terarelay/app/../TeraRelay Drive QA");
            assert_eq!(
                encode_mountinfo_path(path),
                "/srv/terarelay/TeraRelay\\040Drive\\040QA"
            );
        }

        #[test]
        fn failed_mount_preparation_releases_the_retry_gate() {
            let flag = std::sync::atomic::AtomicBool::new(false);
            let result: Result<(), ()> = {
                let _attempt = super::MountAttemptGuard::acquire(&flag).unwrap();
                Err(())
            };
            assert!(result.is_err());
            assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
            assert!(super::MountAttemptGuard::acquire(&flag).is_some());
        }

        #[test]
        fn overlapping_attempt_does_not_release_another_attempts_gate() {
            let flag = std::sync::atomic::AtomicBool::new(false);
            let first = super::MountAttemptGuard::acquire(&flag).unwrap();
            assert!(super::MountAttemptGuard::acquire(&flag).is_none());
            assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
            drop(first);
            assert!(super::MountAttemptGuard::acquire(&flag).is_some());
        }

        #[test]
        fn an_unwinding_mount_attempt_can_be_retried() {
            let flag = std::sync::atomic::AtomicBool::new(false);
            let result = std::panic::catch_unwind(|| {
                let _attempt = super::MountAttemptGuard::acquire(&flag).unwrap();
                panic!("QA mount preparation panic");
            });
            assert!(result.is_err());
            assert!(super::MountAttemptGuard::acquire(&flag).is_some());
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
    desktop::mount_impl(app_handle, state, db_pool, telegram_state)
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn cmd_drive_recover_pending(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveRecoveryResult, String> {
    desktop::recover_pending_impl(app_handle, state, db_pool, telegram_state)
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    desktop::unmount_impl(state)
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn cmd_drive_mount(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveMountStatus, String> {
    let root = crate::drive_windows::connect(
        &app_handle,
        state.inner(),
        db_pool.inner(),
        telegram_state.inner(),
    )?;
    state.mark_provider_connected(Some(root));
    Ok(state.status())
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn cmd_drive_recover_pending(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveRecoveryResult, String> {
    let summary = build_drive_fs_core(
        &app_handle,
        state.inner(),
        db_pool.inner(),
        telegram_state.inner(),
    )?
    .recover_pending()
    .map_err(|e| e.to_string())?;
    crate::drive_windows::refresh()?;
    Ok(DriveRecoveryResult {
        recovered: summary.recovered,
        remaining: summary.remaining,
        failures: summary.failures,
    })
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    crate::drive_windows::disconnect();
    state.mark_provider_disconnected();
    Ok(state.status())
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn cmd_drive_mount(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveMountStatus, String> {
    crate::drive_provider_ipc::start(
        &app_handle,
        state.inner(),
        db_pool.inner(),
        telegram_state.inner(),
    )?;
    state.mark_provider_connected(None);
    crate::drive_provider_ipc::signal_changes(&app_handle)?;
    Ok(state.status())
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn cmd_drive_recover_pending(
    app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    db_pool: State<'_, DbConnection>,
    telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveRecoveryResult, String> {
    let summary = build_drive_fs_core(
        &app_handle,
        state.inner(),
        db_pool.inner(),
        telegram_state.inner(),
    )?
    .recover_pending()
    .map_err(|e| e.to_string())?;
    crate::drive_provider_ipc::signal_changes(&app_handle)?;
    Ok(DriveRecoveryResult {
        recovered: summary.recovered,
        remaining: summary.remaining,
        failures: summary.failures,
    })
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    crate::drive_provider_ipc::stop();
    state.mark_provider_disconnected();
    Ok(state.status())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
#[tauri::command]
pub fn cmd_drive_mount(
    _app_handle: AppHandle,
    state: State<'_, DriveMountState>,
    _db_pool: State<'_, DbConnection>,
    _telegram_state: State<'_, crate::commands::TelegramState>,
) -> Result<DriveMountStatus, String> {
    let _ = state;
    Err("TeraRelay Drive native filesystem integration is only available on desktop".to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
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

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
#[tauri::command]
pub fn cmd_drive_unmount(state: State<'_, DriveMountState>) -> Result<DriveMountStatus, String> {
    Ok(state.status())
}

#[cfg(target_os = "linux")]
pub use desktop::unmount_best_effort;

#[cfg(target_os = "windows")]
pub fn unmount_best_effort(state: &DriveMountState) {
    crate::drive_windows::disconnect();
    state.mark_provider_disconnected();
}

#[cfg(target_os = "macos")]
pub fn unmount_best_effort(state: &DriveMountState) {
    crate::drive_provider_ipc::stop();
    state.mark_provider_disconnected();
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn unmount_best_effort(_state: &DriveMountState) {}
