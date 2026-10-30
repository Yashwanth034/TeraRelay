//! Native lookup/open lifetimes. Namespace removal does not end a reader's lifetime.
use crate::commands::drive_metadata::{DriveObjectBlockRecord, DriveObjectPartRecord};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

struct References<T> {
    lookups: u64,
    opens: u64,
    retired: Option<T>,
}
pub(super) struct InodeLifetimes<T> {
    entries: HashMap<u64, References<T>>,
}
impl<T> Default for InodeLifetimes<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}
impl<T> InodeLifetimes<T> {
    fn entry(&mut self, ino: u64) -> &mut References<T> {
        self.entries.entry(ino).or_insert(References {
            lookups: 0,
            opens: 0,
            retired: None,
        })
    }
    pub(super) fn lookup(&mut self, ino: u64) {
        let entry = self.entry(ino);
        entry.lookups = entry.lookups.saturating_add(1);
    }
    pub(super) fn open(&mut self, ino: u64) {
        let entry = self.entry(ino);
        entry.opens = entry.opens.saturating_add(1);
    }
    pub(super) fn referenced(&self, ino: u64) -> bool {
        self.entries
            .get(&ino)
            .is_some_and(|e| e.lookups != 0 || e.opens != 0)
    }
    pub(super) fn retired(&self, ino: u64) -> Option<&T> {
        self.entries.get(&ino).and_then(|e| e.retired.as_ref())
    }
    /// The caller drops an unneeded view; never retain an unreferenced inode.
    pub(super) fn retire(&mut self, ino: u64, view: T) {
        if self.referenced(ino) {
            let entry = self.entry(ino);
            // A restored identity can become visible again; keep its first detached view.
            if entry.retired.is_none() {
                entry.retired = Some(view);
            }
        }
    }
    fn prune(&mut self, ino: u64) {
        if !self.referenced(ino) {
            self.entries.remove(&ino);
        }
    }
    pub(super) fn forget(&mut self, ino: u64, count: u64) {
        if let Some(e) = self.entries.get_mut(&ino) {
            e.lookups = e.lookups.saturating_sub(count);
        }
        self.prune(ino);
    }
    pub(super) fn close(&mut self, ino: u64) {
        if let Some(e) = self.entries.get_mut(&ino) {
            e.opens = e.opens.saturating_sub(1);
        }
        self.prune(ino);
    }
}

pub(super) struct FrozenV2Locations {
    object_id: String,
    blocks: BTreeMap<u64, DriveObjectBlockRecord>,
    parts: BTreeMap<u64, DriveObjectPartRecord>,
}

