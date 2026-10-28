//! Atomic, preview-gated soft deletion of 1–100 completed Drive file entries.
//! Never deletes remote Telegram content or host files; items stay in Trash
//! for later restore. All updates and the Drive revision share one SAVEPOINT.
use crate::commands::drive_file_copy::source_kind;
use crate::commands::drive_metadata::{
    drive_snapshot, trash_drive_file, validate_drive_name, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_FILES: usize = 100;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchTrashFile {
    entry_id: String,
    expected_name: String,
    expected_parent_id: Option<String>,
    expected_storage_version: i64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchTrashRequest {
    revision: i64,
    files: Vec<BatchTrashFile>,
}
#[derive(Debug, Serialize)]
pub struct BatchTrashPreview {
    revision: i64,
    ready: bool,
    count: usize,
    logical_bytes: u64,
    problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BatchTrashApplied {
    trashed: usize,
    revision: i64,
    dirty: bool,
}
type Commit = (
    BatchTrashApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn selected<'a>(
    conn: &sqlite::Connection,
    snapshot: &'a DriveSnapshot,
    req: &BatchTrashRequest,
) -> Result<Vec<&'a DriveFileRecord>, Vec<String>> {
    if req.revision != snapshot.revision {
        return Err(vec![
            "Drive changed since selection; refresh and preview again.".into(),
        ]);
    }
    if req.files.is_empty() || req.files.len() > MAX_FILES {
        return Err(vec![
            "Select 1–100 distinct completed files to move to Recently Deleted.".into(),
        ]);
    }
    let mut indexed: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snapshot.files {
        indexed
            .entry(file.entry_id.as_str())
            .and_modify(|existing| {
                if file.storage_version > existing.storage_version {
                    *existing = file;
                }
            })
            .or_insert(file);
    }
    let mut errors = Vec::new();
    let mut ids = HashSet::new();
    let mut output = Vec::new();
    for (index, row) in req.files.iter().enumerate() {
        let line = index + 1;
        if !safe_id(&row.entry_id) || !ids.insert(row.entry_id.as_str()) {
            errors.push(format!("Row {line}: invalid or duplicate file identity."));
            continue;
        }
        let Some(file) = indexed.get(row.entry_id.as_str()).copied() else {
            errors.push(format!(
                "Row {line}: source file disappeared or is unavailable."
            ));
            continue;
        };
        if file.display_name != row.expected_name
            || file.directory_id != row.expected_parent_id
            || i64::from(file.storage_version) != row.expected_storage_version
            || validate_drive_name(&file.display_name).is_err()
            || file.inode_id.is_some()
        {
            errors.push(format!(
                "Row {line}: source identity, location, version or inode changed."
            ));
            continue;
        }
        if snapshot.pending.iter().any(|p| {
            p.base_entry_id.as_deref() == Some(file.entry_id.as_str())
                || (p.directory_id == file.directory_id && p.display_name == file.display_name)
        }) {
            errors.push(format!(
                "Row {line}: an unfinished write shadows the source file."
            ));
            continue;
        }
        if let Err(reason) = source_kind(conn, file) {
            errors.push(format!(
                "Row {line}: backing content is not safely available: {reason}"
            ));
            continue;
        }
        output.push(file);
    }
    if errors.is_empty() {
        Ok(output)
    } else {
        errors.truncate(100);
        Err(errors)
    }
}
fn logical_bytes(rows: &[&DriveFileRecord]) -> Result<u64, String> {
    rows.iter().try_fold(0u64, |bytes, row| {
        bytes
            .checked_add(row.plaintext_size.unwrap_or(row.total_size))
            .ok_or_else(|| "Selected logical byte total overflow".into())
    })
}
fn preview(
    conn: &sqlite::Connection,
    req: &BatchTrashRequest,
) -> Result<BatchTrashPreview, String> {
    let snap = drive_snapshot(conn)?;
    Ok(match selected(conn, &snap, req) {
        Ok(rows) => match logical_bytes(&rows) {
            Ok(bytes) => BatchTrashPreview {
                revision: snap.revision,
                ready: true,
                count: rows.len(),
                logical_bytes: bytes,
                problems: vec![],
            },
            Err(problem) => BatchTrashPreview {
                revision: snap.revision,
                ready: false,
                count: req.files.len(),
                logical_bytes: 0,
                problems: vec![problem],
            },
        },
        Err(problems) => BatchTrashPreview {
            revision: snap.revision,
            ready: false,
            count: req.files.len(),
            logical_bytes: 0,
            problems,
        },
    })
}
fn apply(conn: &sqlite::Connection, req: &BatchTrashRequest) -> Result<Commit, String> {
    let before = drive_snapshot(conn)?;
    let entries = selected(conn, &before, req).map_err(|problems| problems.join(" "))?;
    let _size = logical_bytes(&entries)?;
    conn.execute("SAVEPOINT drive_atomic_batch_trash")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let mut locations = Vec::new();
        let mut inodes = Vec::new();
        for file in &entries {
            // Uses the normal authoritative V2-or-legacy single-item Trash
            // implementation, with its own nested savepoint and inode guard.
            trash_drive_file(conn, &file.entry_id)?;
            locations.push((file.directory_id.clone(), file.display_name.clone()));
            inodes.push(("file".to_owned(), file.entry_id.clone()));
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= before.revision {
            return Err("Drive did not advance metadata revision after batch trash.".into());
        }
        for file in &entries {
            if after.files.iter().any(|f| f.entry_id == file.entry_id) {
                return Err(
                    "A selected file remained active; rolling back the entire trash batch.".into(),
                );
            }
            // The normal snapshot hides unavailable and pending entries, so
            // verify raw active V1/V2 database rows rather than trusting it.
            let mut check = conn.prepare(
                "SELECT COUNT(*) FROM drive_file_entries WHERE entry_id=?1 AND trashed_at IS NULL
                 UNION ALL
                 SELECT COUNT(*) FROM drive_entries_v2 WHERE entry_id=?1 AND trashed_at IS NULL",
            ).map_err(|e:sqlite::Error|e.to_string())?;
            check
                .bind((1, file.entry_id.as_str()))
                .map_err(|e| e.to_string())?;
            while let sqlite::State::Row = check.next().map_err(|e| e.to_string())? {
                if check.read::<i64, _>(0).map_err(|e| e.to_string())? != 0 {
                    return Err("A selected raw Drive file remained active; rollback.".into());
                }
            }
        }
        Ok((
            BatchTrashApplied {
                trashed: entries.len(),
                revision: after.revision,
                dirty: after.dirty,
            },
            locations,
            inodes,
        ))
    })();
    match outcome {
        Ok(result) => {
            if let Err(e) = conn.execute("RELEASE drive_atomic_batch_trash") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_atomic_batch_trash; RELEASE drive_atomic_batch_trash",
                );
                return Err(e.to_string());
            }
            Ok(result)
        }
        Err(reason) => {
            conn.execute("ROLLBACK TO drive_atomic_batch_trash; RELEASE drive_atomic_batch_trash")
                .map_err(|e| format!("{reason}; rollback failed: {e}"))?;
            Err(reason)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_batch_trash_preview(
    db: State<'_, DbConnection>,
    request: BatchTrashRequest,
) -> Result<BatchTrashPreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_batch_trash_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: BatchTrashRequest,
) -> Result<BatchTrashApplied, String> {
    let (result, locations, inodes) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&locations, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&locations, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Drive batch trash committed but Windows refresh failed: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Drive batch trash committed but macOS refresh failed: {e}");
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
                logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER UNIQUE,
                name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
             CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,
                total_size INTEGER NOT NULL,mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,
                whole_sha256 TEXT,storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'complete');",
        )
        .unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('docs',NULL,'Documents',1,1);
             INSERT INTO logical_channels
               (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES('qa',47,'owner','owner',1,1,1);
             INSERT INTO logical_files
               (file_id,logical_channel_id,first_message_id,display_name,total_size,
                mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('obj1','qa',5,'report.txt',12,'text/plain',1,'sha',1,1,'complete'),
                   ('obj2','qa',6,'invoice.txt',15,'text/plain',1,'sha2',1,1,'complete');
             INSERT INTO drive_file_entries
               (entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('file1','obj1','docs','report.txt',1,1),
                   ('file2','obj2','docs','invoice.txt',1,1);",
        )
        .unwrap();
        db
    }
    fn request(db: &sqlite::Connection) -> BatchTrashRequest {
        BatchTrashRequest {
            revision: drive_snapshot(db).unwrap().revision,
            files: vec![
                BatchTrashFile {
                    entry_id: "file1".into(),
                    expected_name: "report.txt".into(),
                    expected_parent_id: Some("docs".into()),
                    expected_storage_version: 1,
                },
                BatchTrashFile {
                    entry_id: "file2".into(),
                    expected_name: "invoice.txt".into(),
                    expected_parent_id: Some("docs".into()),
                    expected_storage_version: 1,
                },
            ],
        }
    }
    #[test]
    fn two_file_batch_is_atomic_and_moves_entries_to_recently_deleted() {
        let db = fixture();
        let r = request(&db);
        let p = preview(&db, &r).unwrap();
        assert!(p.ready);
        assert_eq!(p.count, 2);
        assert_eq!(p.logical_bytes, 27);
        let (applied, locations, inodes) = apply(&db, &r).unwrap();
        assert_eq!(applied.trashed, 2);
        assert!(applied.dirty);
        assert_eq!(locations.len(), 2);
        assert_eq!(inodes.len(), 2);
        let after = drive_snapshot(&db).unwrap();
        assert!(after.files.is_empty());
        assert!(after.revision > r.revision);
        let mut check = db
            .prepare("SELECT COUNT(*) FROM drive_file_entries WHERE trashed_at IS NOT NULL")
            .unwrap();
        assert_eq!(check.next().unwrap(), sqlite::State::Row);
        assert_eq!(check.read::<i64, _>(0).unwrap(), 2);
    }
    #[test]
    fn second_trash_trigger_failure_rolls_back_first_file_and_revision() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_trash_abort BEFORE UPDATE OF trashed_at ON drive_file_entries
             WHEN NEW.entry_id='file2' AND NEW.trashed_at IS NOT NULL
             BEGIN SELECT RAISE(ABORT,'synthetic second failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.files.len(), 2);
        assert_eq!(after.revision, r.revision);
    }
    #[test]
    fn ignored_trash_update_cannot_claim_success() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_trash_ignore BEFORE UPDATE OF trashed_at ON drive_file_entries
             WHEN NEW.entry_id='file2' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().files.len(), 2);
    }
    #[test]
    fn stale_revision_moved_source_and_duplicate_id_are_rejected() {
        let db = fixture();
        let mut r = request(&db);
        r.revision = -1;
        assert!(apply(&db, &r).is_err());
        r = request(&db);
        r.files[0].expected_parent_id = None;
        assert!(!preview(&db, &r).unwrap().ready);
        r = request(&db);
        r.files[1].entry_id = "file1".into();
        assert!(!preview(&db, &r).unwrap().ready);
        assert_eq!(drive_snapshot(&db).unwrap().files.len(), 2);
    }
    #[test]
    fn pending_same_path_and_copy_on_write_base_are_rejected() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_pending_files
               (id,directory_id,display_name,staging_path,size,format_version,
                backing_channel_id,base_entry_id,created_at,updated_at)
             VALUES('stage','docs','report.txt','never-open',2,1,47,NULL,1,1)",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
        db.execute(
            "UPDATE drive_pending_files
             SET directory_id=NULL,display_name='elsewhere',base_entry_id='file2'
             WHERE id='stage'",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }
    #[test]
    fn authoritative_v2_is_selected_and_old_legacy_shadow_is_retired() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_objects
              (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
             VALUES('v2obj',12,0,NULL,'v2fingerprint',0,1);
             INSERT INTO drive_entries_v2
              (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
             VALUES('file1','v2obj',NULL,'docs','newer.txt',1,2,NULL);",
        ).unwrap();
        let mut r = request(&db);
        r.files = vec![BatchTrashFile {
            entry_id: "file1".into(),
            expected_name: "newer.txt".into(),
            expected_parent_id: Some("docs".into()),
            expected_storage_version: 2,
        }];
        assert!(preview(&db, &r).unwrap().ready);
        apply(&db, &r).unwrap();
        let after = drive_snapshot(&db).unwrap();
        assert!(!after.files.iter().any(|f| f.entry_id == "file1"));
    }
}
