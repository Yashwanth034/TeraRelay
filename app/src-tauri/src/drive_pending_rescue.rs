//! Explicit local-only emergency copy of CLOSED Drive staging bytes.
//! A pack preserves only locally retained chunks, not remotely uploaded bytes
//! or a full independently restorable Drive database. Never contacts Telegram.
use crate::commands::drive_metadata::{drive_pending_by_id, DrivePendingRecord};
use crate::db::DbConnection;
use crate::drive_mount::DriveMountState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const MAX_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 128 * 1024;
const PACK_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct RescueCandidate {
    pub id: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RescueResult {
    pub pending_id: String,
    pub count: usize,
    pub bytes: u64,
    pub restored: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RescuePart {
    name: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RescueManifest {
    version: u32,
    pending_id: String,
    directory_id: Option<String>,
    display_name: String,
    size: u64,
    format_version: u32,
    backing_channel_id: i64,
    base_entry_id: Option<String>,
    encryption_version: u32,
    crypto_id: Option<String>,
    created_at: i64,
    updated_at: i64,
    files: Vec<RescuePart>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_part_name(name: &str, pending: &DrivePendingRecord) -> bool {
    if name == ".pending"
        || Path::new(&pending.staging_path)
            .file_name()
            .and_then(|n| n.to_str())
            == Some(name)
    {
        return name != "." && name != ".." && !name.contains('/') && !name.contains('\\');
    }
    // Stage filenames are byte-defined ASCII. Never slice a UTF-8 string at
    // arbitrary byte offsets: a corrupt on-disk name or ZIP manifest could
    // otherwise panic the native rescue worker.
    name.len() == 27
        && name.starts_with("plain-")
        && name.ends_with(".part")
        && name.as_bytes()[6..22].iter().all(|b| b.is_ascii_hexdigit())
}

fn match_manifest(manifest: &RescueManifest, pending: &DrivePendingRecord) -> bool {
    manifest.version == PACK_VERSION
        && manifest.pending_id == pending.id
        && manifest.directory_id == pending.directory_id
        && manifest.display_name == pending.display_name
        && manifest.size == pending.size
        && manifest.format_version == pending.format_version
        && manifest.backing_channel_id == pending.backing_channel_id
        && manifest.base_entry_id == pending.base_entry_id
        && manifest.encryption_version == pending.encryption_version
        && manifest.crypto_id == pending.crypto_id
        && manifest.created_at == pending.created_at
        && manifest.updated_at == pending.updated_at
        && pending.closed_at.is_some()
}

fn validate_parts(manifest: &RescueManifest, pending: &DrivePendingRecord) -> Result<u64, String> {
    if !match_manifest(manifest, pending)
        || manifest.files.is_empty()
        || manifest.files.len() > MAX_FILES
    {
        return Err("Rescue pack does not match the existing closed pending upload".into());
    }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for part in &manifest.files {
        if !valid_part_name(&part.name, pending)
            || !seen.insert(&part.name)
            || part.bytes > MAX_FILE_BYTES
            || part.sha256.len() != 64
            || !part.sha256.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err("Rescue pack contains an invalid or duplicate stage entry".into());
        }
        total = total
            .checked_add(part.bytes)
            .ok_or("Rescue byte count overflow")?;
        if total > MAX_PACK_BYTES {
            return Err("Rescue pack exceeds the safe local backup limit".into());
        }
    }
    Ok(total)
}

fn stage_dir(cache_root: &Path, pending: &DrivePendingRecord) -> Result<PathBuf, String> {
    if !valid_id(&pending.id) || pending.closed_at.is_none() {
        return Err("Only valid closed pending uploads can be rescued".into());
    }
    crate::drive_stream::owned_finalized_staging_dir(cache_root, pending)
        .map_err(|e| format!("Unsafe or missing Drive stage: {e}"))
}

fn open_stage_read(path: &Path) -> Result<File, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("Stage contains a symlink, special file or oversized entry".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("Stage file permissions permit access by another user".into());
        }
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
        }
        .map_err(|e| format!("Failed to open stage entry safely: {e}"))?;
        let current = file.metadata().map_err(|e| e.to_string())?;
        if !current.is_file() || current.ino() != metadata.ino() || current.dev() != metadata.dev()
        {
            return Err("Stage entry changed during secure open".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = File::open(path).map_err(|e| e.to_string())?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("Stage entry is not a regular file".into());
        }
        Ok(file)
    }
}

fn file_digest(mut reader: impl Read, limit: u64) -> Result<(u64, String), String> {
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or("Rescue length overflow")?;
        if bytes > limit {
            return Err("Rescue entry exceeds declared size limit".into());
        }
        hash.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn private_new_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|e| format!("Could not create new rescue file: {e}"))
}

