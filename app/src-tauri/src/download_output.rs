use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DestinationStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    is_directory: bool,
    is_symlink: bool,
    #[cfg(unix)]
    file_identity: (u64, u64, i64, i64),
}

async fn destination_stamp(path: &Path) -> Result<Option<DestinationStamp>, String> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot inspect download destination: {error}")),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(DestinationStamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        is_directory: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        #[cfg(unix)]
        file_identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
    }))
}

#[cfg(target_os = "linux")]
async fn reclaim_crashed_stages(parent: &Path, prefix: &str) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(parent)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".part"))
        else {
            continue;
        };
        let Some((pid, nonce)) = suffix.split_once('-') else {
            continue;
        };
        let (Ok(pid), Ok(_nonce)) = (pid.parse::<u32>(), nonce.parse::<u64>()) else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let alive = if pid == std::process::id() {
            true
        } else {
            match tokio::fs::metadata(format!("/proc/{pid}")).await {
                Ok(_) => true,
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            }
        };
        if alive {
            return Err("This download already has an active output writer. Wait for it to stop before retrying.".to_string());
        }
        // Only this transfer's private stage is reclaimed. Its destination
        // and TDLib's confirmed-part cache are never removed.
        tokio::fs::remove_file(entry.path())
            .await
            .map_err(|e| format!("Cannot reclaim interrupted download output: {e}"))?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeHeader {
    version: u32,
    binding: String,
    original: Option<DestinationStamp>,
}

/// SQLite indexes regions on disk instead of retaining one allocation per region.
/// The separate exclusive transaction is an OS-backed lease released on a crash.
struct ResumeJournal {
    database: Mutex<sqlite::Connection>,
    lease: Mutex<sqlite::Connection>,
    journal_path: PathBuf,
    lease_path: PathBuf,
    confirmed: AtomicU64,
}

fn journal_error(error: sqlite::Error) -> String {
    format!("Cannot read or save download resume journal: {error}")
}

fn regular_private_file(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Download resume state is not a regular private file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot inspect download resume state: {error}")),
    }
}

#[cfg(windows)]
fn prepare_sparse_staging(file: &std::fs::File) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, HANDLE};
    use windows::Win32::System::{Ioctl::FSCTL_SET_SPARSE, IO::DeviceIoControl};

    let mut returned = 0_u32;
    // NTFS otherwise allocates every byte between EOF and an out-of-order region.
    // The borrowed synchronous handle remains open for this bufferless call.
    let result = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    match result {
        Ok(()) => Ok(()),
        // Filesystems without sparse support still permit ordinary downloads.
        Err(error)
            if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_FUNCTION.0)
                || error.code() == windows::core::HRESULT::from_win32(ERROR_NOT_SUPPORTED.0) =>
        {
            Ok(())
        }
        Err(error) => Err(format!("Cannot prepare sparse download output: {error}")),
    }
}

fn region_bounds(offset: u64, len: u64) -> Result<(i64, i64), String> {
    let end = offset
        .checked_add(len)
        .filter(|end| len > 0 && *end <= i64::MAX as u64)
        .ok_or_else(|| "Invalid download checkpoint range".to_string())?;
    Ok((offset as i64, end as i64))
}

