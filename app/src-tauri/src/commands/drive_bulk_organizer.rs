//! Metadata-only Drive Bulk Organizer. Never reads file bytes or contacts Telegram.
//! Preview and commit use the same validation; commit is one SQLite savepoint.
#[path = "drive_bulk_history.rs"]
mod history;
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, rename_drive_file, validate_drive_name, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_BATCH: usize = 100;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveBulkChange {
    pub entry_id: String,
    pub expected_name: String,
    pub expected_directory_id: Option<String>,
    pub new_name: String,
    pub new_directory_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveBulkRequest {
    pub revision: i64,
    pub changes: Vec<DriveBulkChange>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DriveBulkPreview {
    pub revision: i64,
    pub ready: bool,
    pub problems: Vec<String>,
    pub count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct DriveBulkApplied {
    pub changed: usize,
    pub revision: i64,
    pub dirty: bool,
}

fn occupied_key(folder: &Option<String>, name: &str) -> (Option<String>, String) {
    (folder.clone(), portable_name_key(name))
}

// A corrupt or legacy index may already contain two occupants of the same
// portable name. Never silently choose whichever entry happened to come last.
fn add_occupant(
    occupied: &mut HashMap<(Option<String>, String), Option<String>>,
    key: (Option<String>, String),
    id: String,
) {
    occupied
        .entry(key)
        .and_modify(|value| *value = None)
        .or_insert(Some(id));
}

struct NamespaceInvalidations {
    entries: Vec<(Option<String>, String)>,
    inodes: Vec<(String, String)>,
}

fn namespace_invalidations(request: &DriveBulkRequest) -> NamespaceInvalidations {
    let mut entries = HashSet::new();
    let mut inodes = HashSet::new();
    for change in &request.changes {
        entries.insert((
            change.expected_directory_id.clone(),
            change.expected_name.clone(),
        ));
        entries.insert((change.new_directory_id.clone(), change.new_name.clone()));
        inodes.insert(("file".to_owned(), change.entry_id.clone()));
    }
    let mut entries: Vec<_> = entries.into_iter().collect();
    let mut inodes: Vec<_> = inodes.into_iter().collect();
    entries.sort();
    inodes.sort();
    NamespaceInvalidations { entries, inodes }
}

fn verify_plan(snapshot: &DriveSnapshot, request: &DriveBulkRequest) -> Vec<String> {
    let mut problems = Vec::new();
    if request.revision != snapshot.revision {
        problems.push("Drive changed since preview. Refresh and review the batch again.".into());
        return problems;
    }
    if request.changes.is_empty() || request.changes.len() > MAX_BATCH {
        problems.push(format!(
            "Select between 1 and {MAX_BATCH} completed Drive files."
        ));
        return problems;
    }
    let files: HashMap<_, _> = snapshot
        .files
        .iter()
        .map(|f| (f.entry_id.as_str(), f))
        .collect();
    let folders: HashSet<_> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| d.id.as_str())
        .collect();
    let mut occupied: HashMap<(Option<String>, String), Option<String>> = HashMap::new();
    for file in &snapshot.files {
        add_occupant(
            &mut occupied,
            occupied_key(&file.directory_id, &file.display_name),
            file.entry_id.clone(),
        );
    }
    for pending in &snapshot.pending {
        add_occupant(
            &mut occupied,
            occupied_key(&pending.directory_id, &pending.display_name),
            format!("pending:{}", pending.id),
        );
    }
    for directory in snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
    {
        add_occupant(
            &mut occupied,
            occupied_key(&directory.parent_id, &directory.name),
            format!("folder:{}", directory.id),
        );
    }
    for symlink in snapshot.symlinks.iter().filter(|s| s.trashed_at.is_none()) {
        add_occupant(
            &mut occupied,
            occupied_key(&symlink.directory_id, &symlink.display_name),
            format!("symlink:{}", symlink.id),
        );
    }

    let mut ids = HashSet::new();
    let mut destinations = HashSet::new();
    for (index, change) in request.changes.iter().enumerate() {
        let label = format!("Row {}", index + 1);
        if !ids.insert(change.entry_id.as_str()) {
            problems.push(format!(
                "{label}: the same file was selected more than once."
            ));
            continue;
        }
        if change.entry_id.is_empty() || change.entry_id.len() > 256 {
            problems.push(format!("{label}: invalid file identity."));
            continue;
        }
        let Some(file) = files.get(change.entry_id.as_str()) else {
            problems.push(format!(
                "{label}: source is missing, unfinished, or no longer available."
            ));
            continue;
        };
        if file.display_name != change.expected_name
            || file.directory_id != change.expected_directory_id
        {
            problems.push(format!(
                "{label}: source moved or changed. Refresh before applying."
            ));
            continue;
        }
        if file.inode_id.is_some() {
            problems.push(format!(
                "{label}: shared-inode Drive links cannot be reorganized independently."
            ));
            continue;
        }
        if let Err(problem) = validate_drive_name(&change.new_name) {
            problems.push(format!("{label}: {problem}"));
            continue;
        }
        if change.new_name == file.display_name && change.new_directory_id == file.directory_id {
            problems.push(format!("{label}: no change to apply."));
            continue;
        }
        if let Some(parent) = change.new_directory_id.as_deref() {
            if !folders.contains(parent) {
                problems.push(format!("{label}: destination folder no longer exists."));
                continue;
            }
        }
        let key = occupied_key(&change.new_directory_id, &change.new_name);
        if !destinations.insert(key.clone()) {
            problems.push(format!(
                "{label}: two selected files would get the same destination name."
            ));
            continue;
        }
        if let Some(existing) = occupied.get(&key) {
            // A file currently occupying a target is not implicitly overwritten, even
            // if another row plans to move it. This avoids order-dependent swaps.
            if existing.as_deref() != Some(change.entry_id.as_str()) {
                problems.push(format!(
                    "{label}: destination already contains an item named \"{}\".",
                    change.new_name
                ));
            }
        }
    }
    problems
}

fn preview(
    conn: &sqlite::Connection,
    request: &DriveBulkRequest,
) -> Result<DriveBulkPreview, String> {
    let snapshot = drive_snapshot(conn)?;
    let problems = verify_plan(&snapshot, request);
    Ok(DriveBulkPreview {
        revision: snapshot.revision,
        ready: problems.is_empty(),
        problems,
        count: request.changes.len(),
    })
}

fn apply(
    conn: &sqlite::Connection,
    request: &DriveBulkRequest,
) -> Result<DriveBulkApplied, String> {
    // Snapshot preflight runs while holding the same DB mutex as commit in IPC.
    let checked = preview(conn, request)?;
    if !checked.ready {
        return Err(checked.problems.join(" "));
    }
    conn.execute("SAVEPOINT drive_bulk_organize")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        for change in &request.changes {
            rename_drive_file(
                conn,
                &change.entry_id,
                change.new_directory_id.as_deref(),
                &change.new_name,
            )?;
        }
        let updated = drive_snapshot(conn)?;
        // The journal is written inside this SAME savepoint. If the history
        // cannot be persisted, roll back every rename instead of losing undo.
        history::record_applied(conn, request, &updated)?;
        Ok(DriveBulkApplied {
            changed: request.changes.len(),
            revision: updated.revision,
            dirty: updated.dirty,
        })
    })();
    match result {
        Ok(value) => {
            if let Err(err) = conn.execute("RELEASE drive_bulk_organize") {
                let _ =
                    conn.execute("ROLLBACK TO drive_bulk_organize; RELEASE drive_bulk_organize");
                return Err(err.to_string());
            }
            Ok(value)
        }
        Err(error) => {
            conn.execute("ROLLBACK TO drive_bulk_organize; RELEASE drive_bulk_organize")
                .map_err(|rollback| format!("{error}; rollback also failed: {rollback}"))?;
            Err(error)
        }
    }
}

