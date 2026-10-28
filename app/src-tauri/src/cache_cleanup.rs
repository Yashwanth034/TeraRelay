//! Cache policy: never treat login state or pending transfers as disposable.
use std::path::Path;

// Keep two full encrypted 16 MiB windows, including their framing overhead.
pub(crate) const DRIVE_CACHE_BYTES: u64 = 32 * 1024 * 1024 + 4096;

#[cfg(test)]
thread_local! {
    // Each test runner thread observes only its own prune invocations.
    // A process-wide counter makes parallel unrelated cache tests racy.
    static PRUNE_SORT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Missing/malformed queues are not evidence that resumable data is unused.
pub(crate) fn has_unfinished_transfers(conn: &sqlite::Connection) -> Result<bool, String> {
    let mut table = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='transfer_queues'")
        .map_err(|e| e.to_string())?;
    if table.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(true);
    }
    for kind in ["upload", "download"] {
        let mut statement = conn
            .prepare("SELECT items FROM transfer_queues WHERE kind=?")
            .map_err(|e| e.to_string())?;
        statement.bind((1, kind)).map_err(|e| e.to_string())?;
        if statement.next().map_err(|e| e.to_string())? != sqlite::State::Row {
            return Ok(true);
        }
        // SQLite's String decoder can coerce non-TEXT storage and truncate at
        // an embedded NUL. That could make an unfinished queue look like [].
        // Cleanup authorization requires the exact persisted UTF-8 document.
        match statement
            .read::<sqlite::Value, _>(0)
            .map_err(|e| e.to_string())?
        {
            sqlite::Value::String(_) => {}
            _ => return Err("Invalid saved transfer queue storage type".into()),
        }
        let bytes = statement.read::<Vec<u8>, _>(0).map_err(|e| e.to_string())?;
        if bytes.contains(&0) {
            return Err("Invalid saved transfer queue: embedded NUL".into());
        }
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let items = value.as_array().ok_or("Invalid saved transfer queue")?;
        if items
            .iter()
            .any(|item| item.get("status").and_then(|v| v.as_str()) != Some("success"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn regular_directory(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(format!(
            "Refusing cleanup of non-directory cache: {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Inspect only an owned regular tree before TDLib is allowed to sweep it.
pub(crate) fn cache_contains_files(path: &Path) -> Result<bool, String> {
    if !regular_directory(path)? {
        return Ok(false);
    }
    let mut found = false;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().is_symlink() {
            return Err("Refusing TDLib cache cleanup through a symbolic link".to_string());
        }
        if entry.file_type().is_file() && entry.metadata().map_err(|e| e.to_string())?.len() > 0 {
            found = true;
        }
    }
    Ok(found)
}

/// These file directories belonged to retired engines. Current Boost uses
/// Grammers and writes directly to the chosen export; current TDLib uses v4.
/// Session directories, keys, databases and the current TDLib cache are excluded.
pub(crate) fn prune_legacy_file_caches(app_data: &Path) -> Result<u64, String> {
    const RETIRED: &[&str] = &[
        "tdlib-turbo-files-v1-0",
        "tdlib-turbo-files-v1-1",
        "tdlib-turbo-files-v1-2",
        "tdlib-turbo-files-v1-3",
        "tdlib-fast-files",
        "tdlib-fast-files-v3",
        "turbo-download-temp",
    ];
    if !regular_directory(app_data)? {
        return Ok(0);
    }
    let mut paths = Vec::new();
    // Preflight all roots before deleting anything. Do not follow child links.
    for name in RETIRED {
        let path = app_data.join(name);
        if regular_directory(&path)? {
            paths.push(path);
        }
    }
    let mut removed = 0u64;
    for path in paths {
        let size = walkdir::WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .fold(0u64, u64::saturating_add);
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        removed = removed.saturating_add(size);
    }
    Ok(removed)
}

/// A crashed cache publisher leaves a uniquely named `.tmp-{128-bit nonce}`
/// exactly one cache block, not user data. Reclaim only private, old,
/// unlinked-to-elsewhere regular files after taking a nonblocking exclusive
/// lock (active publishers retain the lock until the atomic rename).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reclaim_stale_cache_temp(entry: &walkdir::DirEntry) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};

    const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
    let Some(filename) = entry.file_name().to_str() else {
        return Ok(());
    };
    let Some((stem, nonce)) = filename.split_once(".tmp-") else {
        return Ok(());
    };
    let block_stem = stem
        .strip_prefix("v2-cipher-")
        .or_else(|| stem.strip_prefix("cipher-"))
        .unwrap_or(stem);
    if block_stem.len() != 16
        || !block_stem.bytes().all(|b| b.is_ascii_hexdigit())
        || nonce.len() != 32
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(());
    }

    let Some(parent) = entry.path().parent() else {
        return Ok(());
    };
    let dir = crate::cache_directory::open_directory(parent, false).map_err(|e| e.to_string())?;
    let file = match crate::cache_directory::open_child(
        &dir,
        entry.file_name(),
        libc::O_RDONLY | libc::O_NONBLOCK,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    // Private permissions and a sole directory entry are required: never
    // reclaim a user-created or hard-linked file that resembles cache data.
    if !meta.is_file() || meta.nlink() != 1 || meta.mode() & 0o777 != 0o600 {
        return Ok(());
    }
    let Ok(age) = SystemTime::now().duration_since(meta.modified().map_err(|e| e.to_string())?)
    else {
        return Ok(());
    };
    if age < ORPHAN_GRACE || file.try_lock().is_err() {
        return Ok(());
    }
    // Check the name still refers to this locked inode before unlinking.
    let check = match crate::cache_directory::open_child(
        &dir,
        entry.file_name(),
        libc::O_RDONLY | libc::O_NONBLOCK,
    ) {
        Ok(check) => check,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let actual = check.metadata().map_err(|e| e.to_string())?;
    if (actual.dev(), actual.ino()) != (meta.dev(), meta.ino()) {
        return Ok(());
    }
    crate::cache_directory::remove_child(&dir, entry.file_name()).map_err(|e| e.to_string())
}

// Cache blocks have deterministic names generated by Drive's native read
// paths. An arbitrary .bin file is not evidence that it is disposable cache.
fn is_owned_drive_cache_block(name: &std::ffi::OsStr) -> bool {
    let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".bin")) else {
        return false;
    };
    let index = stem
        .strip_prefix("v2-cipher-")
        .or_else(|| stem.strip_prefix("cipher-"))
        .unwrap_or(stem);
    index.len() == 16 && index.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// Record the exact directory and cache-file inode before sorting eviction
// candidates. Deletion is later performed through descriptor-relative,
// no-follow operations; a replaced name never authorizes deleting a new file.
#[cfg(any(target_os = "linux", target_os = "macos"))]
type OwnedBlockIdentity = ((u64, u64), (u64, u64));

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn owned_cache_block_identity(path: &Path) -> Result<Option<OwnedBlockIdentity>, String> {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().ok_or("Invalid Drive cache file directory")?;
    let filename = path.file_name().ok_or("Invalid Drive cache filename")?;
    let directory = crate::cache_directory::open_directory(parent, false)
        .map_err(|e| format!("Unsafe Drive cache directory: {e}"))?;
    let file = match crate::cache_directory::open_child(
        &directory,
        filename,
        libc::O_RDONLY | libc::O_NONBLOCK,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let dir_metadata = directory.metadata().map_err(|e| e.to_string())?;
    Ok(Some((
        (dir_metadata.dev(), dir_metadata.ino()),
        (metadata.dev(), metadata.ino()),
    )))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_owned_cache_block(path: &Path, expected: OwnedBlockIdentity) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().ok_or("Invalid Drive cache file directory")?;
    let filename = path.file_name().ok_or("Invalid Drive cache filename")?;
    let directory = crate::cache_directory::open_directory(parent, false)
        .map_err(|e| format!("Unsafe Drive cache directory: {e}"))?;
    let parent_metadata = directory.metadata().map_err(|e| e.to_string())?;
    if (parent_metadata.dev(), parent_metadata.ino()) != expected.0 {
        return Ok(false);
    }
    let file = match crate::cache_directory::open_child(
        &directory,
        filename,
        libc::O_RDONLY | libc::O_NONBLOCK,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || (metadata.dev(), metadata.ino()) != expected.1 {
        return Ok(false);
    }
    crate::cache_directory::remove_child(&directory, filename)
        .map(|_| true)
        .map_err(|e| e.to_string())
}

/// Prune committed block-cache files to the budget, and reclaim only old,
/// uniquely-named abandoned write temps. Never touch pending or unknown files.
/// Both operations run only on the coalesced background cache worker.
pub(crate) fn prune_drive_blocks(blocks: &Path, budget: u64) -> Result<u64, String> {
    if !regular_directory(blocks)? {
        return Ok(0);
    }
    // Unlike symlink_metadata(blocks), this verifies every ancestor, too.
    // Never even scan an externally redirected root for disposable files.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _owned_root = crate::cache_directory::open_directory(blocks, false)
        .map_err(|e| format!("Unsafe Drive cache block root: {e}"))?;
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(blocks)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry.map_err(|e| e.to_string())?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if entry.file_type().is_file() {
            reclaim_stale_cache_temp(&entry)?;
        }
        if !entry.file_type().is_file() || !is_owned_drive_cache_block(entry.file_name()) {
            continue;
        }
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            let Some(identity) = owned_cache_block_identity(entry.path())? else {
                continue;
            };
            if identity.1 != (meta.dev(), meta.ino()) {
                continue; // Swapped between the directory walk and openat.
            }
            identity
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let identity = ();
        total = total.saturating_add(meta.len());
        files.push((
            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            meta.len(),
            entry.into_path(),
            identity,
        ));
    }
    // Walking the cache is still required to know its size, but arranging
    // eviction candidates is wasteful when everything already fits. Most
    // cache insertions should retain existing blocks without any eviction.
    if total <= budget {
        return Ok(0);
    }
    #[cfg(test)]
    PRUNE_SORT_CALLS.with(|counter| counter.set(counter.get() + 1));
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    let mut removed = 0u64;
    for (_, size, path, identity) in files {
        if total <= budget {
            break;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let result = remove_owned_cache_block(&path, identity)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let result = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        total = total.saturating_sub(size);
        if result {
            removed = removed.saturating_add(size);
        }
    }
    Ok(removed)
}

/// Cache-block insertions must not run an O(cache entries) recursive directory
/// walk inside a serial Linux FUSE read callback. One owned worker performs
/// eviction asynchronously, with at most one pending rescan for concurrent
/// cache misses. Only disposable committed `.bin` blocks are considered.
pub(crate) struct DriveCachePruner {
    wakeup: std::sync::mpsc::SyncSender<()>,
}

impl DriveCachePruner {
    pub(crate) fn new(blocks: std::path::PathBuf, budget: u64) -> Result<Self, String> {
        Self::new_with_job(move || {
            if let Err(error) = prune_drive_blocks(&blocks, budget) {
                log::warn!("Could not trim Drive read cache: {error}");
            }
        })
    }

    fn new_with_job(mut job: impl FnMut() + Send + 'static) -> Result<Self, String> {
        let (wakeup, receiver) = std::sync::mpsc::sync_channel::<()>(1);
        std::thread::Builder::new()
            .name("terarelay-drive-cache-pruner".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    // Merge cache misses queued before each walk. A producer
                    // never waits for an active scan or queues unbounded work.
                    while receiver.try_recv().is_ok() {}
                    job();
                }
            })
            .map_err(|error| format!("Could not start Drive cache pruner: {error}"))?;
        Ok(Self { wakeup })
    }

    pub(crate) fn schedule(&self) {
        match self.wakeup.try_send(()) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                log::warn!("Drive cache eviction worker stopped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cache_eviction_rejects_parent_symlinks_and_replaced_file_identities() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let foreign = fixture();
        let blocks = foreign.join("blocks");
        fs::create_dir(&blocks).unwrap();
        let name = "0000000000000001.bin";
        fs::write(blocks.join(name), b"external block contents").unwrap();
        symlink(&foreign, root.join("drive")).unwrap();
        assert!(prune_drive_blocks(&root.join("drive").join("blocks"), 0).is_err());
        assert_eq!(
            fs::read(blocks.join(name)).unwrap(),
            b"external block contents"
        );
        fs::remove_file(root.join("drive")).unwrap();

        let owned = root.join("blocks");
        fs::create_dir(&owned).unwrap();
        let candidate = owned.join(name);
        fs::write(&candidate, b"original cached block").unwrap();
        let identity = owned_cache_block_identity(&candidate).unwrap().unwrap();
        fs::rename(&candidate, owned.join("moved-original.bin")).unwrap();
        fs::write(&candidate, b"replacement user data").unwrap();
        assert!(!remove_owned_cache_block(&candidate, identity).unwrap());
        assert_eq!(fs::read(&candidate).unwrap(), b"replacement user data");

        // The same captured inode cannot authorize cleanup through a newly
        // symlinked directory, even if the filename still looks like a block.
        let old_owned = root.join("original-blocks");
        fs::rename(&owned, &old_owned).unwrap();
        symlink(&blocks, &owned).unwrap();
        assert!(remove_owned_cache_block(&candidate, identity).is_err());
        assert_eq!(
            fs::read(blocks.join(name)).unwrap(),
            b"external block contents"
        );
        fs::remove_file(&owned).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(foreign).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphan_cleanup_preserves_active_malformed_linked_and_unrelated_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{Duration, SystemTime};
        let root = fixture();
        let blocks = root.join("file-a");
        fs::create_dir(&blocks).unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        let names = [
            "0000000000000000.tmp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "cipher-0000000000000001.tmp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "v2-cipher-0000000000000002.tmp-cccccccccccccccccccccccccccccccc",
            "0000000000000003.tmp-dddddddddddddddddddddddddddddddd",
            "notes.tmp-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "0000000000000000.tmp-short",
            "pending.stage",
            "unrelated.bin",
        ];
        for name in names {
            let file = fs::File::create(blocks.join(name)).unwrap();
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .unwrap();
            file.set_modified(old).unwrap();
        }
        let active = fs::File::open(blocks.join(names[1])).unwrap();
        active.lock().unwrap();
        fs::File::open(blocks.join(names[2]))
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        fs::hard_link(blocks.join(names[3]), blocks.join("keep-hardlink")).unwrap();
        symlink(
            blocks.join(names[4]),
            blocks.join("0000000000000004.tmp-ffffffffffffffffffffffffffffffff"),
        )
        .unwrap();
        let outside = fixture();
        let outside_file = outside.join(names[0]);
        fs::write(&outside_file, b"outside").unwrap();
        fs::File::open(&outside_file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        symlink(&outside, root.join("redirect")).unwrap();
        prune_drive_blocks(&root, u64::MAX).unwrap();
        let stale_remains = blocks.join(names[0]).exists();
        for name in &names[1..] {
            assert!(blocks.join(name).exists(), "removed protected {name}");
        }
        assert_eq!(fs::read(&outside_file).unwrap(), b"outside");
        drop(active);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
        assert!(
            !stale_remains,
            "crashed cache writer's stale temp was not reclaimed"
        );
    }

    #[test]
    fn drive_cache_eviction_is_nonblocking_and_coalesces_pending_scans() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let pruner = DriveCachePruner::new_with_job(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
        .unwrap();
        pruner.schedule();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let before = std::time::Instant::now();
        for _ in 0..1_000 {
            pruner.schedule();
        }
        assert!(
            before.elapsed() < Duration::from_secs(1),
            "A busy eviction scan must not hold the FUSE read callback"
        );
        release_tx.send(()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        release_tx.send(()).unwrap();
        drop(pruner);
        assert!(
            entered_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "Thousands of cache misses must not queue thousands of recursive scans"
        );
    }
    #[test]
    fn background_drive_pruner_enforces_budget_without_touching_non_cache_files() {
        use std::time::{Duration, Instant};
        let root = fixture();
        let committed = root.join("file").join("blocks");
        fs::create_dir_all(&committed).unwrap();
        fs::write(committed.join("000000000000000a.bin"), [1u8; 8]).unwrap();
        fs::write(committed.join("000000000000000b.bin"), [2u8; 8]).unwrap();
        fs::write(committed.join("pending.stage"), [3u8; 8]).unwrap();
        fs::write(root.join("unrelated.txt"), [4u8; 8]).unwrap();

        let pruner = DriveCachePruner::new(root.clone(), 8).unwrap();
        pruner.schedule();
        let deadline = Instant::now() + Duration::from_secs(3);
        while committed.join("000000000000000a.bin").exists()
            && committed.join("000000000000000b.bin").exists()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let retained = [
            committed.join("000000000000000a.bin"),
            committed.join("000000000000000b.bin"),
        ]
        .into_iter()
        .filter(|path| path.exists())
        .count();
        assert_eq!(
            retained, 1,
            "background eviction must enforce the cache budget"
        );
        assert_eq!(fs::read(committed.join("pending.stage")).unwrap(), [3u8; 8]);
        assert_eq!(fs::read(root.join("unrelated.txt")).unwrap(), [4u8; 8]);
        drop(pruner);
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("tr-cache-policy-{:016x}", rand::random::<u64>()));
        fs::create_dir(&p).unwrap();
        p
    }
    fn queues(upload: &str, download: &str) -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute("CREATE TABLE transfer_queues (kind TEXT PRIMARY KEY, items TEXT NOT NULL)")
            .unwrap();
        for (kind, items) in [("upload", upload), ("download", download)] {
            let mut s = conn
                .prepare("INSERT INTO transfer_queues VALUES (?,?)")
                .unwrap();
            s.bind((1, kind)).unwrap();
            s.bind((2, items)).unwrap();
            s.next().unwrap();
        }
        conn
    }
    #[test]
    fn corrupted_queue_sqlite_storage_never_authorizes_cache_cleanup() {
        let conn = queues("[]", "[]");
        assert!(!has_unfinished_transfers(&conn).unwrap());
        for invalid in [
            "'[]'||char(0)||'[ {\"status\":\"uploading\"} ]'",
            "x'5b5d'",
            "0",
            "0.5",
        ] {
            conn.execute(format!(
                "UPDATE transfer_queues SET items={invalid} WHERE kind='upload'"
            ))
            .unwrap();
            assert!(
                has_unfinished_transfers(&conn).is_err(),
                "corrupt saved transfer queue {invalid} was accepted as complete"
            );
        }
        conn.execute("UPDATE transfer_queues SET items='[]' WHERE kind='upload'")
            .unwrap();
        assert!(!has_unfinished_transfers(&conn).unwrap());
        conn.execute(
            "UPDATE transfer_queues SET items='[{\"status\":\"uploading\"}]' WHERE kind='upload'",
        )
        .unwrap();
        assert!(has_unfinished_transfers(&conn).unwrap());
    }

    #[test]
    fn unknown_or_corrupt_queue_state_cannot_authorize_cleanup() {
        assert!(has_unfinished_transfers(&sqlite::open(":memory:").unwrap()).unwrap());
        assert!(has_unfinished_transfers(&queues("[]", "invalid")).is_err());
        assert!(has_unfinished_transfers(&queues("[]", "[{}]")).unwrap());
    }
    #[test]
    fn paused_failed_and_active_transfers_keep_their_cache() {
        for status in [
            "paused",
            "error",
            "pending",
            "downloading",
            "pausing",
            "uploading",
            "cancelled",
        ] {
            let item = format!(r#"[{{"status":"{}"}}]"#, status);
            assert!(has_unfinished_transfers(&queues("[]", &item)).unwrap());
            assert!(has_unfinished_transfers(&queues(&item, "[]")).unwrap());
        }
        assert!(!has_unfinished_transfers(&queues("[]", "[]")).unwrap());
        assert!(!has_unfinished_transfers(&queues(r#"[{"status":"success"}]"#, "[]")).unwrap());
    }
    #[test]
    fn legacy_cleanup_preserves_all_authorization_and_current_transfer_data() {
        let root = fixture();
        for name in [
            "tdlib-turbo-files-v1-0",
            "tdlib-fast-session-v4",
            "tdlib-turbo-session-v1-0",
            "tdlib-fast-files-v4",
            "user-files",
        ] {
            fs::create_dir(root.join(name)).unwrap();
            fs::write(root.join(name).join("keep.bin"), b"fixture").unwrap();
        }
        fs::write(root.join("tdlib-fast-database-key-v4"), b"fixture-key").unwrap();
        fs::write(root.join("user.db"), b"fixture-database").unwrap();
        assert_eq!(prune_legacy_file_caches(&root).unwrap(), 7);
        assert!(!root.join("tdlib-turbo-files-v1-0").exists());
        for name in [
            "tdlib-fast-session-v4",
            "tdlib-turbo-session-v1-0",
            "tdlib-fast-files-v4",
            "user-files",
        ] {
            assert_eq!(
                fs::read(root.join(name).join("keep.bin")).unwrap(),
                b"fixture"
            );
        }
        assert!(root.join("tdlib-fast-database-key-v4").exists());
        assert!(root.join("user.db").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn drive_pruning_never_deletes_unknown_binary_files() {
        let root = fixture();
        let recognized = [
            "0000000000000000.bin",
            "cipher-0000000000000001.bin",
            "v2-cipher-0000000000000002.bin",
        ];
        for name in recognized {
            assert!(is_owned_drive_cache_block(std::ffi::OsStr::new(name)));
            fs::write(root.join(name), b"cached-block").unwrap();
        }
        let unknown = [
            "unrelated.bin",
            "00000001.bin",
            "cipher-0.bin",
            "v2-cipher-000000000000000g.bin",
            "0000000000000000.bin.backup",
            "metadata.json",
        ];
        for name in unknown {
            assert!(!is_owned_drive_cache_block(std::ffi::OsStr::new(name)));
            fs::write(root.join(name), b"keep").unwrap();
        }
        assert_eq!(prune_drive_blocks(&root, 0).unwrap(), 3 * 12);
        for name in recognized {
            assert!(
                !root.join(name).exists(),
                "recognized cache block was not evicted: {name}"
            );
        }
        for name in unknown {
            assert_eq!(
                fs::read(root.join(name)).unwrap(),
                b"keep",
                "unknown binary cache victim: {name}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drive_pruning_preserves_temporary_writes_and_unknown_files() {
        let root = fixture();
        let file = root.join("file-a");
        fs::create_dir(&file).unwrap();
        fs::write(file.join("0000000000000000.bin"), vec![0; 32]).unwrap();
        fs::write(file.join("0000000000000001.bin"), vec![0; 32]).unwrap();
        fs::write(file.join("active.tmp"), b"pending").unwrap();
        fs::write(file.join("metadata.json"), b"keep").unwrap();
        assert_eq!(prune_drive_blocks(&root, 32).unwrap(), 32);
        assert!(file.join("active.tmp").exists());
        assert!(file.join("metadata.json").exists());
        assert_eq!(prune_drive_blocks(&root, 0).unwrap(), 32);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_encrypted_read_windows_remain_cached_for_small_following_reads() {
        let root = fixture();
        let file = root.join("file-a");
        fs::create_dir(&file).unwrap();
        let plaintext = vec![1; crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE as usize];
        let key = [7; 32];
        for index in 0..2 {
            let bytes =
                crate::drive_crypto::encrypt_chunk(&key, "cache-fixture", index, &plaintext)
                    .unwrap();
            fs::write(file.join(format!("cipher-{index:016x}.bin")), bytes).unwrap();
        }
        assert_eq!(
            prune_drive_blocks(&root, DRIVE_CACHE_BYTES).unwrap(),
            0,
            "Ciphertext overhead causes full blocks to evict themselves"
        );
        assert_eq!(fs::read_dir(&file).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_below_budget_does_not_sort_or_evict_unchanged_entries() {
        let root = fixture();
        for i in 0..3 {
            fs::write(root.join(format!("{i:016x}.bin")), b"same").unwrap();
        }
        PRUNE_SORT_CALLS.with(|counter| counter.set(0));
        let removed = prune_drive_blocks(&root, DRIVE_CACHE_BYTES).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
        assert_eq!(
            PRUNE_SORT_CALLS.with(|counter| counter.get()),
            0,
            "no eviction needed: the full cache sort is unnecessary"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_over_budget_preserves_oldest_first_eviction() {
        let root = fixture();
        fs::write(root.join("0000000000000001.bin"), b"early").unwrap();
        // Alphabetical path order is the same tiebreak as the cache policy.
        fs::write(root.join("0000000000000002.bin"), b"later").unwrap();
        PRUNE_SORT_CALLS.with(|counter| counter.set(0));
        assert_eq!(prune_drive_blocks(&root, 5).unwrap(), 5);
        assert!(!root.join("0000000000000001.bin").exists());
        assert!(root.join("0000000000000002.bin").exists());
        assert_eq!(PRUNE_SORT_CALLS.with(|counter| counter.get()), 1);
        fs::remove_dir_all(root).unwrap();
    }

    /// Diagnostic-only cost profile of the *actual* cache-pruning code. The
    /// timing data is printed under --nocapture and has no flaky time limit.
    /// All input files are private tiny .bin files, below the 32 MiB policy.
    /// This does NOT measure FUSE or Telegram/cloud throughput.
    #[test]
    fn profile_prune_work_for_many_small_cached_project_files() {
        use std::time::Instant;
        let root = fixture();
        for count in [64usize, 1024, 4096] {
            let bucket = root.join(format!("profile-{count}"));
            fs::create_dir(&bucket).unwrap();
            for index in 0..count {
                fs::write(bucket.join(format!("{index:08x}.bin")), b"x").unwrap();
            }
            let mut micros = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                let removed = prune_drive_blocks(&bucket, DRIVE_CACHE_BYTES).unwrap();
                micros.push(start.elapsed().as_micros());
                assert_eq!(removed, 0, "cache entries under budget must remain");
            }
            micros.sort_unstable();
            eprintln!(
                "diagnostic_only_cache_prune entries={} median_us={} min_us={} max_us={}",
                count, micros[2], micros[0], micros[4]
            );
            assert_eq!(fs::read_dir(&bucket).unwrap().count(), count);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn neither_cleanup_follows_symlinked_cache_directories() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = fixture();
        fs::write(outside.join("secret.bin"), b"do not remove").unwrap();
        symlink(&outside, root.join("tdlib-turbo-files-v1-0")).unwrap();
        assert!(prune_legacy_file_caches(&root).is_err());
        assert!(outside.join("secret.bin").exists());
        assert!(cache_contains_files(&root).is_err());
        assert!(cache_contains_files(&root.join("tdlib-turbo-files-v1-0")).is_err());
        assert!(prune_drive_blocks(&root.join("tdlib-turbo-files-v1-0"), 0).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