fn region_hash(file: &mut std::fs::File, offset: u64, len: u64) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])
            .map_err(|e| format!("Cannot verify download checkpoint bytes: {e}"))?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn install_resume_database(
    journal_path: &Path,
    initialize: impl FnOnce(&sqlite::Connection) -> Result<(), String>,
) -> Result<sqlite::Connection, String> {
    // Publish ownership only after the complete SQLite transaction is durable.
    // An interrupted initializer can leave a private temp, never a final journal.
    let temp = journal_path.with_extension(format!(
        "init-{}-{}.sqlite",
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("Cannot allocate download journal: {e}"))?;
        let database = sqlite::open(&temp).map_err(journal_error)?;
        database
            .execute("PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;")
            .map_err(journal_error)?;
        initialize(&database)?;
        drop(database);
        // Windows FlushFileBuffers requires a handle opened with write access.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("Cannot sync download journal: {e}"))?;
        // hard_link publishes atomically and refuses to replace foreign state.
        std::fs::hard_link(&temp, journal_path)
            .map_err(|e| format!("Cannot publish download journal: {e}"))?;
        #[cfg(unix)]
        if let Some(parent) = journal_path.parent() {
            std::fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        sqlite::open(journal_path).map_err(journal_error)
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

fn open_resume_journal(
    staging: &Path,
    journal_path: PathBuf,
    lease_path: PathBuf,
    binding: String,
    original: Option<DestinationStamp>,
) -> Result<ResumeJournal, String> {
    regular_private_file(&lease_path)?;
    let lease = sqlite::open(&lease_path).map_err(journal_error)?;
    lease.execute("PRAGMA busy_timeout=0; BEGIN EXCLUSIVE;")
        .map_err(|_| "This download already has an active output writer. Wait for it to stop before retrying.".to_string())?;
    let stage_exists = regular_private_file(staging)?;
    let journal_exists = regular_private_file(&journal_path)?;
    if stage_exists && !journal_exists {
        return Err("Download staging bytes have no owned resume journal; choose another save location or transfer.".to_string());
    }
    let database = if journal_exists {
        sqlite::open(&journal_path).map_err(journal_error)?
    } else {
        let header = serde_json::to_string(&ResumeHeader {
            version: 1,
            binding: binding.clone(),
            original: original.clone(),
        })
        .map_err(|e| e.to_string())?
        .replace('\'', "''");
        install_resume_database(&journal_path, |database| {
            database.execute(format!(
                "BEGIN IMMEDIATE; CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); \
                 CREATE TABLE regions(start INTEGER PRIMARY KEY,end INTEGER NOT NULL,hash TEXT NOT NULL); \
                 INSERT INTO metadata VALUES('header','{header}'); COMMIT;"
            )).map_err(journal_error)
        })?
    };
    database
        .execute("PRAGMA cache_size=-2048; PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;")
        .map_err(journal_error)?;
    if journal_exists {
        let mut check = database
            .prepare("PRAGMA quick_check")
            .map_err(journal_error)?;
        if check.next().map_err(journal_error)? != sqlite::State::Row
            || check.read::<String, _>(0).map_err(journal_error)? != "ok"
        {
            return Err("Download resume journal is corrupt".to_string());
        }
        drop(check);
        let mut statement = database
            .prepare("SELECT value FROM metadata WHERE key='header'")
            .map_err(journal_error)?;
        if statement.next().map_err(journal_error)? != sqlite::State::Row {
            return Err("Download resume journal has no ownership header".to_string());
        }
        let header: ResumeHeader = serde_json::from_str(
            &statement.read::<String, _>(0).map_err(journal_error)?,
        )
        .map_err(|_| "Download resume journal has an invalid ownership header".to_string())?;
        if header.version != 1 || header.binding != binding {
            return Err(
                "Download resume journal belongs to another remote source or transfer".to_string(),
            );
        }
        if header.original != original {
            return Err("Destination changed while the download was paused. Choose another save location before retrying.".to_string());
        }
    }
    if !stage_exists {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(staging)
            .map_err(|e| format!("Cannot create resumable download output: {e}"))?;
        file.sync_all().map_err(|e| e.to_string())?;
        database
            .execute("DELETE FROM regions")
            .map_err(journal_error)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    let mut file = options.open(staging).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    prepare_sparse_staging(&file)?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut previous_start = -1_i64;
    let mut previous_end = 0_i64;
    let mut confirmed = 0_u64;
    loop {
        // Keyset scanning allows invalid-byte records to be deleted without
        // retaining them in RAM or mutating the table under an open cursor.
        let mut statement = database
            .prepare(format!(
            "SELECT start,end,hash FROM regions WHERE start>{previous_start} ORDER BY start LIMIT 1"
        ))
            .map_err(journal_error)?;
        if statement.next().map_err(journal_error)? != sqlite::State::Row {
            break;
        }
        let start = statement.read::<i64, _>(0).map_err(journal_error)?;
        let end = statement.read::<i64, _>(1).map_err(journal_error)?;
        let hash = statement.read::<String, _>(2).map_err(journal_error)?;
        drop(statement);
        if start < 0
            || end <= start
            || start < previous_end
            || hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(
                "Download resume journal contains invalid or overlapping regions".to_string(),
            );
        }
        previous_start = start;
        previous_end = end;
        let len = (end - start) as u64;
        if end as u64 <= size && region_hash(&mut file, start as u64, len)? == hash {
            confirmed = confirmed
                .checked_add(len)
                .ok_or_else(|| "Invalid download checkpoint byte total".to_string())?;
        } else {
            database
                .execute(format!("DELETE FROM regions WHERE start={start}"))
                .map_err(journal_error)?;
        }
    }
    // Negative records must not be hidden by the keyset scan's initial bound.
    let mut negatives = database
        .prepare("SELECT start FROM regions WHERE start<0 LIMIT 1")
        .map_err(journal_error)?;
    if negatives.next().map_err(journal_error)? == sqlite::State::Row {
        return Err("Download resume journal contains an invalid region offset".to_string());
    }
    drop(negatives);
    #[cfg(unix)]
    if let Some(parent) = staging.parent() {
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("Cannot sync download resume directory: {e}"))?;
    }
    Ok(ResumeJournal {
        database: Mutex::new(database),
        lease: Mutex::new(lease),
        journal_path,
        lease_path,
        confirmed: AtomicU64::new(confirmed),
    })
}

/// Own a unique sibling output. Transports write here; only verified complete
/// data is published, so their failures cannot truncate/delete the destination.
pub struct DownloadOutput {
    destination: PathBuf,
    original: Option<DestinationStamp>,
    staging: Option<PathBuf>,
    path_text: String,
    resume: Option<Arc<ResumeJournal>>,
    #[cfg(test)]
    prefix: String,
}

impl DownloadOutput {
    pub async fn create(destination: &str, transfer_id: &str) -> Result<Self, String> {
        let destination = PathBuf::from(destination);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Cannot create download destination directory: {e}"))?;
        let original = destination_stamp(&destination).await?;
        let identity = serde_json::to_vec(&(destination.to_string_lossy(), transfer_id))
            .map_err(|e| e.to_string())?;
        let prefix = format!(".terarelay-download-{:x}-", Sha256::digest(&identity));
        #[cfg(target_os = "linux")]
        reclaim_crashed_stages(parent, &prefix).await?;
        for _ in 0..8 {
            let staging = parent.join(format!(
                "{prefix}{}-{}.part",
                std::process::id(),
                rand::random::<u64>()
            ));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .await
            {
                Ok(file) => {
                    #[cfg(windows)]
                    let file = file.into_std().await;
                    #[cfg(windows)]
                    if let Err(error) = prepare_sparse_staging(&file) {
                        drop(file);
                        let _ = tokio::fs::remove_file(&staging).await;
                        return Err(error);
                    }
                    drop(file);
                    let path_text = staging.to_string_lossy().into_owned();
                    return Ok(Self {
                        destination,
                        original,
                        staging: Some(staging),
                        path_text,
                        resume: None,
                        #[cfg(test)]
                        prefix,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("Cannot create staged download output: {error}")),
            }
        }
        Err("Cannot allocate a unique download output".to_string())
    }

    /// Keep completed output regions through pause/relaunch. The source identity
    /// must contain stable document IDs, sizes, DC and expected hashes, never a
    /// rotating Telegram file_reference. Regions must not be rewritten after confirmation.
    pub async fn create_resumable(
        destination: &str,
        transfer_id: &str,
        remote_identity: String,
    ) -> Result<Self, String> {
        let requested = PathBuf::from(destination);
        let parent = requested
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
        let parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|e| e.to_string())?;
        let name = requested
            .file_name()
            .ok_or_else(|| "Invalid download destination filename".to_string())?;
        let destination = parent.join(name);
        let original = destination_stamp(&destination).await?;
        let identity =
            serde_json::to_vec(&(destination.to_string_lossy(), transfer_id, remote_identity))
                .map_err(|e| e.to_string())?;
        let binding = format!("{:x}", Sha256::digest(&identity));
        let prefix = format!(".terarelay-resume-{binding}");
        let staging = parent.join(format!("{prefix}.part"));
        let journal_path = parent.join(format!("{prefix}.sqlite"));
        let lease_path = parent.join(format!("{prefix}.lock.sqlite"));
        let stage_copy = staging.clone();
        let original_copy = original.clone();
        let resume = tokio::task::spawn_blocking(move || {
            open_resume_journal(
                &stage_copy,
                journal_path,
                lease_path,
                binding,
                original_copy,
            )
        })
        .await
        .map_err(|e| format!("Download resume validation failed: {e}"))??;
        Ok(Self {
            destination,
            original,
            path_text: staging.to_string_lossy().into_owned(),
            staging: Some(staging),
            resume: Some(Arc::new(resume)),
            #[cfg(test)]
            prefix,
        })
    }

    /// Each blocking write retains the output lease even if its awaiter is aborted.
    /// Completion means all bytes reached the file before checkpoint hashing.
    pub async fn write_region(&self, offset: u64, bytes: &[u8]) -> Result<(), String> {
        region_bounds(offset, bytes.len() as u64)?;
        let lease = self.resume.clone();
        let path = self.path_text.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::{Seek, SeekFrom, Write};
            let _lease = lease;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
            file.write_all(&bytes).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("Download disk write task failed: {e}"))?
    }

    /// Only exact previously confirmed regions may be skipped.
    pub fn is_region_confirmed(&self, offset: u64, len: u64) -> bool {
        let Ok((start, end)) = region_bounds(offset, len) else {
            return false;
        };
        let Some(resume) = &self.resume else {
            return false;
        };
        let Ok(database) = resume.database.lock() else {
            return false;
        };
        let Ok(mut statement) =
            database.prepare(format!("SELECT end FROM regions WHERE start={start}"))
        else {
            return false;
        };
        statement.next().ok() == Some(sqlite::State::Row)
            && statement.read::<i64, _>(0).ok() == Some(end)
    }

    pub fn confirmed_bytes(&self) -> u64 {
        self.resume
            .as_ref()
            .map(|resume| resume.confirmed.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Flush durable bytes, hash them with bounded memory, then commit their
    /// checkpoint. A crash before the database commit only causes a redownload.
    pub async fn confirm_region(&self, offset: u64, len: u64) -> Result<(), String> {
        if self.resume.is_none() {
            return Ok(());
        }
        let (start, end) = region_bounds(offset, len)?;
        let resume = self
            .resume
            .as_ref()
            .ok_or_else(|| "Download output has no resume journal".to_string())?
            .clone();
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| "Download output was already published".to_string())?
            .clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let database = resume
                .database
                .lock()
                .map_err(|_| "Download resume journal lock failed".to_string())?;
            regular_private_file(&staging)?;
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staging)
                .map_err(|e| format!("Cannot open download checkpoint output: {e}"))?;
            if file.metadata().map_err(|e| e.to_string())?.len() < end as u64 {
                return Err("Download checkpoint extends beyond completed output bytes".to_string());
            }
            file.sync_data()
                .map_err(|e| format!("Cannot sync download checkpoint bytes: {e}"))?;
            let hash = region_hash(&mut file, offset, len)?;
            let mut existing = database
                .prepare(format!(
                "SELECT start,end,hash FROM regions WHERE start<{end} ORDER BY start DESC LIMIT 1"
            ))
                .map_err(journal_error)?;
            if existing.next().map_err(journal_error)? == sqlite::State::Row {
                let existing_start = existing.read::<i64, _>(0).map_err(journal_error)?;
                let existing_end = existing.read::<i64, _>(1).map_err(journal_error)?;
                if existing_end > start {
                    if existing_start == start
                        && existing_end == end
                        && existing.read::<String, _>(2).map_err(journal_error)? == hash
                    {
                        return Ok(());
                    }
                    return Err(
                        "Download checkpoint overlaps an existing confirmed region".to_string()
                    );
                }
            }
            drop(existing);
            database
                .execute(format!(
                    "INSERT INTO regions(start,end,hash) VALUES({start},{end},'{hash}')"
                ))
                .map_err(journal_error)?;
            resume.confirmed.fetch_add(len, Ordering::Release);
            Ok(())
        })
        .await
        .map_err(|e| format!("Download checkpoint task failed: {e}"))?
    }

    pub fn path(&self) -> &str {
        &self.path_text
    }

    pub async fn publish(&mut self) -> Result<(), String> {
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| "Download output was already published".to_string())?;
        if destination_stamp(&self.destination).await? != self.original {
            return Err("Destination changed during download. Choose another save location before retrying.".to_string());
        }
        // Sync requires write access on Windows; opening must not truncate bytes.
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(staging)
            .await
            .map_err(|e| format!("Cannot open completed download output: {e}"))?;
        file.sync_all()
            .await
            .map_err(|e| format!("Cannot sync completed download output: {e}"))?;
        drop(file);
        tokio::fs::rename(staging, &self.destination)
            .await
            .map_err(|e| format!("Cannot publish completed download: {e}"))?;
        self.staging = None;
        if let Some(resume) = self.resume.take() {
            // Close the journal before unlinking it (also needed on Windows),
            // while retaining the exclusive lease until cleanup completes.
            if let Ok(resume) = Arc::try_unwrap(resume) {
                let ResumeJournal {
                    database,
                    lease,
                    journal_path,
                    lease_path,
                    ..
                } = resume;
                drop(database);
                if let Err(error) = std::fs::remove_file(&journal_path) {
                    log::warn!(
                        "Completed download was saved but resume journal cleanup failed: {error}"
                    );
                }
                drop(lease);
                // The exclusive SQLite lease is no longer needed after the
                // completed destination is safely published. Its persistent
                // .lock.sqlite file was previously left in the Downloads folder.
                // Never remove this lease on pause/failure: resumability relies
                // on preserving the journal and its matching stage.
                if let Err(error) = std::fs::remove_file(&lease_path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn!(
                            "Completed download was saved but resume lock cleanup failed: {error}"
                        );
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            let parent = self
                .destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if let Err(error) =
                std::fs::File::open(parent).and_then(|directory| directory.sync_all())
            {
                log::warn!("Completed download was saved but directory sync failed: {error}");
            }
        }
        Ok(())
    }
}

impl Drop for DownloadOutput {
    fn drop(&mut self) {
        if self.resume.is_some() {
            return;
        }
        if let Some(staging) = &self.staging {
            let _ = std::fs::remove_file(staging);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resumable_regions_survive_drop_and_reopen_but_unconfirmed_bytes_do_not_count() {
        let dir = scratch();
        let destination = dir.join("resume.bin");
        std::fs::write(&destination, b"old complete").unwrap();
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "document:7:size:12:dc:2".into(),
        )
        .await
        .unwrap();
        let stage = output.path().to_owned();
        let journal = output.resume.as_ref().unwrap().journal_path.clone();
        let lock = output.resume.as_ref().unwrap().lease_path.clone();
        std::fs::write(&stage, b"goodparttail").unwrap();
        assert!(journal.exists());
        assert!(lock.exists());
        output.confirm_region(0, 8).await.unwrap();
        assert_eq!(output.confirmed_bytes(), 8);
        drop(output);
        assert!(Path::new(&stage).exists());
        assert!(
            journal.exists(),
            "Interrupted downloads must remain resumable"
        );
        assert!(lock.exists(), "Interrupted downloads keep their lease path");
        let mut output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "document:7:size:12:dc:2".into(),
        )
        .await
        .unwrap();
        assert_eq!(output.path(), stage);
        assert!(output.is_region_confirmed(0, 8));
        assert!(!output.is_region_confirmed(8, 4));
        assert_eq!(output.confirmed_bytes(), 8);
        output.confirm_region(8, 4).await.unwrap();
        output.publish().await.unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"goodparttail");
        assert!(!Path::new(&stage).exists());
        assert!(
            !journal.exists(),
            "Completed downloads must remove the journal"
        );
        assert!(
            !lock.exists(),
            "Completed downloads must remove the SQLite lock"
        );
        drop(output);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resumable_rechecks_saved_bytes_and_keeps_other_source_separate() {
        let dir = scratch();
        let destination = dir.join("resume.bin");
        std::fs::write(&destination, b"previous file").unwrap();
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into(),
        )
        .await
        .unwrap();
        let stage = output.path().to_owned();
        std::fs::write(&stage, b"goodparttail").unwrap();
        output.confirm_region(0, 8).await.unwrap();
        output.confirm_region(8, 4).await.unwrap();
        assert!(DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into()
        )
        .await
        .is_err());
        drop(output);
        std::fs::write(&stage, b"bad!parttail").unwrap();
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into(),
        )
        .await
        .unwrap();
        assert!(!output.is_region_confirmed(0, 8));
        assert!(output.is_region_confirmed(8, 4));
        assert_eq!(output.confirmed_bytes(), 4);
        drop(output);
        let other = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-two".into(),
        )
        .await
        .unwrap();
        assert_ne!(other.path(), stage);
        assert_eq!(other.confirmed_bytes(), 0);
        drop(other);
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous file");
        assert!(Path::new(&stage).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resumable_rejects_overlap_overflow_and_destination_changes_across_reopen() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = scratch();
        let destination = dir.join("resume.bin");
        std::fs::write(&destination, b"previous file").unwrap();
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into(),
        )
        .await
        .unwrap();
        std::fs::write(output.path(), b"abcdefgh").unwrap();
        output.confirm_region(0, 4).await.unwrap();
        assert!(output.confirm_region(2, 4).await.is_err());
        assert!(output.confirm_region(u64::MAX, 1).await.is_err());
        assert!(output.confirm_region(8, 1).await.is_err());
        assert!(output.confirm_region(0, 0).await.is_err());
        // A sparse 10-TB logical output demonstrates u64 offsets without allocating it.
        let offset = 10_000_000_000_000_u64;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(output.path())
            .unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"tiny").unwrap();
        drop(file);
        output.confirm_region(offset, 4).await.unwrap();
        assert!(output.is_region_confirmed(offset, 4));
        assert_eq!(output.confirmed_bytes(), 8);
        drop(output);
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into(),
        )
        .await
        .unwrap();
        assert!(output.is_region_confirmed(offset, 4));
        drop(output);
        std::fs::write(&destination, b"changed while paused").unwrap();
        assert!(DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "resume",
            "source-one".into()
        )
        .await
        .is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"changed while paused"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resumable_rejects_foreign_corrupt_and_overlapping_journals_without_removing_bytes() {
        for corruption in ["foreign", "corrupt", "overlap", "negative"] {
            let dir = scratch();
            let destination = dir.join("resume.bin");
            std::fs::write(&destination, b"previous file").unwrap();
            let output = DownloadOutput::create_resumable(
                destination.to_str().unwrap(),
                "resume",
                "source".into(),
            )
            .await
            .unwrap();
            let stage = PathBuf::from(output.path());
            let journal = output.resume.as_ref().unwrap().journal_path.clone();
            std::fs::write(&stage, b"abcdefgh").unwrap();
            output.confirm_region(0, 4).await.unwrap();
            drop(output);
            if corruption == "corrupt" {
                std::fs::write(&journal, b"invalid SQLite bytes").unwrap();
            } else {
                let database = sqlite::open(&journal).unwrap();
                match corruption {
                    "foreign" => {
                        let mut statement = database
                            .prepare("SELECT value FROM metadata WHERE key='header'")
                            .unwrap();
                        assert_eq!(statement.next().unwrap(), sqlite::State::Row);
                        let mut header: ResumeHeader =
                            serde_json::from_str(&statement.read::<String, _>(0).unwrap()).unwrap();
                        drop(statement);
                        header.binding = "another-source".into();
                        let header = serde_json::to_string(&header).unwrap().replace('\'', "''");
                        database
                            .execute(format!(
                                "UPDATE metadata SET value='{header}' WHERE key='header'"
                            ))
                            .unwrap();
                    }
                    "overlap" => database
                        .execute(format!(
                            "INSERT INTO regions VALUES(2,6,'{}')",
                            "0".repeat(64)
                        ))
                        .unwrap(),
                    "negative" => database
                        .execute(format!(
                            "INSERT INTO regions VALUES(-1,1,'{}')",
                            "0".repeat(64)
                        ))
                        .unwrap(),
                    _ => unreachable!(),
                }
            }
            assert!(
                DownloadOutput::create_resumable(
                    destination.to_str().unwrap(),
                    "resume",
                    "source".into()
                )
                .await
                .is_err(),
                "{corruption}"
            );
            assert_eq!(std::fs::read(&stage).unwrap(), b"abcdefgh");
            assert_eq!(std::fs::read(&destination).unwrap(), b"previous file");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn resumable_process_exit_releases_lease_and_preserves_confirmed_regions() {
        const CHILD_DESTINATION: &str = "TERARELAY_OUTPUT_EXIT_TEST_DESTINATION";
        if let Ok(destination) = std::env::var(CHILD_DESTINATION) {
            let output = DownloadOutput::create_resumable(&destination, "exit", "source".into())
                .await
                .unwrap();
            std::fs::write(output.path(), b"goodtail").unwrap();
            output.confirm_region(0, 4).await.unwrap();
            // Exit skips Rust destructors, reproducing the interrupted owner's
            // durable-file and OS-lock behavior without any account or network.
            std::process::exit(0);
        }
        let dir = scratch();
        let destination = dir.join("resume.bin");
        std::fs::write(&destination, b"previous file").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("resumable_process_exit_releases_lease_and_preserves_confirmed_regions")
            .arg("--nocapture")
            .env(CHILD_DESTINATION, &destination)
            .status()
            .unwrap();
        assert!(status.success());
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "exit",
            "source".into(),
        )
        .await
        .unwrap();
        assert!(output.is_region_confirmed(0, 4));
        assert!(!output.is_region_confirmed(4, 4));
        assert_eq!(output.confirmed_bytes(), 4);
        assert_eq!(std::fs::read(output.path()).unwrap(), b"goodtail");
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous file");
        drop(output);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn nonresumable_checkpoint_is_a_noop_and_retains_temporary_cleanup() {
        let dir = scratch();
        let destination = dir.join("normal.bin");
        let output = DownloadOutput::create(destination.to_str().unwrap(), "normal")
            .await
            .unwrap();
        let stage = output.path().to_owned();
        std::fs::write(&stage, b"good").unwrap();
        output.confirm_region(0, 4).await.unwrap();
        assert_eq!(output.confirmed_bytes(), 0);
        assert!(!output.is_region_confirmed(0, 4));
        drop(output);
        assert!(!Path::new(&stage).exists());
        assert!(!destination.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resumable_initialization_exit_does_not_publish_an_ownerless_journal() {
        const CHILD_JOURNAL: &str = "TERARELAY_OUTPUT_INIT_EXIT_TEST_JOURNAL";
        if let Ok(journal) = std::env::var(CHILD_JOURNAL) {
            let _ = install_resume_database(Path::new(&journal), |database| {
                database
                    .execute("BEGIN IMMEDIATE; CREATE TABLE interrupted(value TEXT);")
                    .map_err(journal_error)?;
                // Simulate interruption before the ownership transaction commits.
                std::process::exit(0);
            });
            panic!("Initializer must reach the exit point");
        }
        let dir = scratch();
        let destination = dir.join("resume.bin");
        std::fs::write(&destination, b"previous file").unwrap();
        let identity =
            serde_json::to_vec(&(destination.to_string_lossy(), "init-exit", "source")).unwrap();
        let journal = dir.join(format!(
            ".terarelay-resume-{:x}.sqlite",
            Sha256::digest(identity)
        ));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("resumable_initialization_exit_does_not_publish_an_ownerless_journal")
            .arg("--nocapture")
            .env(CHILD_JOURNAL, &journal)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            !journal.exists(),
            "Uncommitted initialization must not expose the final journal"
        );
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "init-exit",
            "source".into(),
        )
        .await
        .unwrap();
        assert_eq!(output.confirmed_bytes(), 0);
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous file");
        drop(output);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sparse_ten_terabyte_write_is_complete_before_checkpoint() {
        let dir = scratch();
        let destination = dir.join("large.bin");
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "large",
            "source".into(),
        )
        .await
        .unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
            assert_ne!(
                std::fs::metadata(output.path()).unwrap().file_attributes()
                    & FILE_ATTRIBUTE_SPARSE_FILE.0,
                0,
                "Out-of-order Windows output must not allocate the entire gap"
            );
        }
        let offset = 10_000_000_000_000_u64 - 4;
        output.write_region(offset, b"done").await.unwrap();
        output.confirm_region(offset, 4).await.unwrap();
        assert_eq!(output.confirmed_bytes(), 4);
        drop(output);
        let output = DownloadOutput::create_resumable(
            destination.to_str().unwrap(),
            "large",
            "source".into(),
        )
        .await
        .unwrap();
        assert!(output.is_region_confirmed(offset, 4));
        assert_eq!(
            std::fs::metadata(output.path()).unwrap().len(),
            10_000_000_000_000
        );
        drop(output);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn aborted_write_future_holds_lease_until_blocking_write_finishes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = scratch();
            let destination = dir.join("cancel.bin");
            let output = Arc::new(
                DownloadOutput::create_resumable(
                    destination.to_str().unwrap(),
                    "cancel-write",
                    "source".into(),
                )
                .await
                .unwrap(),
            );
            let lease_path = dir.join(format!("{}.lock.sqlite", output.prefix));
            let (release, wait) = std::sync::mpsc::channel();
            let (started, ready) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            ready
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            let writer = output.clone();
            let writing = tokio::spawn(async move { writer.write_region(0, b"safe").await });
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            writing.abort();
            assert!(writing.await.unwrap_err().is_cancelled());
            drop(output);
            let probe = sqlite::open(&lease_path).unwrap();
            assert!(
                probe
                    .execute("PRAGMA busy_timeout=0; BEGIN EXCLUSIVE;")
                    .is_err(),
                "A queued disk write released its output lease early"
            );
            drop(probe);
            release.send(()).unwrap();
            blocker.await.unwrap();
            let output = DownloadOutput::create_resumable(
                destination.to_str().unwrap(),
                "cancel-write",
                "source".into(),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(output.path()).unwrap(), b"safe");
            assert_eq!(output.confirmed_bytes(), 0);
            drop(output);
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    fn scratch() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "terarelay-output-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn failures_and_delayed_cleanup_preserve_destination_and_newer_download() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        std::fs::write(&destination, b"previous complete file").unwrap();
        let first = DownloadOutput::create(destination.to_str().unwrap(), "first")
            .await
            .unwrap();
        let first_path = first.path().to_string();
        std::fs::write(&first_path, b"truncated transfer").unwrap();
        let mut second = DownloadOutput::create(destination.to_str().unwrap(), "second")
            .await
            .unwrap();
        std::fs::write(second.path(), b"verified new file").unwrap();
        second.publish().await.unwrap();
        drop(first);
        assert!(!std::path::Path::new(&first_path).exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified new file");
        let failed = DownloadOutput::create(destination.to_str().unwrap(), "failed")
            .await
            .unwrap();
        drop(failed);
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified new file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn publication_rejects_destination_changes_and_rename_errors() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        std::fs::write(&destination, b"old").unwrap();
        let mut stage = DownloadOutput::create(destination.to_str().unwrap(), "changed")
            .await
            .unwrap();
        std::fs::write(stage.path(), b"new download").unwrap();
        std::fs::write(&destination, b"someone changed this file").unwrap();
        assert!(stage
            .publish()
            .await
            .unwrap_err()
            .contains("Destination changed"));
        drop(stage);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"someone changed this file"
        );
        let directory = dir.join("existing-directory");
        std::fs::create_dir(&directory).unwrap();
        let mut stage = DownloadOutput::create(directory.to_str().unwrap(), "rename")
            .await
            .unwrap();
        std::fs::write(stage.path(), b"verified").unwrap();
        assert!(stage.publish().await.is_err());
        drop(stage);
        assert!(directory.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reopening_reclaims_only_owned_dead_process_stages() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        let first = DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
            .await
            .unwrap();
        assert!(
            DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
                .await
                .is_err()
        );
        let abandoned = format!("{}4294967295-123.part", first.prefix);
        let abandoned = dir.join(abandoned);
        std::fs::rename(first.path(), &abandoned).unwrap();
        // Simulate a killed process: its destructor cannot remove the moved stage.
        drop(first);
        std::fs::write(dir.join("unrelated.part"), b"keep").unwrap();
        let resumed = DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
            .await
            .unwrap();
        assert!(!abandoned.exists());
        assert!(dir.join("unrelated.part").exists());
        drop(resumed);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
