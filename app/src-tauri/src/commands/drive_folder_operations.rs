//! Atomic, preview-gated folder organization. Entire folder subtrees follow their
//! directory IDs; only metadata moves and the Drive dirty revision are changed.
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, rename_drive_directory, validate_drive_name, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_FOLDER_BATCH: usize = 100;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderMove {
    pub folder_id: String,
    pub expected_name: String,
    pub expected_parent_id: Option<String>,
    pub new_name: String,
    pub new_parent_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderPlan {
    pub revision: i64,
    pub changes: Vec<FolderMove>,
}
#[derive(Debug, Serialize)]
pub struct FolderPreview {
    pub revision: i64,
    pub count: usize,
    pub ready: bool,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct FolderApplied {
    pub changed: usize,
    pub revision: i64,
    pub dirty: bool,
}

fn namespace_key(parent: &Option<String>, name: &str) -> (Option<String>, String) {
    (parent.clone(), portable_name_key(name))
}
fn insert_occupant(
    occupied: &mut HashMap<(Option<String>, String), Option<String>>,
    parent: &Option<String>,
    name: &str,
    identity: String,
) {
    occupied
        .entry(namespace_key(parent, name))
        .and_modify(|previous| *previous = None)
        .or_insert(Some(identity));
}
fn is_ancestor(
    ancestor: &str,
    child: &str,
    folders: &HashMap<&str, &crate::commands::drive_metadata::DriveDirectoryRecord>,
) -> bool {
    let mut cursor = Some(child);
    let mut visited = HashSet::new();
    while let Some(id) = cursor {
        if id == ancestor {
            return true;
        }
        if !visited.insert(id) {
            // Fail closed for cyclic/malformed ancestry.
            return true;
        }
        cursor = match folders.get(id) {
            Some(folder) => folder.parent_id.as_deref(),
            None => return true,
        };
    }
    false
}
pub(super) fn validate(snapshot: &DriveSnapshot, plan: &FolderPlan) -> Vec<String> {
    let mut errors = Vec::new();
    if plan.revision != snapshot.revision {
        return vec!["Drive changed since the folder preview. Refresh the plan.".into()];
    }
    if plan.changes.is_empty() || plan.changes.len() > MAX_FOLDER_BATCH {
        return vec![format!(
            "Select 1–{MAX_FOLDER_BATCH} distinct Drive folders."
        )];
    }
    let folders: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut occupants = HashMap::new();
    for folder in folders.values() {
        insert_occupant(
            &mut occupants,
            &folder.parent_id,
            &folder.name,
            folder.id.clone(),
        );
    }
    for file in &snapshot.files {
        insert_occupant(
            &mut occupants,
            &file.directory_id,
            &file.display_name,
            format!("file:{}", file.entry_id),
        );
    }
    for pending in &snapshot.pending {
        insert_occupant(
            &mut occupants,
            &pending.directory_id,
            &pending.display_name,
            format!("pending:{}", pending.id),
        );
    }
    for symlink in snapshot.symlinks.iter().filter(|s| s.trashed_at.is_none()) {
        insert_occupant(
            &mut occupants,
            &symlink.directory_id,
            &symlink.display_name,
            format!("symlink:{}", symlink.id),
        );
    }
    let mut selected = HashSet::new();
    let mut targets = HashSet::new();
    for (n, change) in plan.changes.iter().enumerate() {
        let row = n + 1;
        if change.folder_id.is_empty()
            || change.folder_id.len() > 256
            || !selected.insert(change.folder_id.as_str())
        {
            errors.push(format!("Row {row}: duplicate or invalid folder identity."));
            continue;
        }
        let Some(source) = folders.get(change.folder_id.as_str()) else {
            errors.push(format!("Row {row}: source folder no longer exists."));
            continue;
        };
        if source.name != change.expected_name || source.parent_id != change.expected_parent_id {
            errors.push(format!(
                "Row {row}: source name or parent changed; refresh."
            ));
            continue;
        }
        if let Err(problem) = validate_drive_name(&change.new_name) {
            errors.push(format!("Row {row}: {problem}."));
            continue;
        }
        if change.new_name == source.name && change.new_parent_id == source.parent_id {
            errors.push(format!("Row {row}: no folder change was specified."));
            continue;
        }
        if let Some(target) = change.new_parent_id.as_deref() {
            if !folders.contains_key(target) {
                errors.push(format!(
                    "Row {row}: destination folder is missing or trashed."
                ));
                continue;
            }
            if is_ancestor(&change.folder_id, target, &folders) {
                errors.push(format!(
                    "Row {row}: a folder cannot move into its own subtree."
                ));
                continue;
            }
        }
        let key = namespace_key(&change.new_parent_id, &change.new_name);
        if !targets.insert(key.clone()) {
            errors.push(format!(
                "Row {row}: another row targets the same name and parent."
            ));
            continue;
        }
        if let Some(existing) = occupants.get(&key) {
            // Swapping names with another row is intentionally not implicit.
            if existing.as_deref() != Some(change.folder_id.as_str()) {
                errors.push(format!("Row {row}: destination name is already occupied."));
            }
        }
    }
    // Reject overlapping ancestor/descendant plans. The folder subtree itself
    // still moves, but simultaneous parent and child changes would be ambiguous.
    for (i, a) in plan.changes.iter().enumerate() {
        for b in plan.changes.iter().skip(i + 1) {
            if is_ancestor(&a.folder_id, &b.folder_id, &folders)
                || is_ancestor(&b.folder_id, &a.folder_id, &folders)
            {
                errors.push(
                    "Select folders from separate subtrees; nested folder batches are unsafe."
                        .into(),
                );
                break;
            }
        }
        // A selected folder cannot also be another row's destination.
        if plan.changes.iter().any(|other| {
            other.folder_id != a.folder_id
                && other.new_parent_id.as_deref() == Some(a.folder_id.as_str())
        }) {
            errors.push("A selected folder cannot also be a destination for another row.".into());
        }
    }
    errors
}
fn preview(conn: &sqlite::Connection, plan: &FolderPlan) -> Result<FolderPreview, String> {
    let snapshot = drive_snapshot(conn)?;
    let problems = validate(&snapshot, plan);
    Ok(FolderPreview {
        revision: snapshot.revision,
        count: plan.changes.len(),
        ready: problems.is_empty(),
        problems,
    })
}
pub(super) fn apply(conn: &sqlite::Connection, plan: &FolderPlan) -> Result<FolderApplied, String> {
    let check = preview(conn, plan)?;
    if !check.ready {
        return Err(check.problems.join(" "));
    }
    conn.execute("SAVEPOINT drive_folder_batch")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        for change in &plan.changes {
            rename_drive_directory(
                conn,
                &change.folder_id,
                change.new_parent_id.as_deref(),
                &change.new_name,
            )?;
        }
        let snapshot = drive_snapshot(conn)?;
        // Triggers can silently RAISE(IGNORE) an UPDATE. Verify every requested
        // postcondition inside the SAME outer savepoint before reporting success.
        let by_id: HashMap<_, _> = snapshot
            .directories
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        if !snapshot.dirty || snapshot.revision <= plan.revision {
            return Err("Drive folder revision did not advance; rolling back batch".into());
        }
        for change in &plan.changes {
            let actual = by_id.get(change.folder_id.as_str());
            if !matches!(actual, Some(folder)
                if folder.trashed_at.is_none()
                && folder.name == change.new_name
                && folder.parent_id == change.new_parent_id)
            {
                return Err(
                    "A folder update was ignored or changed during apply; whole batch rolled back"
                        .into(),
                );
            }
        }
        // Journal is part of the SAME outer savepoint. A history write failure
        // aborts the original user-requested folder movement too.
        crate::commands::drive_folder_undo::record_applied(conn, plan, &snapshot)?;
        Ok(FolderApplied {
            changed: plan.changes.len(),
            revision: snapshot.revision,
            dirty: snapshot.dirty,
        })
    })();
    match result {
        Ok(done) => {
            if let Err(error) = conn.execute("RELEASE drive_folder_batch") {
                let _ = conn.execute("ROLLBACK TO drive_folder_batch; RELEASE drive_folder_batch");
                return Err(error.to_string());
            }
            Ok(done)
        }
        Err(problem) => {
            conn.execute("ROLLBACK TO drive_folder_batch; RELEASE drive_folder_batch")
                .map_err(|e| format!("{problem}; failed to roll back folder batch: {e}"))?;
            Err(problem)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_folder_preview(
    db_pool: State<'_, DbConnection>,
    plan: FolderPlan,
) -> Result<FolderPreview, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Drive database unavailable".to_owned())?;
    preview(&conn, &plan)
}
#[tauri::command]
pub fn cmd_drive_folder_apply(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
    plan: FolderPlan,
) -> Result<FolderApplied, String> {
    // Keep SQLite locked through revalidation and commit, but NEVER while
    // notifying FUSE, which can call back into the database.
    let applied = {
        let conn = db_pool
            .lock()
            .map_err(|_| "Drive database unavailable".to_owned())?;
        apply(&conn, &plan)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        let mut paths = HashSet::new();
        let mut inodes = HashSet::new();
        for change in &plan.changes {
            paths.insert((
                change.expected_parent_id.clone(),
                change.expected_name.clone(),
            ));
            paths.insert((change.new_parent_id.clone(), change.new_name.clone()));
            inodes.insert(("dir".to_owned(), change.folder_id.clone()));
        }
        mount.notify_namespace_changed(
            &paths.into_iter().collect::<Vec<_>>(),
            &inodes.into_iter().collect::<Vec<_>>(),
        );
    }
    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Folder batch committed, but Windows refresh failed: {error}");
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Folder batch committed, but macOS refresh failed: {error}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = app_handle;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        let db = sqlite::open(":memory:").unwrap();
        db.execute("CREATE TABLE logical_channels(logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER, name TEXT, role TEXT, storage_version INTEGER, created_at INTEGER, joined_at INTEGER);\n            CREATE TABLE logical_files(file_id TEXT PRIMARY KEY, logical_channel_id TEXT NOT NULL, first_message_id INTEGER NOT NULL, display_name TEXT NOT NULL, total_size INTEGER NOT NULL, mime_type TEXT, chunk_count INTEGER NOT NULL DEFAULT 1, whole_sha256 TEXT, storage_version INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');").unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES
             ('a',NULL,'Alpha',1,1),
             ('a-child','a','child',1,1),
             ('b',NULL,'Beta',1,1),
             ('c',NULL,'Gamma',1,1);",
        )
        .unwrap();
        db
    }
    fn row(
        id: &str,
        expected: &str,
        parent: Option<&str>,
        name: &str,
        destination: Option<&str>,
    ) -> FolderMove {
        FolderMove {
            folder_id: id.into(),
            expected_name: expected.into(),
            expected_parent_id: parent.map(str::to_owned),
            new_name: name.into(),
            new_parent_id: destination.map(str::to_owned),
        }
    }
    fn plan(db: &sqlite::Connection, changes: Vec<FolderMove>) -> FolderPlan {
        FolderPlan {
            revision: drive_snapshot(db).unwrap().revision,
            changes,
        }
    }
    #[test]
    fn folder_subtree_moves_atomically_without_changing_child_parent_ids() {
        let db = fixture();
        let p = plan(&db, vec![row("a", "Alpha", None, "Moved", Some("b"))]);
        assert!(preview(&db, &p).unwrap().ready);
        let result = apply(&db, &p).unwrap();
        assert!(result.dirty && result.revision > p.revision);
        let snap = drive_snapshot(&db).unwrap();
        let a = snap.directories.iter().find(|d| d.id == "a").unwrap();
        let child = snap.directories.iter().find(|d| d.id == "a-child").unwrap();
        assert_eq!(a.parent_id.as_deref(), Some("b"));
        assert_eq!(child.parent_id.as_deref(), Some("a"));
    }
    #[test]
    fn rejects_descendant_cycle_and_nested_batches() {
        let db = fixture();
        let cycle = plan(&db, vec![row("a", "Alpha", None, "Alpha", Some("a-child"))]);
        assert!(!preview(&db, &cycle).unwrap().ready);
        let nested = plan(
            &db,
            vec![
                row("a", "Alpha", None, "NewAlpha", None),
                row("a-child", "child", Some("a"), "newchild", Some("a")),
            ],
        );
        assert!(!preview(&db, &nested).unwrap().ready);
    }
    #[test]
    fn rejects_occupied_destination_and_stale_revision() {
        let db = fixture();
        let conflict = plan(&db, vec![row("a", "Alpha", None, "Beta", None)]);
        assert!(!preview(&db, &conflict).unwrap().ready);
        let stale = FolderPlan {
            revision: -1,
            ..conflict
        };
        assert!(!preview(&db, &stale).unwrap().ready);
    }
    #[test]
    fn rejects_case_insensitive_duplicate_targets() {
        let db = fixture();
        let p = plan(
            &db,
            vec![
                row("a", "Alpha", None, "Work", Some("b")),
                row("c", "Gamma", None, "work", Some("b")),
            ],
        );
        assert!(!preview(&db, &p).unwrap().ready);
    }
    #[test]
    fn later_failure_rolls_back_earlier_folder_rename() {
        let db = fixture();
        let p = plan(
            &db,
            vec![
                row("a", "Alpha", None, "MovedA", None),
                row("c", "Gamma", None, "MovedC", None),
            ],
        );
        assert!(preview(&db, &p).unwrap().ready);
        db.execute("CREATE TRIGGER qa_abort_folder BEFORE UPDATE ON drive_directories WHEN OLD.id='c' BEGIN SELECT RAISE(ABORT, 'qa denied'); END;").unwrap();
        assert!(apply(&db, &p).is_err());
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.revision, p.revision);
        assert!(snap
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Alpha"));
    }
    #[test]
    fn ignored_sql_update_rolls_back_whole_batch() {
        let db = fixture();
        let p = plan(
            &db,
            vec![
                row("a", "Alpha", None, "MovedA", None),
                row("c", "Gamma", None, "MovedC", None),
            ],
        );
        assert!(preview(&db, &p).unwrap().ready);
        db.execute("CREATE TRIGGER qa_ignore_folder BEFORE UPDATE ON drive_directories WHEN OLD.id='c' BEGIN SELECT RAISE(IGNORE); END;").unwrap();
        assert!(apply(&db, &p).is_err());
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.revision, p.revision);
        assert!(after
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Alpha"));
        assert!(after
            .directories
            .iter()
            .any(|d| d.id == "c" && d.name == "Gamma"));
    }
    #[test]
    fn rejects_stale_source_and_unsafe_names() {
        let db = fixture();
        let p = plan(&db, vec![row("a", "Wrong", None, "NUL", None)]);
        assert!(!preview(&db, &p).unwrap().ready);
        let bad = plan(&db, vec![row("a", "Alpha", None, "CON", None)]);
        assert!(!preview(&db, &bad).unwrap().ready);
    }
}