#[tauri::command]
pub fn cmd_drive_bulk_preview(
    db_pool: State<'_, DbConnection>,
    request: DriveBulkRequest,
) -> Result<DriveBulkPreview, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Drive database lock unavailable".to_string())?;
    preview(&conn, &request)
}

#[tauri::command]
pub fn cmd_drive_bulk_apply(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
    request: DriveBulkRequest,
) -> Result<DriveBulkApplied, String> {
    // Release the database lock BEFORE notifying the kernel. FUSE invalidation
    // can cause file-manager callbacks which need this same database mutex.
    let applied = {
        let conn = db_pool
            .lock()
            .map_err(|_| "Drive database lock unavailable".to_string())?;
        apply(&conn, &request)?
    };
    let delta = namespace_invalidations(&request);
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&delta.entries, &delta.inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = &delta;
    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Could not refresh Windows Drive after bulk organization: {error}");
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Could not signal macOS Drive after bulk organization: {error}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app_handle;
    Ok(applied)
}

#[tauri::command]
pub fn cmd_drive_bulk_history(
    db_pool: State<'_, DbConnection>,
) -> Result<Vec<history::BulkUndoHistory>, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Drive database lock unavailable".to_string())?;
    history::list(&conn)
}

fn undo_preview(conn: &sqlite::Connection, batch_id: &str) -> Result<DriveBulkPreview, String> {
    let snapshot = drive_snapshot(conn)?;
    let history = history::load(conn, batch_id)?;
    let inverse = history::inverse(&snapshot, &history)?;
    let problems = verify_plan(&snapshot, &inverse);
    Ok(DriveBulkPreview {
        revision: snapshot.revision,
        ready: problems.is_empty(),
        problems,
        count: inverse.changes.len(),
    })
}

