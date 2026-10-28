//! Durable, fail-closed undo history for Drive FOLDER batch operations.
//! Only metadata is reversed. A changed subtree invalidates the undo.
use crate::commands::drive_folder_operations::{
    validate as validate_folder_moves, FolderMove, FolderPlan,
};
use crate::commands::drive_metadata::{
    drive_snapshot, rename_drive_directory, validate_drive_name, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_HISTORY: usize = 24;
const MAX_BATCH: usize = 100;
const MAX_JOURNAL_BYTES: usize = 250_000;
const MAX_SUBTREE_ENTRIES: usize = 25_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderUndoEntry {
    pub folder_id: String,
    pub previous_name: String,
    pub previous_parent_id: Option<String>,
    pub current_name: String,
    pub current_parent_id: Option<String>,
    pub subtree_sha256: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct FolderUndoBatch {
    pub id: String,
    pub created_at: i64,
    pub applied_revision: i64,
    pub undone_at: Option<i64>,
    pub entries: Vec<FolderUndoEntry>,
}
#[derive(Debug, Clone, Serialize)]
pub struct FolderUndoPreview {
    pub id: String,
    pub revision: i64,
    pub ready: bool,
    pub count: usize,
    pub problems: Vec<String>,
    pub entries: Vec<FolderUndoEntry>,
}
#[derive(Debug, Clone, Serialize)]
pub struct FolderUndoApplied {
    pub restored: usize,
    pub revision: i64,
    pub dirty: bool,
}
fn local_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_local_folder_undo (
            id TEXT PRIMARY KEY NOT NULL,
            created_at INTEGER NOT NULL,
            applied_revision INTEGER NOT NULL,
            entries_json TEXT NOT NULL,
            undone_at INTEGER
        );",
    )
    .map_err(|e| e.to_string())
}
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn parent_ok(id: &Option<String>) -> bool {
    id.as_deref().is_none_or(id_ok)
}
fn decode_entries(json: &str) -> Result<Vec<FolderUndoEntry>, String> {
    if json.len() > MAX_JOURNAL_BYTES {
        return Err("Saved folder undo history is oversized.".into());
    }
    let entries: Vec<FolderUndoEntry> =
        serde_json::from_str(json).map_err(|_| "Saved folder undo history is damaged.")?;
    if entries.is_empty() || entries.len() > MAX_BATCH {
        return Err("Saved folder undo history has an invalid batch size.".into());
    }
    let mut seen = HashSet::new();
    for e in &entries {
        if !id_ok(&e.folder_id)
            || !seen.insert(&e.folder_id)
            || !parent_ok(&e.previous_parent_id)
            || !parent_ok(&e.current_parent_id)
            || validate_drive_name(&e.previous_name).is_err()
            || validate_drive_name(&e.current_name).is_err()
            || (e.previous_name == e.current_name && e.previous_parent_id == e.current_parent_id)
            || e.subtree_sha256.len() != 64
            || !e.subtree_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Saved folder undo history contains invalid identities.".into());
        }
    }
    Ok(entries)
}
fn batch_id_ok(id: &str) -> bool {
    id.len() == 39 && id.starts_with("folder-") && id[7..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn row(conn: &sqlite::Connection, id: &str) -> Result<FolderUndoBatch, String> {
    if !batch_id_ok(id) {
        return Err("Invalid folder undo history identity.".into());
    }
    local_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT id,created_at,applied_revision,entries_json,undone_at
         FROM drive_local_folder_undo WHERE id=? LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if stmt.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Err("Saved folder undo batch no longer exists.".into());
    }
    let encoded: String = stmt.read("entries_json").map_err(|e| e.to_string())?;
    let undone: Option<i64> = stmt.read("undone_at").map_err(|e| e.to_string())?;
    Ok(FolderUndoBatch {
        id: stmt.read("id").map_err(|e| e.to_string())?,
        created_at: stmt.read("created_at").map_err(|e| e.to_string())?,
        applied_revision: stmt.read("applied_revision").map_err(|e| e.to_string())?,
        undone_at: undone,
        entries: decode_entries(&encoded)?,
    })
}
fn digest_subtree(snapshot: &DriveSnapshot, root_id: &str) -> Result<String, String> {
    // Index parent-child links ONCE. Scanning every directory at each level
    // becomes quadratic on a large mounted Drive and freezes undo previews.
    let mut folders = HashMap::new();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for folder in snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
    {
        if folders.insert(folder.id.as_str(), folder).is_some() {
            return Err("Drive contains duplicate folder identities; undo is unsafe.".into());
        }
        if let Some(parent) = folder.parent_id.as_deref() {
            children.entry(parent).or_default().push(&folder.id);
        }
    }
    if !folders.contains_key(root_id) {
        return Err("Moved folder is missing or trashed; undo is unsafe.".into());
    }
    // A defensive, bounded traversal. Missing/cyclic ancestry cannot silently
    // exclude files from the identity digest.
    let mut contained = HashSet::new();
    let mut queue = vec![root_id];
    while let Some(id) = queue.pop() {
        if !contained.insert(id) {
            return Err("Directory cycle or duplicate identity in undo subtree.".into());
        }
        if contained.len() > MAX_SUBTREE_ENTRIES {
            return Err("Folder tree is too large for safe undo verification.".into());
        }
        if let Some(next) = children.get(id) {
            queue.extend(next.iter().copied());
        }
    }
    let mut items: Vec<(String, Vec<u8>)> = Vec::new();
    for id in &contained {
        let folder = folders.get(id).ok_or("Missing folder in undo subtree")?;
        items.push((
            format!("d:{id}"),
            serde_json::to_vec(folder).map_err(|e| e.to_string())?,
        ));
    }
    for file in &snapshot.files {
        if file
            .directory_id
            .as_deref()
            .is_some_and(|id| contained.contains(id))
        {
            items.push((
                format!("f:{}", file.entry_id),
                serde_json::to_vec(file).map_err(|e| e.to_string())?,
            ));
        }
    }
    for pending in &snapshot.pending {
        if pending
            .directory_id
            .as_deref()
            .is_some_and(|id| contained.contains(id))
        {
            items.push((
                format!("p:{}", pending.id),
                serde_json::to_vec(pending).map_err(|e| e.to_string())?,
            ));
        }
    }
    for link in snapshot.symlinks.iter().filter(|s| s.trashed_at.is_none()) {
        if link
            .directory_id
            .as_deref()
            .is_some_and(|id| contained.contains(id))
        {
            items.push((
                format!("s:{}", link.id),
                serde_json::to_vec(link).map_err(|e| e.to_string())?,
            ));
        }
    }
    if items.len() > MAX_SUBTREE_ENTRIES {
        return Err("Folder tree contains too many files to verify safely.".into());
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    if items.windows(2).any(|p| p[0].0 == p[1].0) {
        return Err("Drive subtree has duplicate identities.".into());
    }
    let mut hash = Sha256::new();
    for (key, value) in items {
        hash.update((key.len() as u64).to_le_bytes());
        hash.update(key.as_bytes());
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    Ok(format!("{:x}", hash.finalize()))
}
/// Called INSIDE the original Folder Operations SAVEPOINT after all changes
/// have been checked. Failure aborts the entire move rather than losing undo.
pub(super) fn record_applied(
    conn: &sqlite::Connection,
    plan: &FolderPlan,
    snapshot: &DriveSnapshot,
) -> Result<(), String> {
    local_schema(conn)?;
    let entries = plan
        .changes
        .iter()
        .map(|change| {
            Ok(FolderUndoEntry {
                folder_id: change.folder_id.clone(),
                previous_name: change.expected_name.clone(),
                previous_parent_id: change.expected_parent_id.clone(),
                current_name: change.new_name.clone(),
                current_parent_id: change.new_parent_id.clone(),
                subtree_sha256: digest_subtree(snapshot, &change.folder_id)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let json = serde_json::to_string(&entries).map_err(|e| e.to_string())?;
    decode_entries(&json)?;
    let id = format!("folder-{:032x}", rand::random::<u128>());
    let now = chrono::Utc::now().timestamp_millis();
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_local_folder_undo (id,created_at,applied_revision,entries_json)
         VALUES (?,?,?,?) RETURNING id",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
    stmt.bind((2, now)).map_err(|e| e.to_string())?;
    stmt.bind((3, snapshot.revision))
        .map_err(|e| e.to_string())?;
    stmt.bind((4, json.as_str())).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Err("Folder undo journal write was ignored; entire batch cancelled.".into());
    }
    drop(stmt);
    conn.execute(
        "DELETE FROM drive_local_folder_undo WHERE id IN (
           SELECT id FROM drive_local_folder_undo
           ORDER BY created_at DESC,id DESC LIMIT -1 OFFSET 24
         );",
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
pub fn history(conn: &sqlite::Connection) -> Result<Vec<FolderUndoBatch>, String> {
    local_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT id FROM drive_local_folder_undo ORDER BY created_at DESC,id DESC LIMIT 25")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut ids = Vec::new();
    while stmt.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
        ids.push(stmt.read::<String, _>(0).map_err(|e| e.to_string())?);
    }
    if ids.len() > MAX_HISTORY {
        return Err("Folder undo history exceeds safe limit.".into());
    }
    drop(stmt);
    ids.iter().map(|id| row(conn, id)).collect()
}
fn plan_for_undo(snapshot: &DriveSnapshot, batch: &FolderUndoBatch) -> (FolderPlan, Vec<String>) {
    let mut errors = Vec::new();
    if batch.undone_at.is_some() {
        errors.push("This folder batch was already undone.".into());
    }
    if snapshot.revision < batch.applied_revision {
        errors.push("Drive revision predates saved folder history.".into());
    }
    for entry in &batch.entries {
        match digest_subtree(snapshot, &entry.folder_id) {
            Ok(now) if now == entry.subtree_sha256 => {}
            Ok(_) => errors.push(
                "A folder subtree changed since this operation. Undo would affect newer data."
                    .into(),
            ),
            Err(e) => errors.push(e),
        }
    }
    let plan = FolderPlan {
        revision: snapshot.revision,
        changes: batch
            .entries
            .iter()
            .map(|e| FolderMove {
                folder_id: e.folder_id.clone(),
                expected_name: e.current_name.clone(),
                expected_parent_id: e.current_parent_id.clone(),
                new_name: e.previous_name.clone(),
                new_parent_id: e.previous_parent_id.clone(),
            })
            .collect(),
    };
    errors.extend(validate_folder_moves(snapshot, &plan));
    (plan, errors)
}
pub fn preview(conn: &sqlite::Connection, id: &str) -> Result<FolderUndoPreview, String> {
    let saved = row(conn, id)?;
    let snapshot = drive_snapshot(conn)?;
    let (_, problems) = plan_for_undo(&snapshot, &saved);
    Ok(FolderUndoPreview {
        id: saved.id,
        revision: snapshot.revision,
        count: saved.entries.len(),
        ready: problems.is_empty(),
        problems,
        entries: saved.entries,
    })
}
pub fn apply(
    conn: &sqlite::Connection,
    id: &str,
    revision: i64,
) -> Result<(FolderUndoApplied, Vec<FolderMove>), String> {
    let saved = row(conn, id)?;
    let snapshot = drive_snapshot(conn)?;
    if snapshot.revision != revision {
        return Err("Drive changed since the undo preview; refresh before applying.".into());
    }
    let (plan, errors) = plan_for_undo(&snapshot, &saved);
    if !errors.is_empty() {
        return Err(errors.join(" "));
    }
    conn.execute("SAVEPOINT drive_folder_undo_apply")
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
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= revision {
            return Err("Undo revision did not advance; entire undo rolled back.".into());
        }
        let by_id: HashMap<_, _> = after
            .directories
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        for move_item in &plan.changes {
            let found = by_id.get(move_item.folder_id.as_str());
            if !matches!(found, Some(d)
                if d.trashed_at.is_none()
                && d.name == move_item.new_name
                && d.parent_id == move_item.new_parent_id)
            {
                return Err("Folder undo update was ignored; transaction rolled back.".into());
            }
        }
        let now = chrono::Utc::now().timestamp_millis();
        let mut update = conn.prepare(
            "UPDATE drive_local_folder_undo SET undone_at=? WHERE id=? AND undone_at IS NULL RETURNING id"
        ).map_err(|e:sqlite::Error|e.to_string())?;
        update.bind((1, now)).map_err(|e| e.to_string())?;
        update.bind((2, id)).map_err(|e| e.to_string())?;
        if update.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
            return Err(
                "Folder undo journal completion was ignored; transaction rolled back.".into(),
            );
        }
        Ok(FolderUndoApplied {
            restored: plan.changes.len(),
            revision: after.revision,
            dirty: after.dirty,
        })
    })();
    match result {
        Ok(done) => {
            if let Err(e) = conn.execute("RELEASE drive_folder_undo_apply") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_folder_undo_apply; RELEASE drive_folder_undo_apply",
                );
                return Err(e.to_string());
            }
            Ok((done, plan.changes))
        }
        Err(message) => {
            conn.execute("ROLLBACK TO drive_folder_undo_apply; RELEASE drive_folder_undo_apply")
                .map_err(|e| format!("{message}; rollback failed: {e}"))?;
            Err(message)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_folder_undo_history(
    db: State<'_, DbConnection>,
) -> Result<Vec<FolderUndoBatch>, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    history(&conn)
}
#[tauri::command]
pub fn cmd_drive_folder_undo_preview(
    db: State<'_, DbConnection>,
    id: String,
) -> Result<FolderUndoPreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&conn, &id)
}
#[tauri::command]
pub fn cmd_drive_folder_undo_apply(
    app_handle: AppHandle,
    db: State<'_, DbConnection>,
    id: String,
    revision: i64,
) -> Result<FolderUndoApplied, String> {
    let (result, changes) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&conn, &id, revision)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        let mut entries = HashSet::new();
        let mut inodes = HashSet::new();
        for change in &changes {
            entries.insert((
                change.expected_parent_id.clone(),
                change.expected_name.clone(),
            ));
            entries.insert((change.new_parent_id.clone(), change.new_name.clone()));
            inodes.insert(("dir".to_string(), change.folder_id.clone()));
        }
        mount.notify_namespace_changed(
            &entries.into_iter().collect::<Vec<_>>(),
            &inodes.into_iter().collect::<Vec<_>>(),
        );
    }
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Folder undo committed but Windows refresh unavailable: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Folder undo committed but macOS refresh unavailable: {e}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (&app_handle, &changes);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        let db = sqlite::open(":memory:").unwrap();
        db.execute("CREATE TABLE logical_channels(logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER,name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
        CREATE TABLE logical_files(file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'complete');").unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES
          ('a',NULL,'Alpha',1,1),('kid','a','Documents',1,1),
          ('b',NULL,'Target',1,1),('c',NULL,'Spare',1,1);",
        )
        .unwrap();
        db
    }
    fn moved(db: &sqlite::Connection) -> FolderPlan {
        FolderPlan {
            revision: drive_snapshot(db).unwrap().revision,
            changes: vec![FolderMove {
                folder_id: "a".into(),
                expected_name: "Alpha".into(),
                expected_parent_id: None,
                new_name: "Moved".into(),
                new_parent_id: Some("b".into()),
            }],
        }
    }
    fn create_history(db: &sqlite::Connection) -> String {
        let p = moved(db);
        // Uses the REAL Folder Ops apply path and writes journal in same transaction.
        crate::commands::drive_folder_operations::apply(db, &p).unwrap();
        history(db).unwrap()[0].id.clone()
    }
    #[test]
    fn ignored_history_insert_rolls_back_the_original_folder_move() {
        let db = fixture();
        assert!(history(&db).unwrap().is_empty()); // materialize the journal table
        let plan = moved(&db);
        db.execute(
            "CREATE TRIGGER qa_ignore_folder_history BEFORE INSERT ON drive_local_folder_undo
             BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(crate::commands::drive_folder_operations::apply(&db, &plan).is_err());
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.revision, plan.revision);
        assert!(after
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Alpha" && d.parent_id.is_none()));
        assert!(history(&db).unwrap().is_empty());
    }
    #[test]
    fn changed_subtree_child_metadata_prevents_undo() {
        let db = fixture();
        let id = create_history(&db);
        db.execute(
            "UPDATE drive_directories SET name='Changed Document Folder'
            WHERE id='kid';",
        )
        .unwrap();
        let blocked = preview(&db, &id).unwrap();
        assert!(!blocked.ready);
        assert!(blocked
            .problems
            .iter()
            .any(|s| s.contains("subtree changed")));
        assert!(apply(&db, &id, blocked.revision).is_err());
    }
    #[test]
    fn durable_history_and_atomic_subtree_undo() {
        let db = fixture();
        let id = create_history(&db);
        let list = history(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].entries[0].folder_id, "a");
        let check = preview(&db, &id).unwrap();
        assert!(check.ready, "{:?}", check.problems);
        let (applied, _) = apply(&db, &id, check.revision).unwrap();
        assert_eq!(applied.restored, 1);
        let state = drive_snapshot(&db).unwrap();
        assert!(state
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Alpha" && d.parent_id.is_none()));
        assert!(state
            .directories
            .iter()
            .any(|d| d.id == "kid" && d.parent_id.as_deref() == Some("a")));
        assert!(history(&db).unwrap()[0].undone_at.is_some());
        assert!(!preview(&db, &id).unwrap().ready);
    }
    #[test]
    fn newly_added_subtree_folder_prevents_undo() {
        let db = fixture();
        let id = create_history(&db);
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('late','a','Newly Added',2,2);",
        )
        .unwrap();
        assert!(!preview(&db, &id).unwrap().ready);
    }
    #[test]
    fn changed_folder_or_occupied_original_destination_prevents_undo() {
        let db = fixture();
        let id = create_history(&db);
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
            VALUES('conflict',NULL,'Alpha',2,2);",
        )
        .unwrap();
        assert!(!preview(&db, &id).unwrap().ready);
    }
    #[test]
    fn stale_preview_and_duplicate_undo_blocked() {
        let db = fixture();
        let id = create_history(&db);
        let before = preview(&db, &id).unwrap();
        assert!(apply(&db, &id, before.revision + 1).is_err());
        apply(&db, &id, before.revision).unwrap();
        assert!(apply(&db, &id, drive_snapshot(&db).unwrap().revision).is_err());
    }
    #[test]
    fn later_update_failure_rolls_back_earlier_undo_and_history() {
        let db = fixture();
        let plan = FolderPlan {
            revision: drive_snapshot(&db).unwrap().revision,
            changes: vec![
                FolderMove {
                    folder_id: "a".into(),
                    expected_name: "Alpha".into(),
                    expected_parent_id: None,
                    new_name: "Moved".into(),
                    new_parent_id: Some("b".into()),
                },
                FolderMove {
                    folder_id: "c".into(),
                    expected_name: "Spare".into(),
                    expected_parent_id: None,
                    new_name: "Spare2".into(),
                    new_parent_id: Some("b".into()),
                },
            ],
        };
        crate::commands::drive_folder_operations::apply(&db, &plan).unwrap();
        let id = history(&db).unwrap()[0].id.clone();
        let check = preview(&db, &id).unwrap();
        assert!(check.ready);
        db.execute(
            "CREATE TRIGGER qa_abort_undo BEFORE UPDATE ON drive_directories WHEN OLD.id='c'
            BEGIN SELECT RAISE(ABORT,'blocked'); END;",
        )
        .unwrap();
        assert!(apply(&db, &id, check.revision).is_err());
        let snapshot = drive_snapshot(&db).unwrap();
        assert!(snapshot
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Moved"));
        assert!(snapshot
            .directories
            .iter()
            .any(|d| d.id == "c" && d.name == "Spare2"));
        assert!(history(&db).unwrap()[0].undone_at.is_none());
        assert_eq!(snapshot.revision, check.revision);
    }
    #[test]
    fn ignored_journal_completion_rolls_back_folder_reversal() {
        let db = fixture();
        let id = create_history(&db);
        let check = preview(&db, &id).unwrap();
        db.execute(
            "CREATE TRIGGER qa_ignore_undo_done BEFORE UPDATE ON drive_local_folder_undo
            BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &id, check.revision).is_err());
        assert!(drive_snapshot(&db)
            .unwrap()
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Moved"));
    }
    #[test]
    fn new_pending_upload_inside_subtree_blocks_undo() {
        let db = fixture();
        let id = create_history(&db);
        db.execute(
            "INSERT INTO drive_pending_files
            (id,directory_id,display_name,staging_path,size,format_version,
             backing_channel_id,created_at,updated_at)
            VALUES('pending-late','kid','New upload','staging/new',1,1,1,3,3);",
        )
        .unwrap();
        let p = preview(&db, &id).unwrap();
        assert!(!p.ready);
        assert!(apply(&db, &id, p.revision).is_err());
    }
    #[test]
    fn silently_ignored_folder_undo_update_rolls_back_everything() {
        let db = fixture();
        let id = create_history(&db);
        let p = preview(&db, &id).unwrap();
        db.execute(
            "CREATE TRIGGER qa_ignore_folder_undo BEFORE UPDATE
            ON drive_directories WHEN OLD.id='a'
            BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &id, p.revision).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().revision, p.revision);
        assert!(history(&db).unwrap()[0].undone_at.is_none());
        assert!(drive_snapshot(&db)
            .unwrap()
            .directories
            .iter()
            .any(|d| d.id == "a" && d.name == "Moved"));
    }
    #[test]
    fn undo_history_retains_only_latest_twenty_four_batches() {
        let db = fixture();
        let mut name = "Alpha".to_owned();
        for n in 0..26 {
            let newer = format!("folder-round-{n}");
            let p = FolderPlan {
                revision: drive_snapshot(&db).unwrap().revision,
                changes: vec![FolderMove {
                    folder_id: "a".into(),
                    expected_name: name,
                    expected_parent_id: None,
                    new_name: newer.clone(),
                    new_parent_id: None,
                }],
            };
            crate::commands::drive_folder_operations::apply(&db, &p).unwrap();
            name = newer;
        }
        let saved = history(&db).unwrap();
        assert_eq!(saved.len(), MAX_HISTORY);
        assert_eq!(saved[0].entries.len(), 1);
        assert_eq!(saved[0].entries[0].current_name, "folder-round-25");
    }
    #[test]
    fn damaged_saved_history_refuses_undo() {
        let db = fixture();
        let id = create_history(&db);
        db.execute("UPDATE drive_local_folder_undo SET entries_json='not json';")
            .unwrap();
        assert!(preview(&db, &id).is_err());
        assert!(history(&db).is_err());
    }
}
