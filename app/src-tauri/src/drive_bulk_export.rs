//! Explicit native Linux bulk export of COMPLETED Drive file content.
//! Reads through the existing mounted FUSE provider; never mutates Telegram,
//! Drive metadata or upload staging. A failure leaves no published archive.
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, validate_drive_name, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use crate::drive_mount::DriveMountState;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};
use tauri::{AppHandle, Manager, State};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const MAX_FILES: usize = 100;
const MAX_TOTAL: u64 = 8 * 1024 * 1024 * 1024;
const MAX_MANIFEST: usize = 1024 * 1024;
const BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug)]
struct SelectedFile {
    record: DriveFileRecord,
    names: Vec<String>,
    archive_path: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportProgress {
    pub id: String,
    pub phase: &'static str,
    pub current: usize,
    pub count: usize,
    pub bytes_written: u64,
    pub total_bytes: u64,
    pub files_verified: usize,
    pub error: Option<String>,
}

struct ExportJob {
    cancelled: AtomicBool,
    progress: Mutex<ExportProgress>,
}
impl ExportJob {
    fn snapshot(&self) -> Result<ExportProgress, String> {
        self.progress
            .lock()
            .map(|p| p.clone())
            .map_err(|_| "Drive export status unavailable".into())
    }
    fn update(&self, action: impl FnOnce(&mut ExportProgress)) {
        if let Ok(mut status) = self.progress.lock() {
            action(&mut status);
        }
    }
    fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err("Drive export cancelled before completion; no archive was published".into())
        } else {
            Ok(())
        }
    }
}
static EXPORT: LazyLock<Mutex<Option<Arc<ExportJob>>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Serialize)]
struct ArchiveManifest {
    format: &'static str,
    revision: i64,
    files: Vec<ArchiveEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    folders: Vec<String>,
}
#[derive(Serialize)]
struct ArchiveEntry {
    entry_id: String,
    path: String,
    bytes: u64,
    sha256: String,
    verified_in_export_zip: bool,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}

/// Native completed-file readers must never include a local uncommitted
/// replacement or a separate pending write shadowing the same path. Keep
/// this identical for initial selection and every progress revalidation.
fn pending_shadows_file(snapshot: &DriveSnapshot, file: &DriveFileRecord) -> bool {
    snapshot.pending.iter().any(|p| {
        p.base_entry_id.as_deref() == Some(file.entry_id.as_str())
            || (p.directory_id == file.directory_id && p.display_name == file.display_name)
    })
}

fn select(snapshot: &DriveSnapshot, entry_ids: &[String]) -> Result<Vec<SelectedFile>, String> {
    if entry_ids.is_empty() || entry_ids.len() > MAX_FILES {
        return Err("Select 1–100 completed Drive files".into());
    }
    let mut unique = HashSet::new();
    let mut portable_paths = HashSet::new();
    let mut total = 0u64;
    let dirs: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let files: HashMap<_, _> = snapshot
        .files
        .iter()
        .map(|f| (f.entry_id.as_str(), f))
        .collect();
    let mut out = Vec::with_capacity(entry_ids.len());
    for id in entry_ids {
        if !valid_id(id) || !unique.insert(id.as_str()) {
            return Err("Duplicate or invalid Drive selection identity".into());
        }
        let file = files
            .get(id.as_str())
            .ok_or("Selected completed Drive file is unavailable")?;
        // Native pending COW overlays must never be mistaken for completed data.
        if pending_shadows_file(snapshot, file) {
            return Err(
                "A selected file has unfinished local changes. Finish recovery first.".into(),
            );
        }
        if validate_drive_name(&file.display_name).is_err() {
            return Err("Selected Drive file has a nonportable name".into());
        }
        let size = file.plaintext_size.unwrap_or(file.total_size);
        total = total
            .checked_add(size)
            .ok_or("Drive export total overflows")?;
        if total > MAX_TOTAL {
            return Err("Selected file content exceeds the 8 GiB export limit".into());
        }
        let mut names = vec![file.display_name.clone()];
        let mut ancestry = HashSet::new();
        let mut parent = file.directory_id.as_deref();
        while let Some(id) = parent {
            if !valid_id(id) || !ancestry.insert(id) || ancestry.len() > 100 {
                return Err("Selected file has invalid or cyclic folder ancestry".into());
            }
            let dir = dirs
                .get(id)
                .ok_or("A selected file has a missing or trashed parent")?;
            if validate_drive_name(&dir.name).is_err() {
                return Err("Selected file has an unsafe folder name".into());
            }
            names.push(dir.name.clone());
            parent = dir.parent_id.as_deref();
        }
        names.reverse();
        let archive_path = format!("DriveFiles/{}", names.join("/"));
        if archive_path.len() > 4096 {
            return Err("Selected Drive path is too long for safe ZIP export".into());
        }
        let portable_key = names
            .iter()
            .map(|name| portable_name_key(name))
            .collect::<Vec<_>>()
            .join("/");
        if !portable_paths.insert(portable_key) {
            return Err(
                "Selected files collide when extracted on a case-insensitive filesystem".into(),
            );
        }
        out.push(SelectedFile {
            record: (**file).clone(),
            names,
            archive_path,
            size,
        });
    }
    Ok(out)
}