#[tauri::command]
pub fn cmd_drive_bulk_undo_preview(
    db_pool: State<'_, DbConnection>,
    batch_id: String,
) -> Result<DriveBulkPreview, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Drive database lock unavailable".to_string())?;
    undo_preview(&conn, &batch_id)
}

fn undo(
    conn: &sqlite::Connection,
    batch_id: &str,
    expected_revision: i64,
) -> Result<(DriveBulkApplied, DriveBulkRequest), String> {
    let snapshot = drive_snapshot(conn)?;
    if expected_revision != snapshot.revision {
        return Err("Drive changed since the undo preview. Refresh and inspect again.".into());
    }
    let saved = history::load(conn, batch_id)?;
    let inverse = history::inverse(&snapshot, &saved)?;
    let problems = verify_plan(&snapshot, &inverse);
    if !problems.is_empty() {
        return Err(problems.join(" "));
    }
    conn.execute("SAVEPOINT drive_bulk_undo")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        for change in &inverse.changes {
            rename_drive_file(
                conn,
                &change.entry_id,
                change.new_directory_id.as_deref(),
                &change.new_name,
            )?;
        }
        history::mark_undone(conn, batch_id)?;
        let state = drive_snapshot(conn)?;
        Ok(DriveBulkApplied {
            changed: inverse.changes.len(),
            revision: state.revision,
            dirty: state.dirty,
        })
    })();
    match outcome {
        Ok(result) => {
            if let Err(e) = conn.execute("RELEASE drive_bulk_undo") {
                let _ = conn.execute("ROLLBACK TO drive_bulk_undo; RELEASE drive_bulk_undo");
                return Err(e.to_string());
            }
            Ok((result, inverse))
        }
        Err(problem) => {
            conn.execute("ROLLBACK TO drive_bulk_undo; RELEASE drive_bulk_undo")
                .map_err(|e| format!("{problem}; rollback also failed: {e}"))?;
            Err(problem)
        }
    }
}

