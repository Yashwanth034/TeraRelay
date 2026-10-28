//! Zero-upload copying of completed Drive files.
//! New independent entry identities refer to the existing immutable content;
//! no Telegram upload/download, host file I/O, or shared inode/hardlink is made.
use crate::commands::drive_metadata::{
    drive_snapshot, mark_drive_dirty, new_drive_id, portable_name_key, validate_drive_name,
    DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_COPY: usize = 100;
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCopyRow {
    pub entry_id: String,
    pub expected_name: String,
    pub expected_parent_id: Option<String>,
    pub new_name: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCopyRequest {
    pub revision: i64,
    pub destination_parent_id: Option<String>,
    pub files: Vec<FileCopyRow>,
}
#[derive(Debug, Serialize)]
pub struct FileCopyPreview {
    pub revision: i64,
    pub ready: bool,
    pub count: usize,
    pub total_bytes: u64,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct FileCopyApplied {
    pub copied: usize,
    pub revision: i64,
    pub dirty: bool,
}
type Commit = (
    FileCopyApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && !id
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
}
fn has_pending(snapshot: &DriveSnapshot, file: &DriveFileRecord) -> bool {
    snapshot.pending.iter().any(|p| {
        p.base_entry_id.as_deref() == Some(file.entry_id.as_str())
            || (p.directory_id == file.directory_id && p.display_name == file.display_name)
    })
}
fn validate(snapshot: &DriveSnapshot, request: &FileCopyRequest) -> Vec<String> {
    if request.revision != snapshot.revision {
        return vec!["Drive changed since selecting files; refresh and preview again.".into()];
    }
    if request.files.is_empty() || request.files.len() > MAX_COPY {
        return vec!["Select 1–100 distinct completed Drive files to copy.".into()];
    }
    let folders: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut problems = Vec::new();
    if let Some(parent) = request.destination_parent_id.as_deref() {
        if !safe_id(parent) || !folders.contains_key(parent) {
            problems.push("Destination directory is unavailable or trashed.".into());
        } else {
            let mut current = Some(parent);
            let mut visited = HashSet::new();
            while let Some(id) = current {
                if !visited.insert(id) || visited.len() > 512 {
                    problems.push("Destination directory has cyclic ancestry.".into());
                    break;
                }
                current = folders.get(id).and_then(|d| d.parent_id.as_deref());
                if current.is_some_and(|id| !folders.contains_key(id)) {
                    problems.push("Destination ancestry contains a missing folder.".into());
                    break;
                }
            }
        }
    }
    let mut selected: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snapshot.files {
        selected
            .entry(file.entry_id.as_str())
            .and_modify(|prior| {
                if file.storage_version > prior.storage_version {
                    *prior = file;
                }
            })
            .or_insert(file);
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut total = 0u64;
    for (index, row) in request.files.iter().enumerate() {
        let line = index + 1;
        if !safe_id(&row.entry_id) || !ids.insert(row.entry_id.as_str()) {
            problems.push(format!(
                "Row {line}: duplicate or invalid Drive file identity."
            ));
            continue;
        }
        let Some(file) = selected.get(row.entry_id.as_str()) else {
            problems.push(format!("Row {line}: completed source file is unavailable."));
            continue;
        };
        if file.display_name != row.expected_name
            || file.directory_id != row.expected_parent_id
            || file.inode_id.is_some()
            || has_pending(snapshot, file)
        {
            problems.push(format!(
                "Row {line}: file was renamed, moved or has pending edits."
            ));
            continue;
        }
        if validate_drive_name(&row.new_name).is_err() {
            problems.push(format!(
                "Row {line}: invalid cross-platform destination filename."
            ));
            continue;
        }
        let key = portable_name_key(&row.new_name);
        if !names.insert(key.clone()) {
            problems.push(format!(
                "Row {line}: another copy uses that destination filename."
            ));
            continue;
        }
        let parent = &request.destination_parent_id;
        let collision = snapshot.directories.iter().any(|d| {
            d.trashed_at.is_none() && &d.parent_id == parent && portable_name_key(&d.name) == key
        }) || snapshot
            .files
            .iter()
            .any(|f| &f.directory_id == parent && portable_name_key(&f.display_name) == key)
            || snapshot
                .pending
                .iter()
                .any(|p| &p.directory_id == parent && portable_name_key(&p.display_name) == key)
            || snapshot.symlinks.iter().any(|s| {
                s.trashed_at.is_none()
                    && &s.directory_id == parent
                    && portable_name_key(&s.display_name) == key
            });
        if collision {
            problems.push(format!(
                "Row {line}: destination name is occupied by another Drive item."
            ));
        }
        let size = file.plaintext_size.unwrap_or(file.total_size);
        match total.checked_add(size) {
            Some(next) => total = next,
            None => problems.push("Selected file sizes overflow the safe total.".into()),
        }
    }
    problems.truncate(100);
    problems
}
fn preview(
    conn: &sqlite::Connection,
    request: &FileCopyRequest,
) -> Result<FileCopyPreview, String> {
    let snap = drive_snapshot(conn)?;
    let problems = validate(&snap, request);
    let mut index: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snap.files {
        index
            .entry(file.entry_id.as_str())
            .and_modify(|prior| {
                if file.storage_version > prior.storage_version {
                    *prior = file;
                }
            })
            .or_insert(file);
    }
    let total_bytes = request
        .files
        .iter()
        .filter_map(|r| index.get(r.entry_id.as_str()))
        .map(|f| f.plaintext_size.unwrap_or(f.total_size))
        .try_fold(0u64, |acc, size| acc.checked_add(size))
        .unwrap_or(u64::MAX);
    Ok(FileCopyPreview {
        revision: snap.revision,
        ready: problems.is_empty(),
        count: request.files.len(),
        total_bytes,
        problems,
    })
}
pub(super) fn source_kind(
    conn: &sqlite::Connection,
    original: &DriveFileRecord,
) -> Result<&'static str, String> {
    // V2 is authoritative when an older legacy fallback has the same entry ID.
    // A legacy entry is only copied if there is NO active authoritative V2 row.
    let (kind, sql) = match original.storage_version {
        2 => (
            "v2",
            "SELECT 1 FROM drive_entries_v2 e JOIN drive_objects o
          ON o.object_id=e.object_id WHERE e.entry_id=? AND e.trashed_at IS NULL
          AND e.object_id=?",
        ),
        1 => (
            "legacy",
            "SELECT 1 FROM drive_file_entries e JOIN logical_files f
          ON f.file_id=e.file_id WHERE e.entry_id=? AND e.trashed_at IS NULL
          AND f.status='complete' AND e.file_id=?
          AND NOT EXISTS(SELECT 1 FROM drive_entries_v2 v JOIN drive_objects o
            ON o.object_id=v.object_id WHERE v.entry_id=e.entry_id AND v.trashed_at IS NULL)",
        ),
        _ => return Err("Unsupported completed Drive storage version.".into()),
    };
    let mut query = conn
        .prepare(sql)
        .map_err(|e: sqlite::Error| e.to_string())?;
    query
        .bind((1, original.entry_id.as_str()))
        .map_err(|e| e.to_string())?;
    query
        .bind((2, original.file_id.as_str()))
        .map_err(|e| e.to_string())?;
    if query.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Err(
            "Selected completed Drive content no longer matches its authoritative storage.".into(),
        );
    }
    Ok(kind)
}
pub(super) fn fresh_id(conn: &sqlite::Connection) -> Result<String, String> {
    for _ in 0..5 {
        let id = new_drive_id();
        let mut stmt = conn
            .prepare(
                "SELECT
              (SELECT COUNT(*) FROM drive_entries_v2 WHERE entry_id=?1)
              +(SELECT COUNT(*) FROM drive_file_entries WHERE entry_id=?1)
              +(SELECT COUNT(*) FROM drive_pending_files WHERE id=?1)
              +(SELECT COUNT(*) FROM drive_directories WHERE id=?1)
              +(SELECT COUNT(*) FROM drive_symlinks WHERE id=?1)",
            )
            .map_err(|e| e.to_string())?;
        stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
        if stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row
            && stmt.read::<i64, _>(0).map_err(|e| e.to_string())? == 0
        {
            return Ok(id);
        }
    }
    Err("Could not reserve a unique Drive copy identity.".into())
}
pub(super) fn insert_copy(
    conn: &sqlite::Connection,
    kind: &str,
    new_id: &str,
    source: &DriveFileRecord,
    destination: Option<&str>,
    new_name: &str,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp_millis();
    match kind {
        "v2" => {
            let mut stmt = conn.prepare(
                "INSERT INTO drive_entries_v2
                 (entry_id, object_id, inode_id, directory_id, display_name, created_at, updated_at, trashed_at)
                 SELECT ?, e.object_id, NULL, ?, ?, ?, ?, NULL
                 FROM drive_entries_v2 e JOIN drive_objects o ON o.object_id=e.object_id
                 WHERE e.entry_id=? AND e.object_id=? AND e.trashed_at IS NULL",
            ).map_err(|e| e.to_string())?;
            stmt.bind((1, new_id)).map_err(|e| e.to_string())?;
            stmt.bind((2, destination)).map_err(|e| e.to_string())?;
            stmt.bind((3, new_name)).map_err(|e| e.to_string())?;
            stmt.bind((4, now)).map_err(|e| e.to_string())?;
            stmt.bind((5, now)).map_err(|e| e.to_string())?;
            stmt.bind((6, source.entry_id.as_str()))
                .map_err(|e| e.to_string())?;
            stmt.bind((7, source.file_id.as_str()))
                .map_err(|e| e.to_string())?;
            stmt.next().map_err(|e| e.to_string())?;
        }
        "legacy" => {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                  trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
                 SELECT ?, e.file_id, ?, ?, ?, ?, NULL, e.encryption_version,
                        e.plaintext_size, e.crypto_id, e.content_fingerprint
                 FROM drive_file_entries e JOIN logical_files f ON f.file_id=e.file_id
                 WHERE e.entry_id=? AND e.file_id=? AND e.trashed_at IS NULL
                   AND f.status='complete'",
                )
                .map_err(|e| e.to_string())?;
            stmt.bind((1, new_id)).map_err(|e| e.to_string())?;
            stmt.bind((2, destination)).map_err(|e| e.to_string())?;
            stmt.bind((3, new_name)).map_err(|e| e.to_string())?;
            stmt.bind((4, now)).map_err(|e| e.to_string())?;
            stmt.bind((5, now)).map_err(|e| e.to_string())?;
            stmt.bind((6, source.entry_id.as_str()))
                .map_err(|e| e.to_string())?;
            stmt.bind((7, source.file_id.as_str()))
                .map_err(|e| e.to_string())?;
            stmt.next().map_err(|e| e.to_string())?;
        }
        _ => return Err("Unsupported Drive copy source.".into()),
    }
    let mut count = conn
        .prepare("SELECT changes()")
        .map_err(|e| e.to_string())?;
    if count.next().map_err(|e| e.to_string())? != sqlite::State::Row
        || count.read::<i64, _>(0).map_err(|e| e.to_string())? != 1
    {
        return Err("Drive copy insert was ignored; all copies rolled back.".into());
    }
    Ok(())
}
fn apply(conn: &sqlite::Connection, request: &FileCopyRequest) -> Result<Commit, String> {
    let snap = drive_snapshot(conn)?;
    let errors = validate(&snap, request);
    if !errors.is_empty() {
        return Err(errors.join(" "));
    }
    let mut index: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snap.files {
        index
            .entry(file.entry_id.as_str())
            .and_modify(|prior| {
                if file.storage_version > prior.storage_version {
                    *prior = file;
                }
            })
            .or_insert(file);
    }
    conn.execute("SAVEPOINT drive_instant_file_copies")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let mut entries = Vec::new();
        let mut inodes = Vec::new();
        for row in &request.files {
            let source = index
                .get(row.entry_id.as_str())
                .ok_or("Source file disappeared while copying.")?;
            let kind = source_kind(conn, source)?;
            let id = fresh_id(conn)?;
            insert_copy(
                conn,
                kind,
                &id,
                source,
                request.destination_parent_id.as_deref(),
                &row.new_name,
            )?;
            entries.push((request.destination_parent_id.clone(), row.new_name.clone()));
            inodes.push(("file".into(), id));
        }
        mark_drive_dirty(conn)?;
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= snap.revision {
            return Err("Drive metadata revision did not advance for file copies.".into());
        }
        for (row, (_, id)) in request.files.iter().zip(inodes.iter()) {
            let source = index
                .get(row.entry_id.as_str())
                .ok_or("Lost source during copy verification")?;
            let matches = after.files.iter().any(|file| {
                &file.entry_id == id && file.file_id == source.file_id
                    && file.inode_id.is_none() // independent inode, not shared hardlink
                    && file.directory_id == request.destination_parent_id
                    && file.display_name == row.new_name
                    && file.total_size == source.total_size
                    && file.encryption_version == source.encryption_version
                    && file.crypto_id == source.crypto_id
                    && file.content_fingerprint == source.content_fingerprint
            });
            if !matches {
                return Err("Drive copy was ignored or changed: rolling back every copy.".into());
            }
        }
        Ok((
            FileCopyApplied {
                copied: request.files.len(),
                revision: after.revision,
                dirty: after.dirty,
            },
            entries,
            inodes,
        ))
    })();
    match outcome {
        Ok(value) => {
            if let Err(error) = conn.execute("RELEASE drive_instant_file_copies") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_instant_file_copies; RELEASE drive_instant_file_copies",
                );
                return Err(error.to_string());
            }
            Ok(value)
        }
        Err(error) => {
            conn.execute(
                "ROLLBACK TO drive_instant_file_copies; RELEASE drive_instant_file_copies",
            )
            .map_err(|rollback| format!("{error}; rollback failed: {rollback}"))?;
            Err(error)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_file_copy_preview(
    db: State<'_, DbConnection>,
    request: FileCopyRequest,
) -> Result<FileCopyPreview, String> {
    let db = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&db, &request)
}
#[tauri::command]
pub fn cmd_drive_file_copy_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: FileCopyRequest,
) -> Result<FileCopyApplied, String> {
    let (result, entries, inodes) = {
        let db = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&db, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&entries, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&entries, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Drive copy committed but Windows refresh failed: {error}");
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Drive copy committed but macOS refresh failed: {error}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;
    fn fixture() -> sqlite::Connection {
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
             VALUES ('destination',NULL,'Copies',1,1);
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES ('source',500,'QA','owner',1,1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,
             mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('file1','source',2,'report.txt',12,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES ('entry1','file1',NULL,'report.txt',1,1);",
        ).unwrap();
        db
    }
    fn req(db: &sqlite::Connection) -> FileCopyRequest {
        FileCopyRequest {
            revision: drive_snapshot(db).unwrap().revision,
            destination_parent_id: Some("destination".into()),
            files: vec![FileCopyRow {
                entry_id: "entry1".into(),
                expected_name: "report.txt".into(),
                expected_parent_id: None,
                new_name: "report-copy.txt".into(),
            }],
        }
    }
    #[test]
    fn legacy_copy_reuses_content_with_independent_entry_identity() {
        let db = fixture();
        let request = req(&db);
        let (output, _, _) = apply(&db, &request).unwrap();
        assert_eq!(output.copied, 1);
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.files.len(), 2);
        assert!(snap
            .files
            .iter()
            .any(|f| f.entry_id == "entry1" && f.directory_id.is_none()));
        let copied = snap.files.iter().find(|f| f.entry_id != "entry1").unwrap();
        assert_eq!(copied.file_id, "file1");
        assert_eq!(copied.display_name, "report-copy.txt");
        assert_eq!(copied.directory_id.as_deref(), Some("destination"));
    }
    #[test]
    fn authoritative_v2_copy_ignores_legacy_fallback_at_same_entry_id() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_objects
             (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
             VALUES ('object2',12,0,NULL,'immutable-object-hash',0,1);
             INSERT INTO drive_entries_v2
             (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
             VALUES ('entry1','object2',NULL,NULL,'newer.txt',1,2,NULL);",
        ).unwrap();
        let snapshot = drive_snapshot(&db).unwrap();
        let authoritative = snapshot
            .files
            .iter()
            .find(|f| f.entry_id == "entry1" && f.storage_version == 2)
            .unwrap();
        assert_eq!(source_kind(&db, authoritative).unwrap(), "v2");
        let request = FileCopyRequest {
            revision: snapshot.revision,
            destination_parent_id: Some("destination".into()),
            files: vec![FileCopyRow {
                entry_id: "entry1".into(),
                expected_name: "newer.txt".into(),
                expected_parent_id: None,
                new_name: "duplicate-newer.txt".into(),
            }],
        };
        let (done, _, _) = apply(&db, &request).unwrap();
        assert_eq!(done.copied, 1);
        let after = drive_snapshot(&db).unwrap();
        assert!(after.files.iter().any(|f| f.file_id == "object2"
            && f.entry_id != "entry1"
            && f.display_name == "duplicate-newer.txt"));
        // A distinct V2 entry points at the same immutable remote object,
        // not the legacy logical_files fallback, and has no shared inode ID.
    }
    #[test]
    fn occupied_destination_rejected_without_copy() {
        let db = fixture();
        let mut request = req(&db);
        request.files[0].new_name = "report.txt".into();
        db.execute("INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
          VALUES('occupied','file1','destination','report.txt',1,1)").unwrap();
        assert!(!preview(&db, &request).unwrap().ready);
    }
    #[test]
    fn stale_revision_and_pending_alias_rejected() {
        let db = fixture();
        let mut request = req(&db);
        request.revision = -1;
        assert!(apply(&db, &request).is_err());
        request = req(&db);
        db.execute(
            "INSERT INTO drive_pending_files(id,directory_id,display_name,staging_path,size,
         format_version,backing_channel_id,base_entry_id,created_at,updated_at)
         VALUES('pending',NULL,'report.txt','/unaccessed',0,1,1,NULL,1,1)",
        )
        .unwrap();
        assert!(!preview(&db, &request).unwrap().ready);
    }
    #[test]
    fn sqlite_insert_abort_rolls_back_full_batch() {
        let db = fixture();
        db.execute("INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,
             mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('file2','source',3,'other.txt',13,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES ('entry2','file2',NULL,'other.txt',1,1);").unwrap();
        let mut request = req(&db);
        request.files.push(FileCopyRow {
            entry_id: "entry2".into(),
            expected_name: "other.txt".into(),
            expected_parent_id: None,
            new_name: "fail.txt".into(),
        });
        db.execute(
            "CREATE TRIGGER qa_copy_abort BEFORE INSERT ON drive_file_entries
          WHEN NEW.display_name='fail.txt' BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &request).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().revision, request.revision);
        assert_eq!(drive_snapshot(&db).unwrap().files.len(), 2);
    }
    #[test]
    fn ignored_copy_insert_is_not_success() {
        let db = fixture();
        let request = req(&db);
        db.execute(
            "CREATE TRIGGER qa_copy_ignore BEFORE INSERT ON drive_file_entries
           WHEN NEW.display_name='report-copy.txt' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &request).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().files.len(), 1);
    }
}
