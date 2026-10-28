//! Atomic exact-location recovery for Drive Recently Deleted records.
//! Never overwrites, silently renames, or partially commits a selection.
//! Shares the native single-record restore rules inside one SQLite SAVEPOINT.
use crate::commands::drive_metadata::{
    drive_snapshot, list_drive_trash, portable_name_key, restore_drive_trash_item,
    validate_drive_name, DriveSnapshot, DriveTrashItem,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_BATCH_RESTORE: usize = 100;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRestoreItem {
    kind: String,
    id: String,
    expected_name: String,
    expected_parent_id: Option<String>,
    expected_deleted_at: i64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRestoreRequest {
    revision: i64,
    items: Vec<BatchRestoreItem>,
}
#[derive(Clone, Debug, Serialize)]
pub struct BatchRestorePreview {
    revision: i64,
    ready: bool,
    count: usize,
    restorable_bytes: u64,
    planned: Vec<BatchRestorePlanned>,
    problems: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct BatchRestorePlanned {
    kind: String,
    path: String,
    size: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct BatchRestoreApplied {
    restored: usize,
    revision: i64,
    dirty: bool,
}
type Namespace = (Option<String>, String);
type Commit = (BatchRestoreApplied, Vec<Namespace>, Vec<(String, String)>);

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn key(parent: &Option<String>, name: &str) -> (Option<String>, String) {
    (parent.clone(), portable_name_key(name))
}
fn add_occupant(
    occupied: &mut HashSet<(Option<String>, String)>,
    parent: &Option<String>,
    name: &str,
) {
    occupied.insert(key(parent, name));
}
/// Reconstruct only active, unambiguous original ancestry. Never guess a path
/// when a parent is deleted, cyclic, malformed or outside the Drive namespace.
fn original_path(snapshot: &DriveSnapshot, item: &DriveTrashItem) -> Result<String, String> {
    let index: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut segments = vec![item.name.clone()];
    let mut visited = HashSet::new();
    let mut parent = item.original_directory_id.as_deref();
    while let Some(id) = parent {
        if !safe_id(id) || !visited.insert(id) || visited.len() > 100 {
            return Err("original directory ancestry is cyclic or too deep".into());
        }
        let directory = index
            .get(id)
            .ok_or("original parent is not an active Drive folder")?;
        if validate_drive_name(&directory.name).is_err() {
            return Err("original parent has an unsafe name".into());
        }
        segments.push(directory.name.clone());
        parent = directory.parent_id.as_deref();
    }
    segments.reverse();
    let path = format!("/{}", segments.join("/"));
    if path.len() > 4096 {
        return Err("original Drive path is too long".into());
    }
    Ok(path)
}

fn checked_rows(
    snapshot: &DriveSnapshot,
    trash: &[DriveTrashItem],
    request: &BatchRestoreRequest,
) -> Result<Vec<DriveTrashItem>, Vec<String>> {
    if snapshot.revision != request.revision {
        return Err(vec![
            "Drive changed since selection. Refresh and preview again.".into(),
        ]);
    }
    if request.items.is_empty() || request.items.len() > MAX_BATCH_RESTORE {
        return Err(vec!["Select 1–100 recently deleted Drive entries.".into()]);
    }
    let mut candidates = HashMap::<(String, String), Option<&DriveTrashItem>>::new();
    for entry in trash {
        candidates
            .entry((entry.kind.clone(), entry.id.clone()))
            .and_modify(|p| *p = None) // Duplicate kind/identity is unsafe.
            .or_insert(Some(entry));
    }
    let active_dirs: HashSet<_> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| d.id.as_str())
        .collect();
    let mut occupied = HashSet::new();
    for d in snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
    {
        add_occupant(&mut occupied, &d.parent_id, &d.name);
    }
    for file in &snapshot.files {
        add_occupant(&mut occupied, &file.directory_id, &file.display_name);
    }
    for pending in &snapshot.pending {
        add_occupant(&mut occupied, &pending.directory_id, &pending.display_name);
    }
    for symlink in snapshot.symlinks.iter().filter(|l| l.trashed_at.is_none()) {
        add_occupant(&mut occupied, &symlink.directory_id, &symlink.display_name);
    }
    let mut problems = Vec::new();
    let mut selected = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut planned_targets = HashSet::new();
    for (index, item) in request.items.iter().enumerate() {
        let line = index + 1;
        if !["file", "folder", "symlink"].contains(&item.kind.as_str())
            || !safe_id(&item.id)
            || !seen_ids.insert((item.kind.clone(), item.id.clone()))
        {
            problems.push(format!(
                "Row {line}: invalid or duplicate trash item identity."
            ));
            continue;
        }
        if validate_drive_name(&item.expected_name).is_err() {
            problems.push(format!("Row {line}: unsafe original Drive name."));
            continue;
        }
        let Some(Some(current)) = candidates.get(&(item.kind.clone(), item.id.clone())) else {
            problems.push(format!("Row {line}: trash item missing or ambiguous."));
            continue;
        };
        if !current.restorable
            || current.name != item.expected_name
            || current.original_directory_id != item.expected_parent_id
            || current.deleted_at != item.expected_deleted_at
        {
            problems.push(format!(
                "Row {line}: trash item changed or its content is unavailable."
            ));
            continue;
        }
        if item
            .expected_parent_id
            .as_deref()
            .is_some_and(|id| !safe_id(id) || !active_dirs.contains(id))
        {
            problems.push(format!("Row {line}: original parent is missing or still trashed; restore the parent first."));
            continue;
        }
        if snapshot
            .pending
            .iter()
            .any(|p| p.base_entry_id.as_deref() == Some(item.id.as_str()))
        {
            problems.push(format!(
                "Row {line}: a pending write references this file identity."
            ));
            continue;
        }
        if item.kind == "file" && snapshot.files.iter().any(|f| f.entry_id == item.id) {
            problems.push(format!(
                "Row {line}: file identity still exists as an active entry."
            ));
            continue;
        }
        if item.kind == "folder"
            && snapshot
                .directories
                .iter()
                .any(|d| d.id == item.id && d.trashed_at.is_none())
        {
            problems.push(format!("Row {line}: folder identity already exists."));
            continue;
        }
        if item.kind == "symlink"
            && snapshot
                .symlinks
                .iter()
                .any(|l| l.id == item.id && l.trashed_at.is_none())
        {
            problems.push(format!("Row {line}: link identity already exists."));
            continue;
        }
        if let Err(reason) = original_path(snapshot, current) {
            problems.push(format!("Row {line}: {reason}."));
            continue;
        }
        let target = key(&item.expected_parent_id, &item.expected_name);
        if !planned_targets.insert(target.clone()) || occupied.contains(&target) {
            problems.push(format!(
                "Row {line}: original path conflicts with an existing or selected item."
            ));
            continue;
        }
        selected.push((**current).clone());
    }
    if problems.is_empty() {
        Ok(selected)
    } else {
        problems.truncate(100);
        Err(problems)
    }
}
fn preview(
    conn: &sqlite::Connection,
    request: &BatchRestoreRequest,
) -> Result<BatchRestorePreview, String> {
    let snapshot = drive_snapshot(conn)?;
    let trash = list_drive_trash(conn)?;
    let checked = checked_rows(&snapshot, &trash, request);
    let (problems, bytes, planned) = match checked {
        Ok(rows) => {
            let total = rows
                .iter()
                .filter(|r| r.kind == "file")
                .try_fold(0u64, |sum, r| sum.checked_add(r.size.unwrap_or(0)));
            match total {
                Some(bytes) => {
                    let plan = rows
                        .iter()
                        .map(|row| {
                            original_path(&snapshot, row).map(|path| BatchRestorePlanned {
                                kind: row.kind.clone(),
                                path,
                                size: row.size,
                            })
                        })
                        .collect::<Result<Vec<_>, _>>();
                    match plan {
                        Ok(planned) => (Vec::new(), bytes, planned),
                        Err(err) => (vec![err], 0, Vec::new()),
                    }
                }
                None => (
                    vec!["Selected restorable file sizes overflow.".into()],
                    0,
                    Vec::new(),
                ),
            }
        }
        Err(p) => (p, 0, Vec::new()),
    };
    Ok(BatchRestorePreview {
        revision: snapshot.revision,
        ready: problems.is_empty(),
        count: request.items.len(),
        restorable_bytes: bytes,
        planned,
        problems,
    })
}
fn apply(conn: &sqlite::Connection, request: &BatchRestoreRequest) -> Result<Commit, String> {
    let before = drive_snapshot(conn)?;
    let trash = list_drive_trash(conn)?;
    let selected = checked_rows(&before, &trash, request).map_err(|p| p.join(" "))?;
    conn.execute("SAVEPOINT drive_batch_exact_restore")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut names = Vec::new();
        let mut inodes = Vec::new();
        for entry in &selected {
            let name = restore_drive_trash_item(conn, &entry.kind, &entry.id)?;
            // The existing single-item restore may automatically append a suffix
            // to avoid collisions. A batch is stricter: roll back ALL rows instead.
            if name != entry.name {
                return Err(
                    "Original path changed or a name conflict appeared during restore.".into(),
                );
            }
            names.push((entry.original_directory_id.clone(), entry.name.clone()));
            inodes.push((entry.kind.clone(), entry.id.clone()));
        }
        let after = drive_snapshot(conn)?;
        let still_deleted = list_drive_trash(conn)?;
        for entry in &selected {
            if still_deleted
                .iter()
                .any(|t| t.kind == entry.kind && t.id == entry.id)
            {
                return Err("A restored item remained in Recently Deleted.".into());
            }
            let matches = match entry.kind.as_str() {
                "file" => {
                    // Old V1 fallback entries may remain alongside a restored
                    // authoritative V2 entry. Count only the highest storage
                    // generation for this identity; never reject a valid V2
                    // recovery because its historical fallback is also visible.
                    let canonical = after
                        .files
                        .iter()
                        .filter(|f| f.entry_id == entry.id)
                        .map(|f| f.storage_version)
                        .max()
                        .unwrap_or(0);
                    after
                        .files
                        .iter()
                        .filter(|f| {
                            f.entry_id == entry.id
                                && f.storage_version == canonical
                                && f.display_name == entry.name
                                && f.directory_id == entry.original_directory_id
                        })
                        .count()
                }
                "folder" => after
                    .directories
                    .iter()
                    .filter(|d| {
                        d.id == entry.id
                            && d.name == entry.name
                            && d.parent_id == entry.original_directory_id
                            && d.trashed_at.is_none()
                    })
                    .count(),
                "symlink" => after
                    .symlinks
                    .iter()
                    .filter(|l| {
                        l.id == entry.id
                            && l.display_name == entry.name
                            && l.directory_id == entry.original_directory_id
                            && l.trashed_at.is_none()
                    })
                    .count(),
                _ => 0,
            };
            if matches != 1 {
                return Err(
                    "A restored entry identity or location did not match the requested path."
                        .into(),
                );
            }
        }
        if !after.dirty || after.revision <= before.revision {
            return Err("Drive restore did not advance local metadata revision.".into());
        }
        Ok((
            BatchRestoreApplied {
                restored: selected.len(),
                revision: after.revision,
                dirty: after.dirty,
            },
            names,
            inodes,
        ))
    })();
    match result {
        Ok(value) => {
            if let Err(e) = conn.execute("RELEASE drive_batch_exact_restore") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_batch_exact_restore; RELEASE drive_batch_exact_restore",
                );
                return Err(e.to_string());
            }
            Ok(value)
        }
        Err(e) => {
            conn.execute(
                "ROLLBACK TO drive_batch_exact_restore; RELEASE drive_batch_exact_restore",
            )
            .map_err(|rollback| format!("{e}; rollback failed: {rollback}"))?;
            Err(e)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_batch_restore_preview(
    db: State<'_, DbConnection>,
    request: BatchRestoreRequest,
) -> Result<BatchRestorePreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_batch_restore_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: BatchRestoreRequest,
) -> Result<BatchRestoreApplied, String> {
    // The mounted Linux restore path has a separate FUSE/kernel handshake.
    // A local batch must not bypass it while an active mount owns the namespace.
    #[cfg(target_os = "linux")]
    let _mount_exclusion = app
        .try_state::<crate::drive_mount::DriveMountState>()
        .ok_or("Drive mount safety state unavailable; batch restore refused")?
        .acquire_unmounted_batch_restore()?;
    // Keep the mount-exclusion guard alive through the complete DB commit.
    // A simultaneous mount startup cannot observe a partly restored tree.
    let (done, _, _) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Batch restore committed, Windows provider refresh pending: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Batch restore committed, macOS provider refresh pending: {e}");
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = &app;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
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
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES ('existing',NULL,'Existing',1,1);
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES ('source',500,'QA','owner',1,1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
               total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('remote1','source',2,'alpha.txt',11,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
               total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('remote2','source',3,'beta.txt',12,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,
               created_at,updated_at,trashed_at)
             VALUES ('restore-a','remote1','existing','alpha.txt',1,5,10);
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,
               created_at,updated_at,trashed_at)
             VALUES ('restore-b','remote2','existing','beta.txt',1,5,11);",
        ).unwrap();
        conn
    }
    fn request(conn: &sqlite::Connection) -> BatchRestoreRequest {
        let revision = drive_snapshot(conn).unwrap().revision;
        let items = list_drive_trash(conn)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "file")
            .map(|item| BatchRestoreItem {
                kind: item.kind,
                id: item.id,
                expected_name: item.name,
                expected_parent_id: item.original_directory_id,
                expected_deleted_at: item.deleted_at,
            })
            .collect();
        BatchRestoreRequest { revision, items }
    }

    #[test]
    fn exact_two_file_restore_commits_both_and_preserves_locations() {
        let conn = fixture();
        let request = request(&conn);
        let checked = preview(&conn, &request).unwrap();
        assert!(checked.ready);
        assert_eq!(checked.count, 2);
        assert_eq!(checked.restorable_bytes, 23);
        assert_eq!(checked.planned.len(), 2);
        assert!(checked
            .planned
            .iter()
            .any(|row| row.path == "/Existing/alpha.txt"));
        assert!(checked
            .planned
            .iter()
            .any(|row| row.path == "/Existing/beta.txt"));
        let (applied, names, inodes) = apply(&conn, &request).unwrap();
        assert_eq!(applied.restored, 2);
        assert_eq!(names.len(), 2);
        assert_eq!(inodes.len(), 2);
        let after = drive_snapshot(&conn).unwrap();
        assert!(after.revision > request.revision);
        assert_eq!(after.files.len(), 2);
        assert!(after
            .files
            .iter()
            .all(|f| f.directory_id.as_deref() == Some("existing")));
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 0);
    }
    #[test]
    fn second_item_failure_rolls_back_first_restore() {
        let conn = fixture();
        let request = request(&conn);
        let later = request.items.last().unwrap().id.as_str();
        let statement = format!(
            "CREATE TRIGGER qa_restore_abort BEFORE UPDATE ON drive_file_entries
            WHEN NEW.entry_id='{later}' AND NEW.trashed_at IS NULL
            BEGIN SELECT RAISE(ABORT,'synthetic restore failure'); END;"
        );
        conn.execute(statement).unwrap();
        assert!(apply(&conn, &request).is_err());
        let after = drive_snapshot(&conn).unwrap();
        assert!(after.files.is_empty());
        assert_eq!(after.revision, request.revision);
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 2);
    }
    #[test]
    fn destination_collision_fails_without_partial_restore() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,
            created_at,updated_at) VALUES ('occupied','remote1','existing','alpha.txt',1,1)",
        )
        .unwrap();
        let req = request(&conn);
        assert!(!preview(&conn, &req).unwrap().ready);
        assert!(apply(&conn, &req).is_err());
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 2);
    }
    #[test]
    fn stale_revision_and_duplicate_selection_fail_closed() {
        let conn = fixture();
        let mut req = request(&conn);
        req.revision = -1;
        assert!(!preview(&conn, &req).unwrap().ready);
        req = request(&conn);
        req.items.push(req.items[0].clone());
        assert!(!preview(&conn, &req).unwrap().ready);
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 2);
    }
    #[test]
    fn cannot_recover_into_original_parent_still_in_trash() {
        let conn = fixture();
        conn.execute("UPDATE drive_directories SET trashed_at=123 WHERE id='existing'")
            .unwrap();
        let req = request(&conn);
        assert!(!preview(&conn, &req).unwrap().ready);
        assert!(apply(&conn, &req).is_err());
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 3);
    }
    #[test]
    fn authoritative_v2_restore_with_legacy_fallback_keeps_canonical_identity() {
        let conn = fixture();
        // A historical legacy entry may still exist while a V2 tombstone
        // owns the authoritative identity. Restoring V2 must not count the
        // legacy fallback as a second recovered file.
        conn.execute(
            "UPDATE drive_file_entries SET trashed_at=NULL
            WHERE entry_id='restore-a'",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO drive_objects
            (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
            VALUES ('object-v2',11,0,NULL,'sha-v2',0,1);
            INSERT INTO drive_entries_v2
            (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
            VALUES ('restore-a','object-v2',NULL,'existing','alpha.txt',1,6,10);"
        ).unwrap();
        let req = request(&conn);
        assert_eq!(req.items.len(), 2);
        assert!(preview(&conn, &req).unwrap().ready);
        assert!(apply(&conn, &req).is_ok());
        let after = drive_snapshot(&conn).unwrap();
        assert!(after.files.iter().any(|f| f.entry_id == "restore-a"
            && f.storage_version == 2
            && f.file_id == "object-v2"
            && f.directory_id.as_deref() == Some("existing")));
    }
    #[test]
    fn rollback_on_ignored_restore_update() {
        let conn = fixture();
        let request = request(&conn);
        conn.execute(
            "CREATE TRIGGER qa_restore_ignore BEFORE UPDATE ON drive_file_entries
            WHEN NEW.entry_id='restore-a' AND NEW.trashed_at IS NULL
            BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&conn, &request).is_err());
        assert!(drive_snapshot(&conn).unwrap().files.is_empty());
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 2);
    }
}