#[tauri::command]
pub fn cmd_drive_bulk_undo(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
    batch_id: String,
    expected_revision: i64,
) -> Result<DriveBulkApplied, String> {
    // Do not notify Linux FUSE or desktop providers while holding the SQLite
    // lock, and never notify after a failed or rolled-back undo.
    let (applied, inverse) = {
        let conn = db_pool
            .lock()
            .map_err(|_| "Drive database lock unavailable".to_string())?;
        undo(&conn, &batch_id, expected_revision)?
    };
    let delta = namespace_invalidations(&inverse);
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&delta.entries, &delta.inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = &delta;
    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Could not refresh Windows Drive after batch undo: {error}");
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Could not signal macOS Drive after batch undo: {error}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app_handle;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn setup() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER NOT NULL UNIQUE,
                name TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'owner',
                storage_version INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, joined_at INTEGER NOT NULL);
            CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY, logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL, display_name TEXT NOT NULL,
                total_size INTEGER NOT NULL, mime_type TEXT, chunk_count INTEGER NOT NULL DEFAULT 1,
                whole_sha256 TEXT, storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');
            INSERT INTO logical_channels (logical_id,backing_channel_id,name,created_at,joined_at)
                VALUES ('qa-organizer-channel', 971, 'QA', 1, 1);
            INSERT INTO logical_files (file_id,logical_channel_id,first_message_id,display_name,total_size,created_at)
                VALUES ('qa-object-one','qa-organizer-channel',11,'one.txt',5,1),
                       ('qa-object-two','qa-organizer-channel',12,'two.txt',6,1);",
        ).unwrap();
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories (id,name,created_at,updated_at) VALUES
                ('qa-target','Target',1,1);
             INSERT INTO drive_file_entries
                (entry_id,file_id,display_name,created_at,updated_at)
                VALUES ('qa-entry-one','qa-object-one','one.txt',1,1),
                       ('qa-entry-two','qa-object-two','two.txt',1,1);",
        )
        .unwrap();
        conn
    }

    fn request(conn: &sqlite::Connection, names: [&str; 2]) -> DriveBulkRequest {
        DriveBulkRequest {
            revision: drive_snapshot(conn).unwrap().revision,
            changes: ["qa-entry-one", "qa-entry-two"]
                .iter()
                .enumerate()
                .map(|(i, id)| DriveBulkChange {
                    entry_id: (*id).into(),
                    expected_name: if i == 0 { "one.txt" } else { "two.txt" }.into(),
                    expected_directory_id: None,
                    new_name: names[i].into(),
                    new_directory_id: Some("qa-target".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn invalidation_plan_includes_both_sides_of_every_committed_move() {
        let conn = setup();
        let request = request(&conn, ["archived-one.txt", "archived-two.txt"]);
        let delta = namespace_invalidations(&request);
        let (entries, inodes) = (delta.entries, delta.inodes);
        assert_eq!(entries.len(), 4);
        assert!(entries.contains(&(None, "one.txt".into())));
        assert!(entries.contains(&(None, "two.txt".into())));
        assert!(entries.contains(&(Some("qa-target".into()), "archived-one.txt".into())));
        assert!(entries.contains(&(Some("qa-target".into()), "archived-two.txt".into())));
        assert_eq!(
            inodes,
            vec![
                ("file".into(), "qa-entry-one".into()),
                ("file".into(), "qa-entry-two".into())
            ]
        );
    }

    #[test]
    fn ambiguous_preexisting_portable_name_never_selects_last_occupant() {
        let conn = setup();
        let mut snapshot = drive_snapshot(&conn).unwrap();
        let mut duplicate = snapshot
            .files
            .iter()
            .find(|f| f.entry_id == "qa-entry-one")
            .unwrap()
            .clone();
        duplicate.entry_id = "qa-shadow".into();
        snapshot.files.insert(0, duplicate);
        let request = DriveBulkRequest {
            revision: snapshot.revision,
            changes: vec![DriveBulkChange {
                entry_id: "qa-entry-one".into(),
                expected_name: "one.txt".into(),
                expected_directory_id: None,
                new_name: "ONE.txt".into(),
                new_directory_id: None,
            }],
        };
        assert!(verify_plan(&snapshot, &request)
            .iter()
            .any(|p| p.contains("destination already contains")));
    }

    #[test]
    fn preview_and_atomic_bulk_move_rename_both_files() {
        let conn = setup();
        let request = request(&conn, ["archive-one.txt", "archive-two.txt"]);
        assert!(preview(&conn, &request).unwrap().ready);
        let done = apply(&conn, &request).unwrap();
        assert_eq!(done.changed, 2);
        assert!(done.dirty);
        assert!(done.revision > request.revision);
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(
            snapshot
                .files
                .iter()
                .filter(|file| file.directory_id.as_deref() == Some("qa-target"))
                .count(),
            2
        );
    }

    #[test]
    fn reject_target_collision_without_mutation_and_stale_revision() {
        let conn = setup();
        let before = drive_snapshot(&conn).unwrap();
        let mut conflicting = request(&conn, ["same.txt", "same.txt"]);
        assert!(!preview(&conn, &conflicting).unwrap().ready);
        assert!(apply(&conn, &conflicting).is_err());
        assert_eq!(drive_snapshot(&conn).unwrap().revision, before.revision);
        conflicting.changes[1].new_name = "unique.txt".into();
        apply(&conn, &conflicting).unwrap();
        assert!(apply(&conn, &conflicting)
            .unwrap_err()
            .contains("Drive changed"));
    }

    #[test]
    fn unicode_equivalent_targets_collide_before_commit() {
        let conn = setup();
        let plan = request(&conn, ["Café.txt", "Cafe\u{301}.txt"]);
        assert!(!preview(&conn, &plan).unwrap().ready);
        assert!(apply(&conn, &plan).is_err());
    }

    #[test]
    fn existing_occupants_and_pending_writes_are_never_overwritten() {
        let conn = setup();
        let mut plan = request(&conn, ["Target", "moved.csv"]);
        plan.changes[0].new_directory_id = None;
        assert!(
            !preview(&conn, &plan).unwrap().ready,
            "existing folder must block root collision"
        );
        assert!(apply(&conn, &plan).is_err());

        plan.changes[0].new_name = "two.txt".into();
        assert!(
            !preview(&conn, &plan).unwrap().ready,
            "file that another row moves away must not be implicitly overwritten"
        );
        assert!(apply(&conn, &plan).is_err());

        conn.execute(
            "INSERT INTO drive_pending_files
                (id,directory_id,display_name,staging_path,backing_channel_id,created_at,updated_at)
             VALUES ('qa-stage','qa-target','pending.bin','qa-private-staging',971,1,1);",
        )
        .unwrap();
        plan.changes[0].new_name = "pending.bin".into();
        plan.changes[0].new_directory_id = Some("qa-target".into());
        assert!(
            !preview(&conn, &plan).unwrap().ready,
            "unfinished write must block destination"
        );
        assert!(apply(&conn, &plan).is_err());
    }

    #[test]
    fn invalid_destination_and_identity_rejected_before_writing() {
        let conn = setup();
        let before = drive_snapshot(&conn).unwrap();
        let mut plan = request(&conn, ["valid.txt", "other.txt"]);
        plan.changes[1].new_directory_id = Some("missing-folder".into());
        assert!(!preview(&conn, &plan).unwrap().ready);
        plan.changes[1].new_directory_id = Some("qa-target".into());
        plan.changes[1].expected_name = "stale-filename.txt".into();
        assert!(!preview(&conn, &plan).unwrap().ready);
        plan.changes[1].expected_name = "two.txt".into();
        plan.changes[1].new_name = "CON.txt".into();
        assert!(!preview(&conn, &plan).unwrap().ready);
        assert_eq!(drive_snapshot(&conn).unwrap().revision, before.revision);
    }

    #[test]
    fn rollback_first_success_when_second_write_fails() {
        let conn = setup();
        let plan = request(&conn, ["good.txt", "bad.txt"]);
        conn.execute(
            "CREATE TRIGGER qa_reject_bulk_second BEFORE UPDATE ON drive_file_entries
             WHEN NEW.entry_id = 'qa-entry-two'
             BEGIN SELECT RAISE(ABORT, 'QA second row rejected'); END;",
        )
        .unwrap();
        let before = drive_snapshot(&conn).unwrap();
        assert!(apply(&conn, &plan)
            .unwrap_err()
            .contains("QA second row rejected"));
        let after = drive_snapshot(&conn).unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(
            after
                .files
                .iter()
                .map(|f| f.display_name.as_str())
                .collect::<Vec<_>>(),
            before
                .files
                .iter()
                .map(|f| f.display_name.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn successful_batch_records_durable_history_and_undo_restores_both_names() {
        let conn = setup();
        let forward = request(&conn, ["archive-a.txt", "archive-b.txt"]);
        let after_apply = apply(&conn, &forward).unwrap();
        let recent = history::list(&conn).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].files.len(), 2);
        assert_eq!(recent[0].applied_revision, after_apply.revision);
        assert!(recent[0].undone_at.is_none());
        let undo_preview = undo_preview(&conn, &recent[0].id).unwrap();
        assert!(undo_preview.ready, "{:?}", undo_preview.problems);
        let (undone, inverse) = undo(&conn, &recent[0].id, undo_preview.revision).unwrap();
        assert_eq!(undone.changed, 2);
        let snapshot = drive_snapshot(&conn).unwrap();
        let first = snapshot
            .files
            .iter()
            .find(|f| f.entry_id == "qa-entry-one")
            .unwrap();
        assert_eq!(first.display_name, "one.txt");
        assert_eq!(first.directory_id, None);
        assert!(snapshot.files.iter().all(|f| f.directory_id.is_none()));
        assert!(history::list(&conn).unwrap()[0].undone_at.is_some());
        assert!(undo(&conn, &recent[0].id, undone.revision).is_err());
        let delta = namespace_invalidations(&inverse);
        assert_eq!(delta.entries.len(), 4);
    }

    #[test]
    fn undo_preview_blocks_replaced_file_identity_and_stale_revisions() {
        let conn = setup();
        let forward = request(&conn, ["archive-a.txt", "archive-b.txt"]);
        apply(&conn, &forward).unwrap();
        let entry = history::list(&conn).unwrap().remove(0);
        assert!(undo(&conn, &entry.id, forward.revision)
            .unwrap_err()
            .contains("Drive changed"));
        conn.execute(
            "UPDATE drive_file_entries SET display_name='user-renamed.txt'
             WHERE entry_id='qa-entry-one'",
        )
        .unwrap();
        assert!(undo_preview(&conn, &entry.id).is_err());
        assert!(history::list(&conn).unwrap()[0].undone_at.is_none());
    }

    #[test]
    fn undo_refuses_to_overwrite_new_destination_occupants() {
        let conn = setup();
        let forward = request(&conn, ["archive-a.txt", "archive-b.txt"]);
        apply(&conn, &forward).unwrap();
        let record = history::list(&conn).unwrap().remove(0);
        conn.execute(
            "INSERT INTO drive_file_entries
             (entry_id,file_id,display_name,created_at,updated_at)
             VALUES ('occupied','qa-object-one','one.txt',1,1)",
        )
        .unwrap();
        let preflight = undo_preview(&conn, &record.id).unwrap();
        assert!(!preflight.ready);
        assert!(preflight
            .problems
            .iter()
            .any(|x| x.contains("destination already")));
        assert!(undo(&conn, &record.id, preflight.revision).is_err());
        assert_eq!(history::list(&conn).unwrap()[0].undone_at, None);
    }

    #[test]
    fn undo_second_failure_rolls_back_first_and_history_status() {
        let conn = setup();
        let forward = request(&conn, ["archive-a.txt", "archive-b.txt"]);
        apply(&conn, &forward).unwrap();
        let record = history::list(&conn).unwrap().remove(0);
        conn.execute(
            "CREATE TRIGGER refuse_undo_second BEFORE UPDATE ON drive_file_entries
             WHEN NEW.entry_id='qa-entry-two' AND NEW.display_name='two.txt'
             BEGIN SELECT RAISE(ABORT, 'QA undo second rejected'); END",
        )
        .unwrap();
        let rev = drive_snapshot(&conn).unwrap().revision;
        assert!(undo(&conn, &record.id, rev)
            .unwrap_err()
            .contains("QA undo second rejected"));
        let after = drive_snapshot(&conn).unwrap();
        assert_eq!(after.revision, rev);
        assert!(after
            .files
            .iter()
            .any(|f| f.display_name == "archive-a.txt"));
        assert!(after
            .files
            .iter()
            .any(|f| f.display_name == "archive-b.txt"));
        assert!(history::list(&conn).unwrap()[0].undone_at.is_none());
    }

    #[test]
    fn ignored_journal_completion_aborts_entire_undo() {
        let conn = setup();
        let forward = request(&conn, ["moved-a.txt", "moved-b.txt"]);
        apply(&conn, &forward).unwrap();
        let saved = history::list(&conn).unwrap().remove(0);
        conn.execute(
            "CREATE TRIGGER ignore_undo_status BEFORE UPDATE ON drive_local_bulk_undo
             WHEN NEW.undone_at IS NOT NULL BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        let before = drive_snapshot(&conn).unwrap();
        let reason = undo(&conn, &saved.id, before.revision).unwrap_err();
        assert!(reason.contains("completion was not recorded"));
        assert_eq!(drive_snapshot(&conn).unwrap().revision, before.revision);
        assert!(history::list(&conn).unwrap()[0].undone_at.is_none());
        assert!(drive_snapshot(&conn)
            .unwrap()
            .files
            .iter()
            .any(|f| f.display_name == "moved-a.txt"));
    }

    #[test]
    fn malformed_journal_records_never_trigger_a_partial_restore() {
        let conn = setup();
        apply(&conn, &request(&conn, ["archive-a.txt", "archive-b.txt"])).unwrap();
        let saved = history::list(&conn).unwrap().remove(0);
        let before = drive_snapshot(&conn).unwrap();
        let mut tampered = saved.files.clone();
        tampered[0].file_id = "../outside".into();
        let payload = serde_json::to_string(&tampered).unwrap();
        let mut statement = conn
            .prepare("UPDATE drive_local_bulk_undo SET files_json=? WHERE id=?")
            .unwrap();
        statement.bind((1, payload.as_str())).unwrap();
        statement.bind((2, saved.id.as_str())).unwrap();
        statement.next().unwrap();
        drop(statement);
        assert!(history::list(&conn).is_err());
        assert!(undo_preview(&conn, &saved.id).is_err());
        assert!(undo(&conn, &saved.id, before.revision).is_err());
        assert_eq!(drive_snapshot(&conn).unwrap().revision, before.revision);
    }

    #[test]
    fn unfinished_copy_on_write_replacement_blocks_undo() {
        let conn = setup();
        apply(&conn, &request(&conn, ["archive-a.txt", "archive-b.txt"])).unwrap();
        let saved = history::list(&conn).unwrap().remove(0);
        conn.execute(
            "INSERT INTO drive_pending_files (
                id,display_name,base_entry_id,staging_path,backing_channel_id,created_at,updated_at
             ) VALUES ('undo-qa-write','archive-a.txt','qa-entry-one','private-qa-unopened',971,1,1)"
        ).unwrap();
        assert!(undo_preview(&conn, &saved.id)
            .unwrap_err()
            .contains("unfinished copy-on-write"));
        assert!(history::list(&conn).unwrap()[0].undone_at.is_none());
    }

    #[test]
    fn journal_failure_rolls_back_organizer_not_just_history() {
        let conn = setup();
        assert!(history::list(&conn).unwrap().is_empty());
        conn.execute(
            "CREATE TRIGGER refuse_journal BEFORE INSERT ON drive_local_bulk_undo
             BEGIN SELECT RAISE(ABORT, 'QA undo journal failure'); END",
        )
        .unwrap();
        let before = drive_snapshot(&conn).unwrap();
        let plan = request(&conn, ["moved-a.txt", "moved-b.txt"]);
        assert!(apply(&conn, &plan)
            .unwrap_err()
            .contains("QA undo journal failure"));
        let after = drive_snapshot(&conn).unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(
            before
                .files
                .iter()
                .map(|x| x.display_name.as_str())
                .collect::<Vec<_>>(),
            after
                .files
                .iter()
                .map(|x| x.display_name.as_str())
                .collect::<Vec<_>>()
        );
    }
}