/// Removes only files this operation created on failed/incomplete work.
struct RemoveOnFailure {
    paths: Vec<PathBuf>,
    committed: bool,
}
impl RemoveOnFailure {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            committed: false,
        }
    }
}
impl Drop for RemoveOnFailure {
    fn drop(&mut self) {
        if !self.committed {
            for path in self.paths.iter().rev() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn inventory(
    pending: &DrivePendingRecord,
    cache_root: &Path,
) -> Result<Vec<(String, PathBuf, u64)>, String> {
    let root = stage_dir(cache_root, pending)?;
    let mut out = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if out.len() == MAX_FILES {
            return Err("Too many Drive stage files for one rescue pack".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "Non-UTF8 stage entry")?;
        if !valid_part_name(&name, pending) {
            return Err("Unexpected scratch or unsafe stage entry; rescue declined".into());
        }
        let path = entry.path();
        let file = open_stage_read(&path)?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        out.push((name, path, length));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    if out.is_empty() {
        return Err("Closed Drive stage has no locally retained entries".into());
    }
    let total = out
        .iter()
        .try_fold(0u64, |count, (_, _, bytes)| count.checked_add(*bytes))
        .ok_or("Rescue stage total overflow")?;
    if total > MAX_PACK_BYTES {
        return Err("Rescue stage is larger than 2 GiB".into());
    }
    Ok(out)
}

fn manifest_for(pending: &DrivePendingRecord, files: Vec<RescuePart>) -> RescueManifest {
    RescueManifest {
        version: PACK_VERSION,
        pending_id: pending.id.clone(),
        directory_id: pending.directory_id.clone(),
        display_name: pending.display_name.clone(),
        size: pending.size,
        format_version: pending.format_version,
        backing_channel_id: pending.backing_channel_id,
        base_entry_id: pending.base_entry_id.clone(),
        encryption_version: pending.encryption_version,
        crypto_id: pending.crypto_id.clone(),
        created_at: pending.created_at,
        updated_at: pending.updated_at,
        files,
    }
}

#[cfg(test)]
fn export_pack(
    pending: &DrivePendingRecord,
    cache_root: &Path,
    target: &Path,
) -> Result<RescueResult, String> {
    export_pack_checked(pending, cache_root, target, || Ok(()))
}

fn export_pack_checked(
    pending: &DrivePendingRecord,
    cache_root: &Path,
    target: &Path,
    verify_pending: impl Fn() -> Result<(), String>,
) -> Result<RescueResult, String> {
    verify_pending()?;
    let files = inventory(pending, cache_root)?;
    if target.file_name().is_none() || target.exists() {
        return Err("Choose a new rescue ZIP file; existing files cannot be overwritten".into());
    }
    let parent = target.parent().ok_or("Rescue destination has no parent")?;
    if !fs::symlink_metadata(parent)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("Rescue destination parent must be a directory".into());
    }
    // A backup inside the application's own cache can later be mistaken for
    // upload scratch, be removed by cache cleanup, or fill the staging budget.
    // Resolve symlinked parents before checking the restricted tree.
    let canonical_parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    let canonical_cache = fs::canonicalize(cache_root).map_err(|e| e.to_string())?;
    if canonical_parent.starts_with(&canonical_cache) {
        return Err("Choose a rescue destination outside the TeraRelay cache".into());
    }
    let mut guard = RemoveOnFailure::new();
    let temporary = parent.join(format!(
        ".trdrive-rescue-{:016x}.partial",
        rand::random::<u64>()
    ));
    let output = private_new_file(&temporary)?;
    guard.paths.push(temporary.clone());
    let mut zip = ZipWriter::new(output);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut parts = Vec::new();
    let mut total = 0u64;
    for (name, path, original_size) in files {
        let mut input = open_stage_read(&path)?;
        if input.metadata().map_err(|e| e.to_string())?.len() != original_size {
            return Err("Stage entry changed before backup".into());
        }
        zip.start_file(format!("stage/{name}"), opts)
            .map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut len = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            len = len.checked_add(n as u64).ok_or("Rescue length overflow")?;
            if len > original_size {
                return Err("Stage grew during backup".into());
            }
            hash.update(&buffer[..n]);
            zip.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        }
        if len != original_size {
            return Err("Stage shrank during backup".into());
        }
        total += len;
        parts.push(RescuePart {
            name,
            bytes: len,
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    let manifest = manifest_for(pending, parts);
    let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err("Rescue metadata exceeds limit".into());
    }
    zip.start_file("manifest.json", opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(&bytes).map_err(|e| e.to_string())?;
    let output = zip.finish().map_err(|e| e.to_string())?;
    output.sync_all().map_err(|e| e.to_string())?;
    // A different metadata workflow might have moved or deleted the pending
    // row while this user-triggered copy was running. Never publish a backup
    // that no longer describes the current closed pending entry.
    verify_pending()?;
    // hard_link is intentionally no-clobber: a late conflicting destination
    // must survive. A completed ZIP is made visible only after fsync.
    fs::hard_link(&temporary, target)
        .map_err(|e| format!("Cannot publish new rescue ZIP without overwrite: {e}"))?;
    // If removing the temporary link fails, also retract our newly created
    // target rather than reporting an incomplete export as a successful pack.
    guard.paths.push(target.to_path_buf());
    fs::remove_file(&temporary).map_err(|e| e.to_string())?;
    guard.committed = true;
    Ok(RescueResult {
        pending_id: pending.id.clone(),
        count: manifest.files.len(),
        bytes: total,
        restored: 0,
    })
}

fn read_manifest(
    zip: &mut ZipArchive<File>,
    pending: &DrivePendingRecord,
) -> Result<(RescueManifest, u64), String> {
    if zip.len() < 2 || zip.len() > MAX_FILES + 1 {
        return Err("Rescue archive file count invalid".into());
    }
    let mut source = zip
        .by_name("manifest.json")
        .map_err(|_| "Rescue manifest is missing")?;
    if source.size() > MANIFEST_LIMIT {
        return Err("Rescue manifest too large".into());
    }
    let mut bytes = Vec::new();
    source
        .by_ref()
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    drop(source);
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err("Rescue manifest too large".into());
    }
    let manifest: RescueManifest =
        serde_json::from_slice(&bytes).map_err(|_| "Rescue manifest is corrupt")?;
    let total = validate_parts(&manifest, pending)?;
    if zip.len() != manifest.files.len() + 1 {
        return Err("Rescue archive has extra files".into());
    }
    for part in &manifest.files {
        let entry = zip
            .by_name(&format!("stage/{}", part.name))
            .map_err(|_| "Rescue stage entry missing")?;
        if entry.size() != part.bytes || entry.is_dir() {
            return Err("Rescue stage size/type is invalid".into());
        }
    }
    Ok((manifest, total))
}

fn read_pack_file(path: &Path) -> Result<File, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.len() > MAX_PACK_BYTES + 16 * 1024 * 1024
    {
        return Err("Rescue source is not a valid ordinary ZIP file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        File::open(path).map_err(|e| e.to_string())
    }
}

fn verify_archive(zip: &mut ZipArchive<File>, manifest: &RescueManifest) -> Result<(), String> {
    for part in &manifest.files {
        let reader = zip
            .by_name(&format!("stage/{}", part.name))
            .map_err(|e| e.to_string())?;
        let (len, hash) = file_digest(reader, part.bytes)?;
        if len != part.bytes || hash != part.sha256 {
            return Err("Rescue ZIP staging checksum mismatch".into());
        }
    }
    Ok(())
}

#[cfg(test)]
fn restore_pack(
    pending: &DrivePendingRecord,
    cache_root: &Path,
    source: &Path,
) -> Result<RescueResult, String> {
    restore_pack_checked(pending, cache_root, source, || Ok(()))
}

fn restore_pack_checked(
    pending: &DrivePendingRecord,
    cache_root: &Path,
    source: &Path,
    verify_pending: impl Fn() -> Result<(), String>,
) -> Result<RescueResult, String> {
    verify_pending()?;
    let root = stage_dir(cache_root, pending)?;
    let mut zip =
        ZipArchive::new(read_pack_file(source)?).map_err(|_| "Rescue ZIP cannot be opened")?;
    let (manifest, total) = read_manifest(&mut zip, pending)?;
    // Check every digest before creating a SINGLE file in real staging.
    verify_archive(&mut zip, &manifest)?;
    let mut missing = Vec::new();
    for part in &manifest.files {
        let path = root.join(&part.name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let reader = open_stage_read(&path)?;
                let (bytes, hash) = file_digest(reader, part.bytes)?;
                if bytes != part.bytes || hash != part.sha256 {
                    return Err(
                        "Existing staging file differs; restore refuses to overwrite newer data"
                            .into(),
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(part),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut guard = RemoveOnFailure::new();
    for part in &missing {
        let path = root.join(&part.name);
        let temp = root.join(format!(".rescue-{:016x}.partial", rand::random::<u64>()));
        let mut output = private_new_file(&temp)?;
        guard.paths.push(temp.clone());
        let mut reader = zip
            .by_name(&format!("stage/{}", part.name))
            .map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut written = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            written = written
                .checked_add(n as u64)
                .ok_or("Rescue output size overflow")?;
            if written > part.bytes {
                return Err("ZIP entry expanded past the declared size".into());
            }
            output.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            hash.update(&buffer[..n]);
        }
        if written != part.bytes || format!("{:x}", hash.finalize()) != part.sha256 {
            return Err("Rescue content changed during restore".into());
        }
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        // Publish a fully written and synced chunk WITHOUT replacing an
        // existing file, including one created by another local process.
        fs::hard_link(&temp, &path).map_err(|e| format!("Rescue destination changed: {e}"))?;
        guard.paths.push(path);
        fs::remove_file(&temp).map_err(|e| e.to_string())?;
    }
    // Validation failure here also drops RemoveOnFailure and retracts only
    // stage files created by this rescue attempt, never preexisting bytes.
    verify_pending()?;
    guard.committed = true;
    Ok(RescueResult {
        pending_id: pending.id.clone(),
        count: manifest.files.len(),
        bytes: total,
        restored: missing.len(),
    })
}

/// The upload writer claim protects staged bytes. Recheck the durable row
/// separately because metadata-only organization can still change it while
/// a potentially large local pack is copied. Never keep SQLite locked during
/// archive I/O or while issuing a filesystem notification.
fn verify_pending_row(pool: &DbConnection, expected: &DrivePendingRecord) -> Result<(), String> {
    let conn = pool
        .lock()
        .map_err(|_| "Drive database unavailable during rescue".to_string())?;
    let current = drive_pending_by_id(&conn, &expected.id)?
        .ok_or("Drive pending upload disappeared during rescue")?;
    if !match_manifest(&manifest_for(expected, Vec::new()), &current)
        || current.closed_at != expected.closed_at
        || current.staging_path != expected.staging_path
    {
        return Err(
            "Drive pending metadata changed during rescue; no new data was retained".into(),
        );
    }
    Ok(())
}

#[tauri::command]
pub fn cmd_drive_rescue_candidates(
    db: State<'_, DbConnection>,
) -> Result<Vec<RescueCandidate>, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    let mut query = conn.prepare("SELECT id,display_name,size FROM drive_pending_files WHERE closed_at IS NOT NULL ORDER BY updated_at DESC LIMIT 101").map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while query.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if out.len() == 100 {
            return Err(
                "More than 100 pending uploads; narrow recovery to a smaller staging set".into(),
            );
        }
        let id = query.read::<String, _>(0).map_err(|e| e.to_string())?;
        let name = query.read::<String, _>(1).map_err(|e| e.to_string())?;
        let size = query.read::<i64, _>(2).map_err(|e| e.to_string())?;
        if !valid_id(&id) || name.is_empty() || size < 0 {
            return Err("Corrupt Drive pending inventory".into());
        }
        out.push(RescueCandidate {
            id,
            name,
            size: size as u64,
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn cmd_drive_rescue_export(
    app: AppHandle,
    db: State<'_, DbConnection>,
    mount: State<'_, DriveMountState>,
    pending_id: String,
    destination_path: String,
) -> Result<RescueResult, String> {
    if !valid_id(&pending_id) || destination_path.is_empty() || destination_path.len() > 4096 {
        return Err("Invalid rescue request".into());
    }
    let claim = mount
        .claim_pending_recovery(&pending_id)
        .ok_or("Drive pending upload is in use; try again after it stops")?;
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let pending = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        drive_pending_by_id(&conn, &pending_id)?.ok_or("Drive pending upload no longer exists")?
    };
    // The native worker, NOT the cancellable IPC future, must own the
    // exclusive writer/retry claim for the entire blocking filesystem copy.
    let pool = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _claim = claim;
        export_pack_checked(&pending, &cache, Path::new(&destination_path), || {
            verify_pending_row(&pool, &pending)
        })
    })
    .await
    .map_err(|_| "Drive rescue export worker failed".to_string())?
}

#[tauri::command]
pub async fn cmd_drive_rescue_restore(
    app: AppHandle,
    db: State<'_, DbConnection>,
    mount: State<'_, DriveMountState>,
    pending_id: String,
    source_path: String,
) -> Result<RescueResult, String> {
    if !valid_id(&pending_id) || source_path.is_empty() || source_path.len() > 4096 {
        return Err("Invalid rescue request".into());
    }
    let claim = mount
        .claim_pending_recovery(&pending_id)
        .ok_or("Drive pending upload is in use; try again after it stops")?;
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let pending = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        drive_pending_by_id(&conn, &pending_id)?.ok_or("Drive pending upload no longer exists")?
    };
    let pool = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Retain ownership after a frontend/webview cancellation too.
        let _claim = claim;
        restore_pack_checked(&pending, &cache, Path::new(&source_path), || {
            verify_pending_row(&pool, &pending)
        })
    })
    .await
    .map_err(|_| "Drive rescue restore worker failed".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, DrivePendingRecord, PathBuf) {
        let cache =
            std::env::temp_dir().join(format!("terarelay-rescue-{:016x}", rand::random::<u64>()));
        let stage = cache.join("drive/staging/rescue-fixture");
        fs::create_dir_all(&stage).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [
                &cache,
                &cache.join("drive"),
                &cache.join("drive/staging"),
                &stage,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let mut marker = private_new_file(&stage.join(".pending")).unwrap();
        marker.write_all(b"").unwrap();
        let mut chunk = private_new_file(&stage.join("plain-0000000000000000.part")).unwrap();
        chunk.write_all(b"secret-local-bytes").unwrap();
        let pending = DrivePendingRecord {
            id: "rescue-fixture".into(),
            directory_id: Some("folder-1".into()),
            display_name: "draft.txt".into(),
            staging_path: stage.join(".pending").to_string_lossy().into_owned(),
            size: 12345,
            format_version: 2,
            backing_channel_id: 123,
            base_entry_id: None,
            encryption_version: 2,
            crypto_id: Some("crypto-fixture".into()),
            closed_at: Some(456),
            created_at: 123,
            updated_at: 456,
        };
        (cache, pending, stage)
    }

    fn clean_fixture(cache: PathBuf) {
        let _ = fs::remove_file(cache.with_extension("zip"));
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn rescue_export_refuses_to_write_inside_app_cache() {
        let (cache, pending, _) = fixture();
        let forbidden = cache.join("rescue.zip");
        assert!(export_pack(&pending, &cache, &forbidden).is_err());
        assert!(!forbidden.exists());
        clean_fixture(cache);
    }

    #[test]
    fn rescue_pack_roundtrip_restores_only_missing_local_bytes_and_is_idempotent() {
        let (cache, pending, stage) = fixture();
        let zip = cache.with_extension("zip");
        let result = export_pack(&pending, &cache, &zip).unwrap();
        assert_eq!(result.count, 2);
        assert_eq!(result.bytes, 18);
        fs::remove_file(stage.join("plain-0000000000000000.part")).unwrap();
        let restored = restore_pack(&pending, &cache, &zip).unwrap();
        assert_eq!(restored.restored, 1);
        assert_eq!(
            fs::read(stage.join("plain-0000000000000000.part")).unwrap(),
            b"secret-local-bytes"
        );
        assert_eq!(restore_pack(&pending, &cache, &zip).unwrap().restored, 0);
        clean_fixture(cache);
    }

    #[test]
    fn rescue_never_overwrites_destination_or_changed_stage_content() {
        let (cache, pending, stage) = fixture();
        let zip = cache.with_extension("zip");
        fs::write(&zip, b"previous backup").unwrap();
        assert!(export_pack(&pending, &cache, &zip).is_err());
        assert_eq!(fs::read(&zip).unwrap(), b"previous backup");
        fs::remove_file(&zip).unwrap();
        export_pack(&pending, &cache, &zip).unwrap();
        fs::write(stage.join("plain-0000000000000000.part"), b"newer bytes").unwrap();
        assert!(restore_pack(&pending, &cache, &zip).is_err());
        assert_eq!(
            fs::read(stage.join("plain-0000000000000000.part")).unwrap(),
            b"newer bytes"
        );
        clean_fixture(cache);
    }

    #[test]
    fn rescue_rejects_stale_pending_revision_and_wrong_crypto_identity() {
        let (cache, mut pending, _) = fixture();
        let zip = cache.with_extension("zip");
        export_pack(&pending, &cache, &zip).unwrap();
        pending.updated_at += 1;
        assert!(restore_pack(&pending, &cache, &zip).is_err());
        pending.updated_at -= 1;
        pending.crypto_id = Some("changed".into());
        assert!(restore_pack(&pending, &cache, &zip).is_err());
        pending.closed_at = None;
        assert!(restore_pack(&pending, &cache, &zip).is_err());
        clean_fixture(cache);
    }

    #[test]
    fn corrupt_archive_does_not_create_any_missing_staged_chunk() {
        let (cache, pending, stage) = fixture();
        let zip = cache.with_extension("zip");
        export_pack(&pending, &cache, &zip).unwrap();
        fs::remove_file(stage.join("plain-0000000000000000.part")).unwrap();
        let mut archive_bytes = fs::read(&zip).unwrap();
        archive_bytes.truncate(archive_bytes.len() / 2);
        fs::write(&zip, archive_bytes).unwrap();
        assert!(restore_pack(&pending, &cache, &zip).is_err());
        assert!(!stage.join("plain-0000000000000000.part").exists());
        clean_fixture(cache);
    }

    #[cfg(unix)]
    #[test]
    fn stage_symlink_escape_is_rejected_without_reading_external_file() {
        use std::os::unix::fs::symlink;
        let (cache, pending, stage) = fixture();
        let external = cache.join("external");
        private_new_file(&external)
            .unwrap()
            .write_all(b"outside")
            .unwrap();
        fs::remove_file(stage.join("plain-0000000000000000.part")).unwrap();
        symlink(&external, stage.join("plain-0000000000000000.part")).unwrap();
        let zip = cache.with_extension("zip");
        assert!(export_pack(&pending, &cache, &zip).is_err());
        assert!(!zip.exists());
        assert_eq!(fs::read(&external).unwrap(), b"outside");
        clean_fixture(cache);
    }

    #[test]
    fn changed_metadata_during_io_retracts_backup_and_restored_chunks() {
        use std::cell::Cell;
        let (cache, pending, stage) = fixture();
        let zip = cache.with_extension("zip");
        let checks = Cell::new(0);
        let validation = || {
            checks.set(checks.get() + 1);
            if checks.get() > 1 {
                Err("simulated concurrent metadata revision".into())
            } else {
                Ok(())
            }
        };
        assert!(export_pack_checked(&pending, &cache, &zip, validation).is_err());
        assert_eq!(checks.get(), 2);
        assert!(!zip.exists(), "failed export must not publish a ZIP");
        export_pack(&pending, &cache, &zip).unwrap();
        let stage_file = stage.join("plain-0000000000000000.part");
        fs::remove_file(&stage_file).unwrap();
        let checks = Cell::new(0);
        let validation = || {
            checks.set(checks.get() + 1);
            if checks.get() > 1 {
                Err("simulated concurrent metadata revision".into())
            } else {
                Ok(())
            }
        };
        assert!(restore_pack_checked(&pending, &cache, &zip, validation).is_err());
        assert_eq!(checks.get(), 2);
        assert!(
            !stage_file.exists(),
            "failed restore must remove only chunks it created"
        );
        clean_fixture(cache);
    }

    #[test]
    fn malicious_pack_entries_and_duplicate_filenames_are_rejected() {
        let (cache, pending, _) = fixture();
        let manifest = manifest_for(
            &pending,
            vec![RescuePart {
                name: "../outside".into(),
                bytes: 0,
                sha256: "0".repeat(64),
            }],
        );
        assert!(validate_parts(&manifest, &pending).is_err());
        let duplicate = manifest_for(
            &pending,
            vec![
                RescuePart {
                    name: ".pending".into(),
                    bytes: 0,
                    sha256: "0".repeat(64),
                },
                RescuePart {
                    name: ".pending".into(),
                    bytes: 0,
                    sha256: "0".repeat(64),
                },
            ],
        );
        assert!(validate_parts(&duplicate, &pending).is_err());
        clean_fixture(cache);
    }
}