// Count queries per test thread: two independent fixtures must not alter
// each other's assertions when cargo test runs them in parallel.
#[cfg(test)]
thread_local! {
    static FROZEN_V2_CAPTURE_QUERY_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_frozen_v2_capture_query_steps() {
    FROZEN_V2_CAPTURE_QUERY_STEPS.with(|steps| steps.set(0));
}

#[cfg(test)]
fn frozen_v2_capture_query_steps() -> usize {
    FROZEN_V2_CAPTURE_QUERY_STEPS.with(|steps| steps.get())
}

fn record_frozen_v2_capture_query_step() {
    #[cfg(test)]
    FROZEN_V2_CAPTURE_QUERY_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
}

fn retained_integer(stmt: &sqlite::Statement<'_>, field: &str) -> Result<i64, String> {
    // SQLite's typed read::<i64> can coerce a corrupt REAL/TEXT/BLOB value.
    // Frozen read locations must preserve the exact stored INTEGER identity.
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid retained Drive {field}: {e}"))?
    {
        sqlite::Value::Integer(value) => Ok(value),
        _ => Err(format!("Invalid retained Drive {field}")),
    }
}

// SQLite's String reader can coerce non-TEXT storage and truncate on an
// embedded NUL. Retained read identities and checksums must reflect every
// original UTF-8 byte, or an altered remote location could be trusted.
fn retained_text(stmt: &sqlite::Statement<'_>, field: &str) -> Result<String, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid retained Drive {field}: {e}"))?
    {
        sqlite::Value::String(_) => {}
        _ => return Err(format!("Invalid retained Drive {field} TEXT type")),
    }
    let bytes = stmt
        .read::<Vec<u8>, _>(field)
        .map_err(|e| format!("Invalid retained Drive {field}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!("Invalid retained Drive {field} contains NUL"));
    }
    String::from_utf8(bytes).map_err(|_| format!("Invalid retained Drive {field} UTF-8"))
}

fn retained_optional_text(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<String>, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid retained Drive {field}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::String(_) => retained_text(stmt, field).map(Some),
        _ => Err(format!("Invalid retained Drive {field} TEXT type")),
    }
}

fn retained_nonnegative_integer(stmt: &sqlite::Statement<'_>, field: &str) -> Result<u64, String> {
    u64::try_from(retained_integer(stmt, field)?)
        .map_err(|_| format!("Invalid retained Drive {field}"))
}

impl FrozenV2Locations {
    pub(super) fn capture(conn: &sqlite::Connection, object_id: &str) -> Result<Self, String> {
        crate::commands::drive_metadata::init_drive_schema(conn)?;
        record_frozen_v2_capture_query_step();
        let mut stmt = conn
            .prepare(
                "SELECT b.object_id AS block_object_id, b.chunk_index AS block_chunk_index,
                        b.part_index AS block_part_index, b.part_offset AS block_part_offset,
                        b.ciphertext_size AS block_ciphertext_size, b.sha256 AS block_sha256,
                        b.plaintext_sha256 AS block_plaintext_sha256,
                        p.object_id AS part_object_id, p.part_index AS part_part_index,
                        p.first_chunk_index AS part_first_chunk_index,
                        p.chunk_count AS part_chunk_count,
                        p.backing_channel_id AS part_backing_channel_id,
                        p.message_id AS part_message_id,
                        p.ciphertext_size AS part_ciphertext_size, p.sha256 AS part_sha256
                 FROM drive_object_blocks b
                 LEFT JOIN drive_object_parts p
                   ON p.object_id = b.object_id AND p.part_index = b.part_index
                 WHERE b.object_id = ?
                 ORDER BY b.chunk_index ASC",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, object_id)).map_err(|e| e.to_string())?;

        let mut blocks = Vec::new();
        let mut parts = BTreeMap::new();
        while let sqlite::State::Row = stmt.next().map_err(|e| e.to_string())? {
            let block = DriveObjectBlockRecord {
                object_id: retained_text(&stmt, "block_object_id")?,
                chunk_index: retained_nonnegative_integer(&stmt, "block_chunk_index")?,
                part_index: retained_nonnegative_integer(&stmt, "block_part_index")?,
                part_offset: retained_nonnegative_integer(&stmt, "block_part_offset")?,
                ciphertext_size: retained_nonnegative_integer(&stmt, "block_ciphertext_size")?,
                sha256: retained_text(&stmt, "block_sha256")?,
                plaintext_sha256: retained_text(&stmt, "block_plaintext_sha256")?,
            };
            if block.object_id != object_id {
                return Err("Invalid retained Drive object block identity".into());
            }
            let Some(part_object_id) = retained_optional_text(&stmt, "part_object_id")? else {
                return Err("Invalid retained Drive object block identity".into());
            };
            let part = DriveObjectPartRecord {
                object_id: part_object_id,
                part_index: retained_nonnegative_integer(&stmt, "part_part_index")?,
                first_chunk_index: retained_nonnegative_integer(&stmt, "part_first_chunk_index")?,
                chunk_count: retained_nonnegative_integer(&stmt, "part_chunk_count")?,
                backing_channel_id: retained_integer(&stmt, "part_backing_channel_id")?,
                message_id: retained_integer(&stmt, "part_message_id")?,
                ciphertext_size: retained_nonnegative_integer(&stmt, "part_ciphertext_size")?,
                sha256: retained_text(&stmt, "part_sha256")?,
            };
            if part.object_id != object_id {
                return Err("Invalid retained Drive object part identity".into());
            }
            match parts.get(&part.part_index) {
                Some(existing) if existing != &part => {
                    return Err("Invalid retained Drive object part identity".into())
                }
                Some(_) => {}
                None => {
                    parts.insert(part.part_index, part);
                }
            }
            blocks.push(block);
        }
        Self::new(object_id, blocks, parts.into_values().collect())
    }
    fn new(
        object_id: &str,
        blocks: Vec<DriveObjectBlockRecord>,
        parts: Vec<DriveObjectPartRecord>,
    ) -> Result<Self, String> {
        let mut frozen = Self {
            object_id: object_id.into(),
            blocks: BTreeMap::new(),
            parts: BTreeMap::new(),
        };
        for part in parts {
            if part.object_id != object_id
                || part.chunk_count == 0
                || part.ciphertext_size == 0
                || part
                    .first_chunk_index
                    .checked_add(part.chunk_count)
                    .is_none()
                || frozen.parts.insert(part.part_index, part).is_some()
            {
                return Err("Invalid retained Drive object part identity".into());
            }
        }
        for block in blocks {
            let Some(part) = frozen.parts.get(&block.part_index) else {
                return Err("Invalid retained Drive object block identity".into());
            };
            let chunk_end = part
                .first_chunk_index
                .checked_add(part.chunk_count)
                .ok_or("Invalid retained Drive object part range")?;
            let ciphertext_end = block
                .part_offset
                .checked_add(block.ciphertext_size)
                .ok_or("Invalid retained Drive object block range")?;
            if block.object_id != object_id
                || block.chunk_index < part.first_chunk_index
                || block.chunk_index >= chunk_end
                || block.ciphertext_size == 0
                || ciphertext_end > part.ciphertext_size
                || frozen.blocks.insert(block.chunk_index, block).is_some()
            {
                return Err("Invalid retained Drive object block identity or bounds".into());
            }
        }
        Ok(frozen)
    }
    pub(super) fn location(
        &self,
        object_id: &str,
        index: u64,
    ) -> Result<(DriveObjectBlockRecord, DriveObjectPartRecord), String> {
        if object_id != self.object_id {
            return Err("Retained Drive object identity mismatch".into());
        }
        let block = self
            .blocks
            .get(&index)
            .ok_or("Retained Drive block location is missing")?;
        let part = self
            .parts
            .get(&block.part_index)
            .ok_or("Retained Drive part location is missing")?;
        Ok((block.clone(), part.clone()))
    }
}

/// Apply only recorded local dirty bytes. Missing/truncated dirty data is an error.
pub(super) fn overlay_ranges(
    output: &mut [u8],
    offset: u64,
    ranges: &[(u64, u64)],
    mut read_local: impl FnMut(u64, &mut [u8]) -> Result<usize, String>,
) -> Result<(), String> {
    let end = offset
        .checked_add(output.len() as u64)
        .ok_or("Retained Drive read offset overflow")?;
    for &(start, stop) in ranges {
        if stop < start {
            return Err("Invalid retained Drive dirty range".into());
        }
        let from = start.max(offset);
        let to = stop.min(end);
        if from >= to {
            continue;
        }
        let slice = &mut output[(from - offset) as usize..(to - offset) as usize];
        if read_local(from, slice)? != slice.len() {
            return Err("Retained Drive dirty bytes are incomplete".into());
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}
// Recovery may recursively delete a staged directory once the database says
// its pending writer is gone. A redirected cache/drive/staging ancestor must
// never authorize that deletion, even if the final stage leaf looks valid.
fn verify_retained_stage_root(root: &Path) -> Result<(), String> {
    let drive = root
        .parent()
        .ok_or_else(|| "Retained Drive staging parent is missing".to_string())?;
    let cache = drive
        .parent()
        .ok_or_else(|| "Retained Drive cache parent is missing".to_string())?;
    for directory in [cache, drive, root] {
        let metadata = std::fs::symlink_metadata(directory)
            .map_err(|e| format!("Retained Drive staging ancestor is unavailable: {e}"))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err("Retained Drive staging ancestor is not a real directory".into());
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        Ok(_) => return Err("Retained Drive staging path is not a private directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|e| e.to_string())?;
        }
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

// Capture device/inode identities so a path substituted after preparation
// cannot authorize later recursive cleanup of a different directory.
fn retained_path_identity(path: &Path, directory: bool) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink()
        || (directory && !meta.file_type().is_dir())
        || (!directory && !meta.file_type().is_file())
    {
        return Err("Retained Drive owned path was replaced".into());
    }
    Ok((meta.dev(), meta.ino()))
}

/// Marker precedes the metadata transaction. Only committed removals own the stage.
pub(super) struct RetainedStage {
    stage: PathBuf,
    marker: PathBuf,
    root_identity: (u64, u64),
    stage_identity: (u64, u64),
    markers_identity: (u64, u64),
    marker_identity: (u64, u64),
    committed: bool,
}
impl RetainedStage {
    pub(super) fn prepare(root: &Path, id: &str, staging_path: &Path) -> Result<Self, String> {
        Self::prepare_with_sync(root, id, staging_path, |file, markers, root| {
            file.sync_all().map_err(|e| e.to_string())?;
            std::fs::File::open(markers)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| e.to_string())?;
            std::fs::File::open(root)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }
    fn prepare_with_sync(
        root: &Path,
        id: &str,
        staging_path: &Path,
        sync: impl FnOnce(&std::fs::File, &Path, &Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        if !valid_id(id) || staging_path.parent() != Some(root.join(id).as_path()) {
            return Err("Retained Drive staging ownership mismatch".into());
        }
        verify_retained_stage_root(root)?;
        let stage = root.join(id);
        private_directory(&stage)?;
        let markers = root.join(".retained-readers");
        private_directory(&markers)?;
        let marker = markers.join(id);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)
            .map_err(|e| e.to_string())?;
        // Own the marker before any fallible durability operation. Record
        // identities from real paths, not only path strings: cleanup may be
        // delayed until the last reader releases the open file.
        let owner = Self {
            root_identity: retained_path_identity(root, true)?,
            stage_identity: retained_path_identity(&stage, true)?,
            markers_identity: retained_path_identity(&markers, true)?,
            marker_identity: retained_path_identity(&marker, false)?,
            stage,
            marker,
            committed: false,
        };
        sync(&file, &markers, root)?;
        Ok(owner)
    }
    pub(super) fn committed(&mut self) {
        self.committed = true;
    }
}
impl Drop for RetainedStage {
    fn drop(&mut self) {
        let Some(root) = self.stage.parent() else {
            return;
        };
        let owned = (|| {
            verify_retained_stage_root(root)?;
            if retained_path_identity(root, true)? != self.root_identity
                || retained_path_identity(&self.stage, true)? != self.stage_identity
                || retained_path_identity(
                    self.marker
                        .parent()
                        .ok_or("Retained marker parent missing")?,
                    true,
                )? != self.markers_identity
                || retained_path_identity(&self.marker, false)? != self.marker_identity
            {
                return Err("Retained Drive cleanup target changed since preparation".into());
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = owned {
            log::warn!("Deferred unsafe retained Drive cleanup: {error}");
            return; // Never unlink an unverified stage or replacement marker.
        }
        if self.committed {
            if let Err(e) = std::fs::remove_dir_all(&self.stage) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("Deferred retained Drive staging cleanup: {e}");
                    return; // Keep marker for startup recovery.
                }
            }
        }
        let _ = std::fs::remove_file(&self.marker);
    }
}
pub(super) fn recover_retained_stages(
    root: &Path,
    mut pending_exists: impl FnMut(&str) -> Result<bool, String>,
) -> Result<(), String> {
    verify_retained_stage_root(root)?;
    let markers = root.join(".retained-readers");
    match std::fs::symlink_metadata(&markers) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => return Err("Invalid retained Drive recovery directory".into()),
        Err(e) => return Err(e.to_string()),
    }
    for (count, entry) in std::fs::read_dir(&markers)
        .map_err(|e| e.to_string())?
        .enumerate()
    {
        if count >= 100_000 {
            return Err("Too many retained Drive recovery markers".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_id(&id) || !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("Invalid retained Drive recovery marker".into());
        }
        if !pending_exists(&id)? {
            let stage = root.join(&id);
            match std::fs::symlink_metadata(&stage) {
                Ok(m) if m.file_type().is_dir() => {
                    std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("Invalid retained Drive recovery stage".into()),
            }
        }
        std::fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct View(Arc<AtomicUsize>);
    impl Drop for View {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn native_reader_lifetime_requires_both_lookup_and_open_release() {
        for close_first in [true, false] {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut refs = InodeLifetimes::default();
            refs.lookup(7);
            refs.lookup(7);
            refs.open(7);
            refs.open(7);
            refs.retire(7, View(drops.clone()));
            refs.forget(7, 1);
            refs.close(7);
            assert!(refs.retired(7).is_some());
            if close_first {
                refs.close(7);
            } else {
                refs.forget(7, 1);
            }
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            if close_first {
                refs.forget(7, 1);
            } else {
                refs.close(7);
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(refs.entries.is_empty());
            refs.forget(7, u64::MAX);
            refs.close(7);
            assert!(refs.entries.is_empty());
        }
    }
    #[test]
    fn native_reader_lifetime_lookup_can_open_after_replacement_and_forget() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut refs = InodeLifetimes::default();
        refs.lookup(9);
        refs.retire(9, View(drops.clone()));
        refs.open(9);
        refs.forget(9, 1);
        assert!(refs.retired(9).is_some());
        refs.close(9);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        refs.retire(10, View(drops.clone()));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
    fn fixture() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("terarelay-reader-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        root
    }
    #[test]
    fn native_reader_overlay_keeps_base_holes_and_real_local_bytes() {
        use std::os::unix::fs::FileExt;
        let root = fixture();
        let path = root.join("chunk");
        std::fs::write(&path, b"0123456789").unwrap();
        let local = std::fs::File::open(path).unwrap();
        let mut output = b"abcdefgh".to_vec();
        overlay_ranges(
            &mut output,
            2,
            &[(0, 1), (3, 6), (8, 20)],
            |start, bytes| local.read_at(bytes, start).map_err(|e| e.to_string()),
        )
        .unwrap();
        assert_eq!(output, b"a345ef89");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_reader_overlay_rejects_missing_truncated_and_overflowed_dirty_data() {
        let mut bytes = [0; 4];
        assert!(overlay_ranges(&mut bytes, 0, &[(0, 4)], |_, _| Err(
            "missing local chunk".into()
        ))
        .is_err());
        assert!(overlay_ranges(&mut bytes, 0, &[(0, 4)], |_, _| Ok(3)).is_err());
        assert!(overlay_ranges(&mut bytes, u64::MAX, &[], |_, _| unreachable!()).is_err());
        assert!(overlay_ranges(&mut bytes, 0, &[(4, 1)], |_, _| unreachable!()).is_err());
    }
    #[test]
    fn native_reader_frozen_locations_do_not_follow_live_metadata() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute("INSERT INTO drive_object_parts(object_id,part_index,first_chunk_index,chunk_count,backing_channel_id,message_id,ciphertext_size,sha256)
              VALUES('obj',0,0,2,42,99,8,'part-zero'), ('obj',1,2,1,73,100,4,'part-one');
          INSERT INTO drive_object_blocks(object_id,chunk_index,part_index,part_offset,ciphertext_size,sha256,plaintext_sha256)
              VALUES('obj',0,0,0,4,'block-zero','plain-zero'),
                    ('obj',1,0,4,4,'block-one','plain-one'),
                    ('obj',2,1,0,4,'block-two','plain-two');").unwrap();
        reset_frozen_v2_capture_query_steps();
        let frozen = FrozenV2Locations::capture(&conn, "obj").unwrap();
        assert_eq!(frozen_v2_capture_query_steps(), 1);
        conn.execute("DELETE FROM drive_object_blocks; DELETE FROM drive_object_parts;")
            .unwrap();
        let (block0, part0) = frozen.location("obj", 0).unwrap();
        assert_eq!(
            (
                block0.chunk_index,
                block0.part_offset,
                block0.sha256.as_str()
            ),
            (0, 0, "block-zero")
        );
        assert_eq!(
            (part0.part_index, part0.message_id, part0.sha256.as_str()),
            (0, 99, "part-zero")
        );
        let (block1, part1) = frozen.location("obj", 1).unwrap();
        assert_eq!(
            (
                block1.chunk_index,
                block1.part_offset,
                block1.sha256.as_str()
            ),
            (1, 4, "block-one")
        );
        assert_eq!(
            (part1.part_index, part1.message_id, part1.sha256.as_str()),
            (0, 99, "part-zero")
        );
        let (block2, part2) = frozen.location("obj", 2).unwrap();
        assert_eq!(
            (
                block2.chunk_index,
                block2.part_offset,
                block2.sha256.as_str()
            ),
            (2, 0, "block-two")
        );
        assert_eq!(
            (part2.part_index, part2.message_id, part2.sha256.as_str()),
            (1, 100, "part-one")
        );
        assert!(frozen.location("obj", 3).is_err());
        assert!(frozen.location("other", 0).is_err());
        assert!(FrozenV2Locations::capture(&conn, "obj")
            .unwrap()
            .location("obj", 0)
            .is_err());
    }
    #[test]
    fn retained_v2_read_locations_reject_corrupt_chunk_and_ciphertext_bounds() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_object_parts
                (object_id,part_index,first_chunk_index,chunk_count,backing_channel_id,
                 message_id,ciphertext_size,sha256)
             VALUES ('bounded-object',0,0,2,-100123,100,8,'part-hash');
             INSERT INTO drive_object_blocks
                (object_id,chunk_index,part_index,part_offset,ciphertext_size,
                 sha256,plaintext_sha256)
             VALUES ('bounded-object',0,0,0,4,'first','first-plain'),
                    ('bounded-object',1,0,4,4,'second','second-plain');",
        )
        .unwrap();
        assert!(FrozenV2Locations::capture(&conn, "bounded-object").is_ok());
        for (table, field, damaged, original, selector) in [
            (
                "drive_object_blocks",
                "chunk_index",
                3,
                0,
                "AND chunk_index=0",
            ),
            (
                "drive_object_blocks",
                "part_offset",
                5,
                0,
                "AND chunk_index=0",
            ),
            (
                "drive_object_blocks",
                "ciphertext_size",
                0,
                4,
                "AND chunk_index=0",
            ),
            (
                "drive_object_blocks",
                "ciphertext_size",
                9,
                4,
                "AND chunk_index=0",
            ),
            ("drive_object_parts", "first_chunk_index", 1, 0, ""),
            ("drive_object_parts", "chunk_count", 0, 2, ""),
            ("drive_object_parts", "chunk_count", 1, 2, ""),
            ("drive_object_parts", "ciphertext_size", 0, 8, ""),
            ("drive_object_parts", "ciphertext_size", 7, 8, ""),
        ] {
            conn.execute(format!(
                "UPDATE {table} SET {field}={damaged} WHERE object_id='bounded-object' {selector}"
            ))
            .unwrap();
            assert!(
                FrozenV2Locations::capture(&conn, "bounded-object").is_err(),
                "retained reader accepted {table}.{field}={damaged}"
            );
            let restore_selector = if field == "chunk_index" {
                "AND chunk_index=3"
            } else {
                selector
            };
            conn.execute(format!(
                "UPDATE {table} SET {field}={original} WHERE object_id='bounded-object' {restore_selector}"
            ))
            .unwrap();
            assert!(
                FrozenV2Locations::capture(&conn, "bounded-object").is_ok(),
                "restoring {table}.{field} must restore readable locations"
            );
        }
        let frozen = FrozenV2Locations::capture(&conn, "bounded-object").unwrap();
        assert_eq!(
            frozen.location("bounded-object", 0).unwrap().0.part_offset,
            0
        );
        assert_eq!(
            frozen.location("bounded-object", 1).unwrap().0.part_offset,
            4
        );
    }

    #[test]
    fn native_reader_frozen_location_capture_rejects_missing_parts() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute("INSERT INTO drive_object_blocks(object_id,chunk_index,part_index,part_offset,ciphertext_size,sha256,plaintext_sha256)
             VALUES('orphan',0,7,0,4,'hash','plain');").unwrap();
        reset_frozen_v2_capture_query_steps();
        assert_eq!(
            FrozenV2Locations::capture(&conn, "orphan").err().unwrap(),
            "Invalid retained Drive object block identity"
        );
        assert_eq!(frozen_v2_capture_query_steps(), 1);
    }

    #[test]
    fn native_reader_frozen_capture_rejects_negative_block_and_part_fields() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_object_parts
                (object_id, part_index, first_chunk_index, chunk_count, backing_channel_id,
                 message_id, ciphertext_size, sha256)
             VALUES ('negative-object', 0, 0, 1, 42, 100, 8, 'part-hash');
             INSERT INTO drive_object_blocks
                (object_id, chunk_index, part_index, part_offset, ciphertext_size,
                 sha256, plaintext_sha256)
             VALUES ('negative-object', 0, 0, 0, 8, 'block-hash', 'plain-hash');",
        )
        .unwrap();

        // Existing schema versions do not have CHECK constraints here. An
        // interrupted or corrupted metadata row must not become u64::MAX.
        for (table, column, original) in [
            ("drive_object_blocks", "chunk_index", 0),
            ("drive_object_blocks", "part_offset", 0),
            ("drive_object_blocks", "ciphertext_size", 8),
            ("drive_object_parts", "first_chunk_index", 0),
            ("drive_object_parts", "chunk_count", 1),
            ("drive_object_parts", "ciphertext_size", 8),
        ] {
            conn.execute(format!("UPDATE {table} SET {column} = -1;"))
                .unwrap();
            let result = FrozenV2Locations::capture(&conn, "negative-object");
            assert!(
                result.is_err(),
                "retained reader accepted a negative {table}.{column} as an unsigned value"
            );
            conn.execute(format!("UPDATE {table} SET {column} = {original};"))
                .unwrap();
        }
        // Joined negative indexes can exist in both tables at once, so a
        // matching part must not mask a corrupted block/part identity.
        conn.execute(
            "UPDATE drive_object_parts SET part_index=-1;
             UPDATE drive_object_blocks SET part_index=-1;",
        )
        .unwrap();
        assert!(FrozenV2Locations::capture(&conn, "negative-object").is_err());
        conn.execute(
            "UPDATE drive_object_parts SET part_index=0;
             UPDATE drive_object_blocks SET part_index=0;",
        )
        .unwrap();
        assert!(FrozenV2Locations::capture(&conn, "negative-object").is_ok());
    }

    #[test]
    fn native_reader_frozen_capture_rejects_coerced_numeric_storage_types() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute("INSERT INTO drive_object_parts (object_id,part_index,first_chunk_index,chunk_count,backing_channel_id,message_id,ciphertext_size,sha256) VALUES ('type-check',0,0,1,-100123,100,8,'part-hash'); INSERT INTO drive_object_blocks (object_id,chunk_index,part_index,part_offset,ciphertext_size,sha256,plaintext_sha256) VALUES ('type-check',0,0,0,8,'block-hash','plain-hash');").unwrap();
        assert!(FrozenV2Locations::capture(&conn, "type-check").is_ok());
        for (table, column, original) in [
            ("drive_object_blocks", "chunk_index", 0),
            ("drive_object_blocks", "part_index", 0),
            ("drive_object_blocks", "part_offset", 0),
            ("drive_object_blocks", "ciphertext_size", 8),
            ("drive_object_parts", "part_index", 0),
            ("drive_object_parts", "first_chunk_index", 0),
            ("drive_object_parts", "chunk_count", 1),
            ("drive_object_parts", "backing_channel_id", -100123),
            ("drive_object_parts", "message_id", 100),
            ("drive_object_parts", "ciphertext_size", 8),
        ] {
            for corrupt in ["0.5", "'corrupt'", "x'3030'"] {
                conn.execute(format!("UPDATE {table} SET {column}={corrupt}"))
                    .unwrap();
                assert!(
                    FrozenV2Locations::capture(&conn, "type-check").is_err(),
                    "retained reader accepted {table}.{column} stored as {corrupt}"
                );
                conn.execute(format!("UPDATE {table} SET {column}={original}"))
                    .unwrap();
            }
        }
        assert!(FrozenV2Locations::capture(&conn, "type-check").is_ok());
    }

    #[test]
    fn retained_v2_checksum_metadata_rejects_nul_and_blob_storage() {
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_object_parts
                (object_id,part_index,first_chunk_index,chunk_count,backing_channel_id,
                 message_id,ciphertext_size,sha256)
             VALUES ('exact-checksum',0,0,1,-100123,17,8,'part-hash');
             INSERT INTO drive_object_blocks
                (object_id,chunk_index,part_index,part_offset,ciphertext_size,
                 sha256,plaintext_sha256)
             VALUES ('exact-checksum',0,0,0,8,'block-hash','plain-hash');",
        )
        .unwrap();
        assert!(FrozenV2Locations::capture(&conn, "exact-checksum").is_ok());
        for (table, field, original) in [
            ("drive_object_blocks", "sha256", "'block-hash'"),
            ("drive_object_blocks", "plaintext_sha256", "'plain-hash'"),
            ("drive_object_parts", "sha256", "'part-hash'"),
        ] {
            for corrupt in [
                "'valid-prefix'||char(0)||'malicious-suffix'",
                "x'FFFE'",
                "x'76616c6964'",
            ] {
                conn.execute(format!("UPDATE {table} SET {field}={corrupt}"))
                    .unwrap();
                assert!(
                    FrozenV2Locations::capture(&conn, "exact-checksum").is_err(),
                    "retained V2 location accepted corrupt {table}.{field}={corrupt}"
                );
                conn.execute(format!("UPDATE {table} SET {field}={original}"))
                    .unwrap();
            }
        }
        assert!(FrozenV2Locations::capture(&conn, "exact-checksum").is_ok());
    }

    #[test]
    fn retained_text_decoder_preserves_exact_sqlite_type_and_full_bytes() {
        let conn = sqlite::open(":memory:").unwrap();
        let mut query = conn
            .prepare(
                "SELECT 'valid-identity' AS accepted,
                        'valid-identity'||char(0)||'hidden' AS truncated,
                        x'76616c6964' AS blob,
                        2.5 AS real_value,
                        x'FF' AS invalid_utf8,
                        NULL AS absent",
            )
            .unwrap();
        assert_eq!(query.next().unwrap(), sqlite::State::Row);
        assert_eq!(retained_text(&query, "accepted").unwrap(), "valid-identity");
        for field in ["truncated", "blob", "real_value", "invalid_utf8", "absent"] {
            assert!(retained_text(&query, field).is_err(), "accepted {field}");
        }
        assert_eq!(retained_optional_text(&query, "absent").unwrap(), None);
        assert_eq!(
            retained_optional_text(&query, "accepted")
                .unwrap()
                .as_deref(),
            Some("valid-identity")
        );
        for field in ["truncated", "blob", "real_value", "invalid_utf8"] {
            assert!(
                retained_optional_text(&query, field).is_err(),
                "accepted {field}"
            );
        }
        let source = include_str!("drive_read_lifetime.rs");
        let capture = source
            .split_once("impl FrozenV2Locations {")
            .unwrap()
            .1
            .split_once("    fn new(\n")
            .unwrap()
            .0;
        for field in [
            "block_object_id",
            "block_sha256",
            "block_plaintext_sha256",
            "part_sha256",
        ] {
            assert!(capture.contains(&format!("retained_text(&stmt, \"{field}\")")));
        }
        assert!(capture.contains("retained_optional_text(&stmt, \"part_object_id\")"));
    }

    #[cfg(unix)]
    #[test]
    fn retained_reader_drop_never_deletes_replaced_staging_ancestors_or_stage() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let id = "0123456789abcdef0123456789abcdef";
        let stage = root.join(id);
        std::fs::create_dir(&stage).unwrap();
        let pending = stage.join(".pending");
        std::fs::write(&pending, b"owned").unwrap();
        let mut owner = RetainedStage::prepare(&root, id, &pending).unwrap();
        owner.committed();

        let original = root.with_extension("original-stage-root");
        std::fs::rename(&root, &original).unwrap();
        let unrelated = fixture();
        std::fs::create_dir(unrelated.join(id)).unwrap();
        std::fs::create_dir(unrelated.join(".retained-readers")).unwrap();
        std::fs::write(unrelated.join(id).join("sentinel"), b"do not delete").unwrap();
        std::fs::write(
            unrelated.join(".retained-readers").join(id),
            b"foreign marker",
        )
        .unwrap();
        symlink(&unrelated, &root).unwrap();
        drop(owner);
        assert_eq!(
            std::fs::read(unrelated.join(id).join("sentinel")).unwrap(),
            b"do not delete"
        );
        assert_eq!(
            std::fs::read(unrelated.join(".retained-readers").join(id)).unwrap(),
            b"foreign marker"
        );
        // Restore the owned root; its original durable marker permits the
        // normal startup recovery to finish the interrupted deletion.
        std::fs::remove_file(&root).unwrap();
        std::fs::rename(&original, &root).unwrap();
        assert!(root.join(".retained-readers").join(id).exists());
        recover_retained_stages(&root, |_| Ok(false)).unwrap();
        assert!(!root.join(id).exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(unrelated).unwrap();

        let root = fixture();
        let stage = root.join(id);
        std::fs::create_dir(&stage).unwrap();
        let pending = stage.join(".pending");
        std::fs::write(&pending, b"owned").unwrap();
        let mut owner = RetainedStage::prepare(&root, id, &pending).unwrap();
        owner.committed();
        let saved_stage = root.join("saved-owned-stage");
        std::fs::rename(&stage, &saved_stage).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("sentinel"), b"new stage").unwrap();
        drop(owner);
        assert_eq!(std::fs::read(stage.join("sentinel")).unwrap(), b"new stage");
        assert!(root.join(".retained-readers").join(id).exists());
        std::fs::remove_dir_all(&stage).unwrap();
        std::fs::rename(&saved_stage, &stage).unwrap();
        recover_retained_stages(&root, |_| Ok(false)).unwrap();
        assert!(!stage.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_reader_stage_failed_operation_preserves_source_and_committed_drop_cleans() {
        let root = fixture();
        let id = "0123456789abcdef0123456789abcdef";
        let stage = root.join(id);
        std::fs::create_dir(&stage).unwrap();
        let path = stage.join(".pending");
        std::fs::write(&path, b"retained bytes").unwrap();
        drop(RetainedStage::prepare(&root, id, &path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained bytes");
        let mut owner = RetainedStage::prepare(&root, id, &path).unwrap();
        owner.committed();
        assert!(path.exists());
        drop(owner);
        assert!(!stage.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_reader_stage_sync_failure_keeps_source_and_allows_immediate_retry() {
        let root = fixture();
        let id = "0123456789abcdef0123456789abcdef";
        let stage = root.join(id);
        std::fs::create_dir(&stage).unwrap();
        let path = stage.join(".pending");
        std::fs::write(&path, b"source remains").unwrap();
        let error = RetainedStage::prepare_with_sync(&root, id, &path, |_, _, _| {
            Err("injected marker sync failure".into())
        })
        .err()
        .unwrap();
        assert_eq!(error, "injected marker sync failure");
        assert_eq!(std::fs::read(&path).unwrap(), b"source remains");
        assert!(!root.join(".retained-readers").join(id).exists());
        drop(RetainedStage::prepare(&root, id, &path).unwrap());
        assert!(path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retained_reader_recovery_rejects_symlinked_staging_and_drive_ancestors() {
        use std::os::unix::fs::symlink;
        let container = fixture();
        let external_drive = container.join("external-drive");
        let actual_stage = external_drive.join("staging");
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let stage = actual_stage.join(id);
        let marker_dir = actual_stage.join(".retained-readers");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir(&marker_dir).unwrap();
        std::fs::write(stage.join("sentinel"), b"private user data").unwrap();
        std::fs::write(marker_dir.join(id), b"old transaction marker").unwrap();

        // Substituting the staging root must not allow recovery to recursively
        // delete the external stage, even when its leaf and marker look valid.
        let alias_root = container.join("alias-staging");
        symlink(&actual_stage, &alias_root).unwrap();
        assert!(recover_retained_stages(&alias_root, |_| Ok(false)).is_err());
        assert!(
            RetainedStage::prepare(&alias_root, id, &alias_root.join(id).join(".pending")).is_err()
        );
        assert_eq!(
            std::fs::read(stage.join("sentinel")).unwrap(),
            b"private user data"
        );

        // A real `staging` leaf behind a substituted `drive` ancestor is just
        // as unsafe; check the app-owned parents, not only the final directory.
        let link_drive = container.join("drive");
        symlink(&external_drive, &link_drive).unwrap();
        let redirected_root = link_drive.join("staging");
        assert!(recover_retained_stages(&redirected_root, |_| Ok(false)).is_err());
        assert!(RetainedStage::prepare(
            &redirected_root,
            id,
            &redirected_root.join(id).join(".pending"),
        )
        .is_err());
        assert_eq!(
            std::fs::read(stage.join("sentinel")).unwrap(),
            b"private user data"
        );
        assert!(
            marker_dir.join(id).exists(),
            "recovery must preserve foreign markers"
        );
        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn native_reader_stage_recovery_keeps_live_and_unmarked_stages() {
        let root = fixture();
        for (id, live) in [
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true),
            ("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", false),
        ] {
            let stage = root.join(id);
            std::fs::create_dir(&stage).unwrap();
            let path = stage.join(".pending");
            std::fs::write(&path, b"owned").unwrap();
            let owner = RetainedStage::prepare(&root, id, &path).unwrap();
            std::mem::forget(owner); // Simulate process loss before/after the DB mutation.
            recover_retained_stages(&root, |candidate| {
                assert_eq!(candidate, id);
                Ok(live)
            })
            .unwrap();
            assert_eq!(stage.exists(), live);
        }
        let untouched = root.join("cccccccccccccccccccccccccccccccc");
        std::fs::create_dir(&untouched).unwrap();
        recover_retained_stages(&root, |_| unreachable!()).unwrap();
        assert!(untouched.exists());
        assert!(RetainedStage::prepare(&root, "../outside", &untouched.join(".pending")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