fn check_snapshot(
    pool: &DbConnection,
    expected_revision: i64,
    items: &[SelectedFile],
) -> Result<(), String> {
    let db = pool
        .lock()
        .map_err(|_| "Drive database unavailable during export".to_string())?;
    let fresh = drive_snapshot(&db)?;
    if fresh.revision != expected_revision {
        return Err("Drive metadata changed during export. Refresh and retry.".into());
    }
    for item in items {
        let current = fresh
            .files
            .iter()
            .find(|f| f.entry_id == item.record.entry_id)
            .ok_or("A selected Drive file disappeared during export")?;
        if current.file_id != item.record.file_id
            || current.directory_id != item.record.directory_id
            || current.display_name != item.record.display_name
            || current.plaintext_size != item.record.plaintext_size
            || current.total_size != item.record.total_size
            || current.encryption_version != item.record.encryption_version
            || current.crypto_id != item.record.crypto_id
            || current.storage_version != item.record.storage_version
            || current.content_fingerprint != item.record.content_fingerprint
            || current.updated_at != item.record.updated_at
            || pending_shadows_file(&fresh, &item.record)
        {
            return Err(
                "Selected Drive file changed while exporting; incomplete output discarded".into(),
            );
        }
    }
    Ok(())
}

/// Open each component relative to the mount directory descriptor. A malicious
/// symlink in an imported metadata path can never redirect export to host data.
fn open_beneath(root: &Path, names: &[String]) -> Result<File, String> {
    if names.is_empty() {
        return Err("Drive selection has no file path".into());
    }
    let mut node = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|e| format!("Drive mount is unavailable: {e}"))?;
    for (i, part) in names.iter().enumerate() {
        if validate_drive_name(part).is_err() {
            return Err("Unsafe Drive file path component".into());
        }
        let name = CString::new(std::ffi::OsStr::new(part).as_bytes())
            .map_err(|_| "NUL in Drive file path")?;
        let is_leaf = i + 1 == names.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if is_leaf { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(node.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(format!(
                "Could not safely open selected Drive file: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: openat succeeded and ownership transfers to the new File.
        let child = unsafe { File::from_raw_fd(fd) };
        let meta = child.metadata().map_err(|e| e.to_string())?;
        if (is_leaf && !meta.is_file()) || (!is_leaf && !meta.is_dir()) {
            return Err("A selected Drive path changed into a non-file or symbolic link".into());
        }
        node = child;
    }
    Ok(node)
}

struct Uncommitted(Vec<PathBuf>);
impl Drop for Uncommitted {
    fn drop(&mut self) {
        for path in self.0.drain(..).rev() {
            let _ = fs::remove_file(path);
        }
    }
}

fn destination_guard(
    destination: &Path,
    mount_root: &Path,
    app_cache: &Path,
    app_data: &Path,
) -> Result<(), String> {
    if destination.file_name().is_none() || destination.exists() {
        return Err("Select a new output ZIP. Existing files are never replaced.".into());
    }
    let parent = destination
        .parent()
        .ok_or("ZIP destination has no parent")?;
    let parent_meta = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
    if !parent_meta.is_dir() || parent_meta.file_type().is_symlink() {
        return Err("Export output must be in an ordinary directory".into());
    }
    let canonical = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    for restricted in [mount_root, app_cache, app_data] {
        if let Ok(root) = fs::canonicalize(restricted) {
            if canonical.starts_with(root) {
                return Err(
                    "Choose an export location outside mounted Drive and application data".into(),
                );
            }
        }
    }
    Ok(())
}

fn new_private(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("Could not create private ZIP temporary file: {e}"))
}

#[cfg(test)]
fn export_files(
    root: &Path,
    items: &[SelectedFile],
    revision: i64,
    destinations: (&Path, &Path, &Path),
    job: &Arc<ExportJob>,
    check_fresh: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    export_files_with_folders(root, items, revision, destinations, job, &[], check_fresh)
}

fn export_files_with_folders(
    root: &Path,
    items: &[SelectedFile],
    revision: i64,
    destinations: (&Path, &Path, &Path),
    job: &Arc<ExportJob>,
    folders: &[String],
    check_fresh: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    let (destination, cache_root, app_data) = destinations;
    job.check()?;
    check_fresh()?;
    destination_guard(destination, root, cache_root, app_data)?;
    let parent = destination.parent().ok_or("ZIP target parent missing")?;
    let temporary = parent.join(format!(
        ".trdrive-export-{:032x}.partial",
        rand::random::<u128>()
    ));
    let output = new_private(&temporary)?;
    let mut guard = Uncommitted(vec![temporary.clone()]);
    let mut zip = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut manifest = ArchiveManifest {
        format: if folders.is_empty() {
            "terarelay-completed-drive-export-v1"
        } else {
            "terarelay-completed-drive-folder-export-v2"
        },
        revision,
        files: Vec::new(),
        folders: folders.to_vec(),
    };
    for folder in folders {
        job.check()?;
        check_fresh()?;
        if !folder.starts_with("DriveFiles/") || !folder.ends_with('/') || folder.len() > 4096 {
            return Err("Invalid ZIP folder directory entry".into());
        }
        zip.add_directory(folder.as_str(), options)
            .map_err(|e| e.to_string())?;
    }
    for (index, item) in items.iter().enumerate() {
        job.check()?;
        check_fresh()?;
        let mut reader = open_beneath(root, &item.names)?;
        let stat = reader.metadata().map_err(|e| e.to_string())?;
        if stat.len() != item.size {
            return Err("Mounted Drive file size disagrees with verified local metadata".into());
        }
        zip.start_file(&item.archive_path, options)
            .map_err(|e| e.to_string())?;
        let mut count = 0u64;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; BUFFER_SIZE];
        loop {
            job.check()?;
            let n = reader
                .read(&mut buffer)
                .map_err(|e| format!("Drive read interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n as u64)
                .ok_or("Drive file size overflow")?;
            if count > item.size {
                return Err("Mounted Drive file expanded during export".into());
            }
            zip.write_all(&buffer[..n])
                .map_err(|e| format!("ZIP output write failed: {e}"))?;
            hash.update(&buffer[..n]);
            job.update(|p| p.bytes_written = p.bytes_written.saturating_add(n as u64));
        }
        if count != item.size {
            return Err("Mounted Drive file ended early; partial ZIP will be discarded".into());
        }
        let digest = format!("{:x}", hash.finalize());
        manifest.files.push(ArchiveEntry {
            entry_id: item.record.entry_id.clone(),
            path: item.archive_path.clone(),
            bytes: count,
            sha256: digest,
            verified_in_export_zip: true,
        });
        check_fresh()?;
        job.update(|p| {
            p.current = index + 1;
        });
    }
    job.check()?;
    check_fresh()?;
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if manifest_bytes.len() > MAX_MANIFEST {
        return Err("Export manifest exceeded bounded size".into());
    }
    zip.start_file("manifest.json", options)
        .map_err(|e| e.to_string())?;
    zip.write_all(&manifest_bytes).map_err(|e| e.to_string())?;
    let final_file = zip.finish().map_err(|e| e.to_string())?;
    final_file.sync_all().map_err(|e| e.to_string())?;
    drop(final_file);
    job.check()?;
    // A successful original FUSE read is not enough to call the resulting
    // archive verified. Re-read and SHA-256-check the actual local ZIP bytes.
    verify_written_archive(&temporary, &manifest, job)?;
    job.check()?;
    check_fresh()?;
    // Publish only completed, synced archives without clobbering a late file.
    fs::hard_link(&temporary, destination)
        .map_err(|e| format!("Cannot safely publish ZIP: {e}"))?;
    guard.0.push(destination.to_path_buf());
    fs::remove_file(&temporary).map_err(|e| e.to_string())?;
    guard.0.clear();
    Ok(())
}

/// Read the newly written ZIP back and compare every member to the streamed
/// SHA-256. This checks the *exported archive*, not the Drive content_fingerprint
/// (which is a keyed/public chunk-manifest fingerprint, NOT file SHA-256).
fn verify_written_archive(
    temporary: &Path,
    manifest: &ArchiveManifest,
    job: &Arc<ExportJob>,
) -> Result<(), String> {
    let input = File::open(temporary).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(input).map_err(|e| format!("ZIP re-open failed: {e}"))?;
    if zip.len() != manifest.files.len() + manifest.folders.len() + 1 {
        return Err("Export ZIP has an unexpected number of entries".into());
    }
    for folder in &manifest.folders {
        job.check()?;
        let entry = zip
            .by_name(folder)
            .map_err(|e| format!("ZIP folder verification failed: {e}"))?;
        if !entry.is_dir() || entry.size() != 0 {
            return Err("ZIP directory entry is missing, nonempty or not a directory".into());
        }
    }
    let mut buffer = [0u8; BUFFER_SIZE];
    for (index, expected) in manifest.files.iter().enumerate() {
        job.check()?;
        let mut reader = zip
            .by_name(&expected.path)
            .map_err(|e| format!("ZIP member verification failed: {e}"))?;
        if reader.size() != expected.bytes || reader.is_dir() {
            return Err("ZIP member size or type changed after writing".into());
        }
        let mut hash = Sha256::new();
        let mut count = 0u64;
        loop {
            job.check()?;
            let n = reader
                .read(&mut buffer)
                .map_err(|e| format!("ZIP re-read failed: {e}"))?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n as u64)
                .ok_or("ZIP verification size overflow")?;
            if count > expected.bytes {
                return Err("ZIP member exceeded planned size".into());
            }
            hash.update(&buffer[..n]);
        }
        if count != expected.bytes || format!("{:x}", hash.finalize()) != expected.sha256 {
            return Err("Export ZIP checksum verification failed; discarded".into());
        }
        job.update(|p| p.files_verified = index + 1);
    }
    let manifest_member = zip
        .by_name("manifest.json")
        .map_err(|e| format!("ZIP manifest cannot be reopened: {e}"))?;
    let serialized = serde_json::to_vec(manifest).map_err(|e| e.to_string())?;
    if manifest_member.size() != serialized.len() as u64 || serialized.len() > MAX_MANIFEST {
        return Err("Export ZIP manifest length mismatch".into());
    }
    let mut actual = Vec::new();
    manifest_member
        .take(MAX_MANIFEST as u64 + 1)
        .read_to_end(&mut actual)
        .map_err(|e| e.to_string())?;
    if actual != serialized {
        return Err("Export ZIP manifest changed after writing".into());
    }
    Ok(())
}

fn fresh_job(count: usize, size: u64) -> Arc<ExportJob> {
    Arc::new(ExportJob {
        cancelled: AtomicBool::new(false),
        progress: Mutex::new(ExportProgress {
            id: format!("export-{:032x}", rand::random::<u128>()),
            phase: "running",
            current: 0,
            count,
            bytes_written: 0,
            total_bytes: size,
            files_verified: 0,
            error: None,
        }),
    })
}

/// Collect portable directory entries for *all* descendants, including empty
/// folders. These names are written and verified in the ZIP, not reconstructed
/// from a partial list of file paths.
fn collect_folder_zip_paths(
    snapshot: &DriveSnapshot,
    source_id: &str,
) -> Result<Vec<String>, String> {
    let dirs: HashMap<&str, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if !valid_id(source_id) || !dirs.contains_key(source_id) {
        return Err("Selected ZIP folder is unavailable".into());
    }
    let mut children: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    for dir in dirs.values() {
        children
            .entry(dir.parent_id.as_deref())
            .or_default()
            .push(dir.id.as_str());
    }
    let mut stack = vec![source_id];
    let mut seen = HashSet::new();
    let mut portable = HashSet::new();
    let mut portable_by_id = HashMap::new();
    let mut entries = Vec::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            return Err("Folder ZIP source hierarchy contains a cycle".into());
        }
        if seen.len() > 512 {
            return Err("Folder ZIP supports at most 512 nested directories".into());
        }
        let mut names = Vec::new();
        let mut chain = HashSet::new();
        let mut cursor = Some(id);
        while let Some(parent_id) = cursor {
            if !chain.insert(parent_id) || chain.len() > 100 {
                return Err("Folder ZIP contains cyclic or excessively deep ancestry".into());
            }
            let folder = dirs
                .get(parent_id)
                .ok_or("ZIP folder has a missing active parent")?;
            validate_drive_name(&folder.name)
                .map_err(|_| "ZIP folder contains an unsafe directory name")?;
            names.push(folder.name.as_str());
            cursor = folder.parent_id.as_deref();
        }
        names.reverse();
        let folder_path = format!("DriveFiles/{}/", names.join("/"));
        if folder_path.len() > 4096 {
            return Err("ZIP directory path exceeds the 4096-byte safety limit".into());
        }
        let folded = names
            .iter()
            .map(|n| portable_name_key(n))
            .collect::<Vec<_>>()
            .join("/");
        if !portable.insert(folded.clone()) {
            return Err("Folder ZIP has case-insensitive directory path collisions".into());
        }
        portable_by_id.insert(id, folded);
        entries.push(folder_path);
        if let Some(nested) = children.get(&Some(id)) {
            stack.extend(nested.iter().copied());
        }
    }
    for file in &snapshot.files {
        if let Some(parent) = file
            .directory_id
            .as_deref()
            .and_then(|id| portable_by_id.get(id))
        {
            let key = format!("{}/{}", parent, portable_name_key(&file.display_name));
            if portable.contains(&key) {
                return Err("A Drive file and folder share the same portable archive path".into());
            }
        }
    }
    entries.sort();
    Ok(entries)
}

/// Verify that a recursive-folder ZIP selection contains EVERY live raw entry
/// in the requested subtree. drive_snapshot intentionally hides unavailable
/// backing files and pending COW bases: these MUST invalidate completeness.
fn check_folder_zip_completeness(
    conn: &sqlite::Connection,
    snapshot: &DriveSnapshot,
    folder_id: &str,
    selected_ids: &[String],
) -> Result<(), String> {
    if !valid_id(folder_id) || selected_ids.len() > MAX_FILES {
        return Err("Invalid recursive Drive folder ZIP selection".into());
    }
    let dirs: HashMap<&str, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if !dirs.contains_key(folder_id) {
        return Err("Source Drive folder was removed or trashed".into());
    }
    let mut children: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    for d in dirs.values() {
        children
            .entry(d.parent_id.as_deref())
            .or_default()
            .push(d.id.as_str());
    }
    let mut subtree = HashSet::new();
    let mut stack = vec![folder_id];
    while let Some(id) = stack.pop() {
        if !subtree.insert(id) {
            return Err("Recursive ZIP folder ancestry is cyclic or ambiguous".into());
        }
        if subtree.len() > 25000 {
            return Err("Recursive Drive folder ZIP tree is too large".into());
        }
        if let Some(nested) = children.get(&Some(id)) {
            stack.extend(nested.iter().copied());
        }
    }
    let selected: HashSet<&str> = selected_ids.iter().map(String::as_str).collect();
    if selected.len() != selected_ids.len() {
        return Err("Repeated completed-file identity in folder ZIP selection".into());
    }
    let mut visible = HashSet::new();
    for f in &snapshot.files {
        if f.directory_id
            .as_deref()
            .is_some_and(|p| subtree.contains(p))
            && (!selected.contains(f.entry_id.as_str()) || !visible.insert(f.entry_id.as_str()))
        {
            return Err(
                "Folder ZIP omits a visible file or has an ambiguous V1/V2 identity".into(),
            );
        }
    }
    if visible.len() != selected.len() {
        return Err("Folder ZIP includes files outside the selected subtree".into());
    }
    if snapshot.pending.iter().any(|p| {
        p.directory_id
            .as_deref()
            .is_some_and(|id| subtree.contains(id))
            || p.base_entry_id
                .as_deref()
                .is_some_and(|id| selected.contains(id))
    }) {
        return Err("Folder ZIP contains unfinished file changes".into());
    }
    if snapshot.symlinks.iter().any(|l| {
        l.trashed_at.is_none()
            && l.directory_id
                .as_deref()
                .is_some_and(|id| subtree.contains(id))
    }) {
        return Err("Folder ZIP contains symlinks unsupported by verified content export".into());
    }
    // Critically, scan raw active metadata, not only drive_snapshot. If a
    // backing legacy file is missing or a V2 object is unavailable, its entry
    // is still here even though it vanished from the visible completed list.
    // Scope the SQL walk to this subtree using existing parent indexes,
    // rather than scanning the entire Drive on every export recheck.
    // A live V2 entry supersedes a migrated legacy fallback with the same ID.
    let mut raw = conn
        .prepare(
            "WITH RECURSIVE scoped(id) AS (
            SELECT id FROM drive_directories WHERE id=?1 AND trashed_at IS NULL
            UNION
            SELECT child.id FROM drive_directories child
                JOIN scoped parent ON child.parent_id=parent.id
                WHERE child.trashed_at IS NULL
         )
         SELECT e.entry_id, e.directory_id,
            EXISTS(SELECT 1 FROM logical_files f
                JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                WHERE f.file_id=e.file_id AND f.status='complete')
         FROM drive_file_entries e
         WHERE e.trashed_at IS NULL AND e.directory_id IN (SELECT id FROM scoped)
            AND NOT EXISTS(SELECT 1 FROM drive_entries_v2 v
                WHERE v.entry_id=e.entry_id AND v.trashed_at IS NULL)
         UNION ALL
         SELECT e.entry_id, e.directory_id,
            EXISTS(SELECT 1 FROM drive_objects o WHERE o.object_id=e.object_id)
         FROM drive_entries_v2 e
         WHERE e.trashed_at IS NULL AND e.directory_id IN (SELECT id FROM scoped)
         LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    raw.bind((1, folder_id)).map_err(|e| e.to_string())?;
    raw.bind((2, (MAX_FILES + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut seen_raw = HashSet::new();
    while let sqlite::State::Row = raw.next().map_err(|e| e.to_string())? {
        if seen_raw.len() >= MAX_FILES {
            return Err("Recursive ZIP folder exceeds the 100-file safety limit".into());
        }
        let id = raw.read::<String, _>(0).map_err(|e| e.to_string())?;
        let parent = raw.read::<sqlite::Value, _>(1).map_err(|e| e.to_string())?;
        let backing_complete = raw.read::<i64, _>(2).map_err(|e| e.to_string())? == 1;
        let inside = match parent {
            sqlite::Value::Null => false,
            sqlite::Value::String(p) => subtree.contains(p.as_str()),
            _ => return Err("Invalid Drive raw folder entry parent".into()),
        };
        if !inside
            || !seen_raw.insert(id.clone())
            || !selected.contains(id.as_str())
            || !backing_complete
        {
            return Err("Folder ZIP has a hidden, duplicate or unavailable live file entry".into());
        }
    }
    if seen_raw.len() != selected.len() {
        return Err("Folder ZIP raw entries differ from selected completed files".into());
    }
    Ok(())
}

#[tauri::command]
pub fn cmd_drive_bulk_export_status() -> Result<Option<ExportProgress>, String> {
    let active = EXPORT
        .lock()
        .map_err(|_| "Drive export state unavailable".to_string())?;
    active.as_ref().map(|job| job.snapshot()).transpose()
}

#[tauri::command]
pub fn cmd_drive_bulk_export_cancel(export_id: String) -> Result<bool, String> {
    let active = EXPORT
        .lock()
        .map_err(|_| "Drive export state unavailable".to_string())?;
    let job = active.as_ref().ok_or("No Drive export is active")?;
    if job.snapshot()?.id != export_id {
        return Err("Drive export identity changed; refresh status".into());
    }
    let running = job.snapshot()?.phase == "running";
    if running {
        job.cancelled.store(true, Ordering::SeqCst);
    }
    Ok(running)
}

#[tauri::command]
pub fn cmd_drive_bulk_export_start(
    app: AppHandle,
    db: State<'_, DbConnection>,
    mount: State<'_, DriveMountState>,
    entry_ids: Vec<String>,
    destination_path: String,
    folder_id: Option<String>,
    folder_revision: Option<i64>,
) -> Result<ExportProgress, String> {
    if destination_path.is_empty() || destination_path.len() > 4096 {
        return Err("Select a valid ZIP destination".into());
    }
    let context = mount
        .restore_context()?
        .ok_or("Linux Drive mount is not active")?;
    let destination = PathBuf::from(destination_path);
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let pool = db.inner().clone();
    let (items, revision, folder_paths) = {
        let conn = pool
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        let snapshot = drive_snapshot(&conn)?;
        // Explicit folder mode may export an entirely empty directory tree.
        // Manual file-only export still requires at least one selected file.
        let chosen = if entry_ids.is_empty() && folder_id.is_some() {
            Vec::new()
        } else {
            select(&snapshot, &entry_ids)?
        };
        let mut folder_paths = Vec::new();
        match (&folder_id, folder_revision) {
            (Some(folder), Some(expected)) if expected == snapshot.revision => {
                check_folder_zip_completeness(&conn, &snapshot, folder, &entry_ids)?;
                folder_paths = collect_folder_zip_paths(&snapshot, folder)?;
            }
            (None, None) => {}
            _ => return Err("Recursive Drive ZIP selection became stale; refresh it".into()),
        }
        (chosen, snapshot.revision, folder_paths)
    };
    destination_guard(&destination, &context.1, &cache, &app_data)?;
    let total = items.iter().map(|p| p.size).sum::<u64>();
    let mut lock = EXPORT
        .lock()
        .map_err(|_| "Drive export state unavailable".to_string())?;
    if lock
        .as_ref()
        .is_some_and(|job| job.snapshot().map(|p| p.phase == "running").unwrap_or(true))
    {
        return Err("Another Drive ZIP export is already running".into());
    }
    let job = fresh_job(items.len(), total);
    let initial = job.snapshot()?;
    *lock = Some(job.clone());
    drop(lock);
    let mount = mount.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let verify = || {
            if mount.restore_context()? != Some(context.clone()) {
                return Err("Drive mount changed during ZIP export; aborted".into());
            }
            check_snapshot(&pool, revision, &items)?;
            if let Some(folder) = folder_id.as_deref() {
                let conn = pool
                    .lock()
                    .map_err(|_| "Drive database unavailable".to_string())?;
                let snapshot = drive_snapshot(&conn)?;
                if snapshot.revision != revision {
                    return Err("Recursive Drive ZIP snapshot changed during export".into());
                }
                check_folder_zip_completeness(&conn, &snapshot, folder, &entry_ids)?;
                if collect_folder_zip_paths(&snapshot, folder)? != folder_paths {
                    return Err("Drive directory structure changed during folder ZIP export".into());
                }
            }
            Ok(())
        };
        let result = export_files_with_folders(
            &context.1,
            &items,
            revision,
            (&destination, &cache, &app_data),
            &job,
            &folder_paths,
            verify,
        );
        job.update(|status| match result {
            Ok(()) => status.phase = "done",
            Err(problem) => {
                status.phase = if job.cancelled.load(Ordering::SeqCst) {
                    "cancelled"
                } else {
                    "failed"
                };
                status.error = Some(problem);
            }
        });
    });
    Ok(initial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::{DriveDirectoryRecord, DriveSnapshot};

    fn fixture() -> (PathBuf, DriveSnapshot, Vec<String>) {
        let root =
            std::env::temp_dir().join(format!("trdrive-export-{:032x}", rand::random::<u128>()));
        fs::create_dir_all(root.join("Projects/Docs")).unwrap();
        fs::write(root.join("Projects/Docs/report.txt"), b"verified-content").unwrap();
        fs::write(root.join("Projects/Docs/other.txt"), b"second-content").unwrap();
        let mut snapshot = DriveSnapshot {
            revision: 11,
            dirty: false,
            directories: Vec::new(),
            files: Vec::new(),
            pending: Vec::new(),
            symlinks: Vec::new(),
        };
        snapshot.directories.push(DriveDirectoryRecord {
            id: "projects".into(),
            parent_id: None,
            name: "Projects".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        snapshot.directories.push(DriveDirectoryRecord {
            id: "docs".into(),
            parent_id: Some("projects".into()),
            name: "Docs".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        let ids: Vec<String> = vec!["entry-1".into(), "entry-2".into()];
        for (index, name) in ["report.txt", "other.txt"].iter().enumerate() {
            let bytes = fs::read(root.join("Projects/Docs").join(name)).unwrap();
            snapshot.files.push(DriveFileRecord {
                entry_id: ids[index].clone(),
                file_id: format!("file-{}", index + 1),
                inode_id: None,
                directory_id: Some("docs".into()),
                display_name: (*name).into(),
                total_size: bytes.len() as u64,
                mime_type: None,
                first_message_id: 1,
                backing_channel_id: 1,
                logical_channel_id: "logical".into(),
                created_at: 1,
                updated_at: 1,
                encryption_version: 0,
                plaintext_size: None,
                crypto_id: None,
                // Drive's content fingerprint is a hash of chunk metadata,
                // not a whole-file SHA. Prove export never conflates them.
                content_fingerprint: Some("1".repeat(64)),
                storage_version: 2,
            });
        }
        (root, snapshot, ids)
    }
    fn temp_output(root: &Path) -> PathBuf {
        root.with_extension("zip")
    }
    fn cleanup(root: PathBuf) {
        let _ = fs::remove_file(temp_output(&root));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_completed_files_stream_to_verified_portable_zip_with_manifest() {
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        let job = fresh_job(items.len(), items.iter().map(|p| p.size).sum());
        let destination = temp_output(&root);
        export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || Ok(()),
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&destination).unwrap()).unwrap();
        assert_eq!(zip.len(), 3);
        let mut first = Vec::new();
        zip.by_name("DriveFiles/Projects/Docs/report.txt")
            .unwrap()
            .read_to_end(&mut first)
            .unwrap();
        assert_eq!(first, b"verified-content");
        let mut manifest = String::new();
        zip.by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert!(manifest.contains("verified_in_export_zip"));
        assert_eq!(job.snapshot().unwrap().files_verified, 2);
        cleanup(root);
    }
    #[test]
    fn newly_written_archive_must_rehash_each_member_before_publication() {
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        let job = fresh_job(items.len(), items.iter().map(|p| p.size).sum());
        let destination = temp_output(&root);
        export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || Ok(()),
        )
        .unwrap();
        let good = ArchiveManifest {
            format: "terarelay-completed-drive-export-v1",
            revision: snapshot.revision,
            folders: Vec::new(),
            files: items
                .iter()
                .map(|item| {
                    let bytes = fs::read(root.join(item.names.join("/"))).unwrap();
                    ArchiveEntry {
                        entry_id: item.record.entry_id.clone(),
                        path: item.archive_path.clone(),
                        bytes: item.size,
                        sha256: format!("{:x}", Sha256::digest(&bytes)),
                        verified_in_export_zip: true,
                    }
                })
                .collect(),
        };
        assert!(verify_written_archive(&destination, &good, &job).is_ok());
        let mut forged = good;
        forged.files[0].sha256 = "0".repeat(64);
        assert!(verify_written_archive(&destination, &forged, &job).is_err());
        cleanup(root);
    }

    #[test]
    fn cancelled_export_never_publishes_any_partial_zip() {
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        let job = fresh_job(items.len(), 42);
        let destination = temp_output(&root);
        job.cancelled.store(true, Ordering::SeqCst);
        assert!(export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || Ok(())
        )
        .is_err());
        assert!(!destination.exists());
        cleanup(root);
    }
    #[test]
    fn source_size_mismatch_prevents_publication_and_preserves_existing_output() {
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        fs::write(root.join("Projects/Docs/report.txt"), b"altered-content").unwrap();
        let destination = temp_output(&root);
        let job = fresh_job(items.len(), 42);
        assert!(export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || Ok(())
        )
        .is_err());
        assert!(!destination.exists());
        fs::write(&destination, b"do-not-clobber").unwrap();
        assert!(export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || Ok(())
        )
        .is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"do-not-clobber");
        cleanup(root);
    }
    #[test]
    fn stale_revision_causes_total_rollback_even_after_first_file() {
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        let destination = temp_output(&root);
        let job = fresh_job(items.len(), 42);
        let called = std::cell::Cell::new(0);
        assert!(export_files(
            &root,
            &items,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || {
                called.set(called.get() + 1);
                if called.get() > 2 {
                    Err("changed Drive revision".into())
                } else {
                    Ok(())
                }
            }
        )
        .is_err());
        assert!(!destination.exists());
        cleanup(root);
    }
    #[test]
    fn same_path_pending_overlay_appearing_mid_export_retracts_whole_archive() {
        use std::cell::{Cell, RefCell};
        let (root, snapshot, ids) = fixture();
        let items = select(&snapshot, &ids).unwrap();
        let revision = snapshot.revision;
        let mutable_snapshot = RefCell::new(snapshot);
        let calls = Cell::new(0usize);
        let destination = temp_output(&root);
        let job = fresh_job(items.len(), items.iter().map(|item| item.size).sum());
        let result = export_files(
            &root,
            &items,
            revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 3 {
                    // Simulate a separate pending creation that does not bump
                    // the visible Drive metadata revision. No base ID is set.
                    mutable_snapshot.borrow_mut().pending.push(
                        crate::commands::drive_metadata::DrivePendingRecord {
                            id: "late-pending".into(),
                            directory_id: Some("docs".into()),
                            display_name: "report.txt".into(),
                            staging_path: "not-accessed".into(),
                            size: 16,
                            format_version: 2,
                            backing_channel_id: 1,
                            base_entry_id: None,
                            encryption_version: 2,
                            crypto_id: None,
                            closed_at: None,
                            created_at: 1,
                            updated_at: 1,
                        },
                    );
                }
                if pending_shadows_file(&mutable_snapshot.borrow(), &items[0].record) {
                    Err("A pending write shadows an exported path".into())
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(mutable_snapshot.borrow().revision, revision);
        assert!(
            !destination.exists(),
            "Late pending overlay cannot publish a ZIP"
        );
        cleanup(root);
    }

    fn completed_folder_test_db() -> sqlite::Connection {
        use crate::commands::drive_metadata::init_drive_schema;
        let db = sqlite::open(":memory:").unwrap();
        db.execute(
            "CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER UNIQUE,
                name TEXT, role TEXT, storage_version INTEGER, created_at INTEGER, joined_at INTEGER);
             CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY, logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL, display_name TEXT NOT NULL,
                total_size INTEGER NOT NULL, mime_type TEXT, chunk_count INTEGER NOT NULL DEFAULT 1,
                whole_sha256 TEXT, storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');",
        ).unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES ('folder',NULL,'Archive',1,1),('nested','folder','Docs',1,1);
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES ('qa',19,'QA','owner',1,1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
               total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('body','qa',1,'report.txt',12,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES ('entry','body','nested','report.txt',1,1);",
        ).unwrap();
        db
    }
    #[test]
    fn recursive_zip_exports_directory_only_tree_without_file_content() {
        let db = completed_folder_test_db();
        db.execute("DELETE FROM drive_file_entries WHERE entry_id='entry'")
            .unwrap();
        let snap = drive_snapshot(&db).unwrap();
        assert!(snap.files.is_empty());
        check_folder_zip_completeness(&db, &snap, "folder", &[]).unwrap();
        let dirs = collect_folder_zip_paths(&snap, "folder").unwrap();
        assert_eq!(dirs.len(), 2);
        let (root, _, _) = fixture();
        let destination = temp_output(&root);
        let job = fresh_job(0, 0);
        export_files_with_folders(
            &root,
            &[],
            snap.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            &dirs,
            || Ok(()),
        )
        .unwrap();
        let file = File::open(&destination).unwrap();
        let zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), dirs.len() + 1);
        assert_eq!(job.snapshot().unwrap().files_verified, 0);
        cleanup(root);
    }
    #[test]
    fn recursive_zip_preserves_empty_directories_and_verifies_archive() {
        let (root, snapshot, ids) = fixture();
        let selected = select(&snapshot, &ids).unwrap();
        let empty = root.join("Projects/Empty");
        fs::create_dir_all(&empty).unwrap();
        let folders = vec![
            "DriveFiles/Projects/".to_string(),
            "DriveFiles/Projects/Docs/".to_string(),
            "DriveFiles/Projects/Empty/".to_string(),
        ];
        let destination = temp_output(&root);
        let total: u64 = selected.iter().map(|f| f.size).sum();
        let job = fresh_job(selected.len(), total);
        export_files_with_folders(
            &root,
            &selected,
            snapshot.revision,
            (&destination, &root.join("cache"), &root.join("data")),
            &job,
            &folders,
            || Ok(()),
        )
        .unwrap();
        let input = File::open(&destination).unwrap();
        let mut zip = zip::ZipArchive::new(input).unwrap();
        assert_eq!(zip.len(), selected.len() + folders.len() + 1);
        let entry = zip.by_name("DriveFiles/Projects/Empty/").unwrap();
        assert!(entry.is_dir());
        assert_eq!(entry.size(), 0);
        drop(entry);
        let mut bytes = Vec::new();
        zip.by_name("manifest.json")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest["folders"].as_array().unwrap().len(), 3);
        assert_eq!(
            manifest["format"],
            "terarelay-completed-drive-folder-export-v2"
        );
        drop(zip);
        cleanup(root);
    }
    #[test]
    fn recursive_zip_refuses_file_directory_collision_on_casefold_filesystems() {
        let (_root, mut snapshot, _ids) = fixture();
        let mut conflict = snapshot.files[0].clone();
        conflict.entry_id = "conflicting-file".into();
        conflict.directory_id = Some("projects".into());
        conflict.display_name = "DOCS".into();
        snapshot.files.push(conflict);
        assert!(collect_folder_zip_paths(&snapshot, "projects").is_err());
        // Fixture source data remains untouched; this is metadata-only QA.
        let _ = fs::remove_dir_all(_root);
    }
    #[test]
    fn recursive_zip_preflight_matches_complete_native_subtree() {
        let db = completed_folder_test_db();
        let snap = drive_snapshot(&db).unwrap();
        let ids = vec!["entry".to_string()];
        check_folder_zip_completeness(&db, &snap, "folder", &ids).unwrap();
        assert!(check_folder_zip_completeness(&db, &snap, "nested", &ids).is_ok());
        assert!(check_folder_zip_completeness(&db, &snap, "folder", &["other".into()]).is_err());
    }
    #[test]
    fn recursive_zip_refuses_hidden_unavailable_file_without_publishing_archive() {
        let db = completed_folder_test_db();
        db.execute("UPDATE logical_files SET status='missing' WHERE file_id='body'")
            .unwrap();
        let snap = drive_snapshot(&db).unwrap();
        assert!(snap.files.is_empty());
        assert!(check_folder_zip_completeness(&db, &snap, "folder", &["entry".into()]).is_err());
        // Even adding an otherwise visible file cannot hide the missing raw
        // entry inside the same subtree.
        db.execute(
            "INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,
             display_name,total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('available','qa',2,'ok.txt',8,'text/plain',1,'hash2',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('visible','available','nested','ok.txt',1,1);",
        ).unwrap();
        let snap = drive_snapshot(&db).unwrap();
        assert!(check_folder_zip_completeness(&db, &snap, "folder", &["visible".into()]).is_err());
    }
    #[test]
    fn recursive_zip_refuses_late_unselected_file_even_without_revision_bump() {
        let db = completed_folder_test_db();
        let before = drive_snapshot(&db).unwrap();
        check_folder_zip_completeness(&db, &before, "folder", &["entry".into()]).unwrap();
        db.execute(
            "INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,
             display_name,total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('late','qa',2,'late.txt',10,'text/plain',1,'hash2',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('late-entry','late','nested','late.txt',1,1);",
        ).unwrap();
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.revision, before.revision);
        assert!(check_folder_zip_completeness(&db, &after, "folder", &["entry".into()]).is_err());
    }
    #[test]
    fn recursive_zip_scoped_preflight_ignores_large_unrelated_folder() {
        let db = completed_folder_test_db();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('unrelated',NULL,'Unrelated',1,1);",
        )
        .unwrap();
        for index in 0..150 {
            let sql = format!(
                "INSERT INTO drive_file_entries(
                    entry_id,file_id,directory_id,display_name,created_at,updated_at
                ) VALUES('unrelated-{index}','body','unrelated','file-{index}.txt',1,1);"
            );
            db.execute(&sql).unwrap();
        }
        let snap = drive_snapshot(&db).unwrap();
        check_folder_zip_completeness(&db, &snap, "folder", &["entry".into()]).unwrap();
        assert!(
            check_folder_zip_completeness(&db, &snap, "unrelated", &["unrelated-1".into()])
                .is_err()
        );
    }
    #[test]
    fn recursive_zip_accepts_authoritative_v2_over_missing_legacy_fallback() {
        let db = completed_folder_test_db();
        db.execute(
            "UPDATE logical_files SET status='missing' WHERE file_id='body';
             INSERT INTO drive_objects
                 (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
                 VALUES('object-v2',12,0,NULL,'v2fingerprint',0,1);
             INSERT INTO drive_entries_v2
                 (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
                 VALUES('entry','object-v2',NULL,'nested','report.txt',1,2,NULL);",
        ).unwrap();
        let snap = drive_snapshot(&db).unwrap();
        assert!(snap
            .files
            .iter()
            .any(|f| f.entry_id == "entry" && f.storage_version == 2));
        check_folder_zip_completeness(&db, &snap, "folder", &["entry".into()]).unwrap();
    }
    #[test]
    fn recursive_zip_refuses_hidden_copy_on_write_base() {
        let db = completed_folder_test_db();
        db.execute(
            "INSERT INTO drive_pending_files
             (id,directory_id,display_name,staging_path,size,format_version,
              backing_channel_id,base_entry_id,created_at,updated_at)
             VALUES('cow','folder','other.txt','not-read',0,1,19,'entry',1,1);",
        )
        .unwrap();
        let snap = drive_snapshot(&db).unwrap();
        assert!(snap.files.is_empty());
        assert!(check_folder_zip_completeness(&db, &snap, "folder", &["entry".into()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_and_folder_are_never_followed_outside_selected_mount() {
        use std::os::unix::fs::symlink;
        let (root, snapshot, ids) = fixture();
        let outside = root.with_extension("outside");
        fs::write(&outside, b"private-host-file").unwrap();
        fs::remove_file(root.join("Projects/Docs/report.txt")).unwrap();
        symlink(&outside, root.join("Projects/Docs/report.txt")).unwrap();
        assert!(open_beneath(&root, &select(&snapshot, &ids).unwrap()[0].names).is_err());
        fs::remove_file(root.join("Projects/Docs/other.txt")).unwrap();
        fs::remove_dir_all(root.join("Projects/Docs")).unwrap();
        symlink(&outside, root.join("Projects/Docs")).unwrap();
        assert!(open_beneath(&root, &select(&snapshot, &ids).unwrap()[0].names).is_err());
        fs::remove_file(outside).unwrap();
        cleanup(root);
    }
    #[test]
    fn pending_shadow_and_portable_name_collisions_fail_before_io() {
        let (root, mut snapshot, ids) = fixture();
        let dup = snapshot.files[0].clone();
        snapshot.files[1].display_name = "REPORT.txt".into();
        assert!(select(&snapshot, &ids).is_err());
        snapshot.files[1].display_name = "other.txt".into();
        snapshot
            .pending
            .push(crate::commands::drive_metadata::DrivePendingRecord {
                id: "pending-shadow".into(),
                directory_id: None,
                display_name: "draft".into(),
                staging_path: "unsafe".into(),
                size: 1,
                format_version: 2,
                backing_channel_id: 1,
                base_entry_id: Some(dup.entry_id),
                encryption_version: 2,
                crypto_id: None,
                closed_at: None,
                created_at: 1,
                updated_at: 1,
            });
        assert!(select(&snapshot, &ids).is_err());
        // A pending write can shadow the same logical path WITHOUT a base
        // entry reference; selection and mid-export checks must both reject.
        snapshot.pending[0].base_entry_id = None;
        snapshot.pending[0].directory_id = Some("docs".into());
        snapshot.pending[0].display_name = "report.txt".into();
        assert!(pending_shadows_file(&snapshot, &snapshot.files[0]));
        assert!(select(&snapshot, &ids).is_err());
        snapshot.pending[0].display_name = "unrelated.txt".into();
        assert!(!pending_shadows_file(&snapshot, &snapshot.files[0]));
        cleanup(root);
    }
}
