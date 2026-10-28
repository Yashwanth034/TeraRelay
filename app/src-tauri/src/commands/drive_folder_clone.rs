//! Copy an existing Drive folder tree as a NEW, empty hierarchy.
//! Source folders/files are never modified; no file bytes or Telegram objects
//! are copied. Preview and apply share fail-closed planning under the DB lock.
use crate::commands::drive_metadata::{
    create_drive_directory, drive_snapshot, portable_name_key, validate_drive_name,
    DriveDirectoryRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_CLONE_FOLDERS: usize = 512;
const MAX_CLONE_DEPTH: usize = 64;
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_DISPLAYED_PATHS: usize = 80;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderCloneRequest {
    pub revision: i64,
    pub source_id: String,
    pub expected_source_name: String,
    pub expected_source_parent_id: Option<String>,
    pub destination_parent_id: Option<String>,
    pub new_root_name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderClonePreview {
    pub revision: i64,
    pub ready: bool,
    pub count: usize,
    pub not_copied_files: usize,
    pub not_copied_pending: usize,
    pub paths: Vec<String>,
    pub truncated: bool,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderCloneApplied {
    pub created: usize,
    pub root_id: String,
    pub revision: i64,
    pub dirty: bool,
}

struct CloneStep {
    source_id: String,
    source_parent_id: Option<String>,
    name: String,
    relative_path: String,
}
struct ClonePlan {
    steps: Vec<CloneStep>,
    not_copied_files: usize,
    not_copied_pending: usize,
}
type FolderCloneCommit = (
    FolderCloneApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);
fn identity_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn source_ancestry_valid(start: &str, folders: &HashMap<&str, &DriveDirectoryRecord>) -> bool {
    let mut visited = HashSet::new();
    let mut cursor = Some(start);
    while let Some(id) = cursor {
        if !visited.insert(id) {
            return false;
        }
        let Some(folder) = folders.get(id) else {
            return false;
        };
        if visited.len() > 512 {
            return false;
        }
        cursor = folder.parent_id.as_deref();
    }
    true
}
fn validate(
    snapshot: &DriveSnapshot,
    request: &FolderCloneRequest,
) -> Result<ClonePlan, Vec<String>> {
    let mut problems = Vec::new();
    if request.revision != snapshot.revision {
        return Err(vec![
            "Drive changed since preview. Refresh the snapshot before cloning.".into(),
        ]);
    }
    if !identity_ok(&request.source_id) {
        problems.push("Invalid source folder identity.".into());
    }
    if let Some(id) = request.destination_parent_id.as_deref() {
        if !identity_ok(id) {
            problems.push("Invalid destination folder identity.".into());
        }
    }
    if let Err(e) = validate_drive_name(&request.new_root_name) {
        problems.push(format!("New top folder: {e}."));
    }
    let folders: HashMap<&str, &DriveDirectoryRecord> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let source = folders.get(request.source_id.as_str()).copied();
    match source {
        None => problems.push("Source folder no longer exists or is trashed.".into()),
        Some(folder)
            if folder.name != request.expected_source_name
                || folder.parent_id != request.expected_source_parent_id =>
        {
            problems.push("Source folder name or parent changed since selection.".into())
        }
        Some(_) => {}
    }
    if let Some(target) = request.destination_parent_id.as_deref() {
        if !folders.contains_key(target) {
            problems.push("Destination folder is missing or trashed.".into());
        } else if !source_ancestry_valid(target, &folders) {
            problems.push("Destination folder ancestry is invalid.".into());
        }
    }
    if source.is_some() && !source_ancestry_valid(&request.source_id, &folders) {
        problems.push("Source folder ancestry is invalid or cyclic.".into());
    }
    // A static-snapshot clone of a subtree into its own descendant would be
    // technically finite, but produces a surprising recursively nested copy.
    if let Some(target) = request.destination_parent_id.as_deref() {
        let mut cursor = Some(target);
        let mut seen = HashSet::new();
        while let Some(id) = cursor {
            if id == request.source_id {
                problems.push("Choose a destination outside the source subtree.".into());
                break;
            }
            if !seen.insert(id) {
                break;
            }
            cursor = folders.get(id).and_then(|d| d.parent_id.as_deref());
        }
    }
    // Portable namespace is shared with files, pending writes and symlinks.
    let destination_key = portable_name_key(&request.new_root_name);
    let target_parent = &request.destination_parent_id;
    if snapshot.directories.iter().any(|d| {
        d.trashed_at.is_none()
            && &d.parent_id == target_parent
            && portable_name_key(&d.name) == destination_key
    }) || snapshot.files.iter().any(|f| {
        &f.directory_id == target_parent && portable_name_key(&f.display_name) == destination_key
    }) || snapshot.pending.iter().any(|p| {
        &p.directory_id == target_parent && portable_name_key(&p.display_name) == destination_key
    }) || snapshot.symlinks.iter().any(|s| {
        s.trashed_at.is_none()
            && &s.directory_id == target_parent
            && portable_name_key(&s.display_name) == destination_key
    }) {
        problems.push(
            "A folder, file, pending write or symlink already occupies the destination name."
                .into(),
        );
    }
    if !problems.is_empty() {
        return Err(problems);
    }

    let mut children: HashMap<Option<&str>, Vec<&DriveDirectoryRecord>> = HashMap::new();
    for dir in folders.values() {
        children
            .entry(dir.parent_id.as_deref())
            .or_default()
            .push(dir);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| {
            portable_name_key(&a.name)
                .cmp(&portable_name_key(&b.name))
                .then(a.id.cmp(&b.id))
        });
    }
    let mut steps = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![(
        request.source_id.as_str(),
        1usize,
        request.new_root_name.clone(),
    )];
    while let Some((id, depth, path)) = stack.pop() {
        if !visited.insert(id) {
            return Err(vec![
                "Source folder tree contains repeated or cyclic identities.".into(),
            ]);
        }
        if depth > MAX_CLONE_DEPTH
            || steps.len() >= MAX_CLONE_FOLDERS
            || path.len() > MAX_RELATIVE_PATH_BYTES
        {
            return Err(vec![format!(
                "Folder copy exceeds the safe limit of {MAX_CLONE_FOLDERS} folders,                  {MAX_CLONE_DEPTH} levels or {MAX_RELATIVE_PATH_BYTES} path bytes."
            )]);
        }
        let Some(source_dir) = folders.get(id) else {
            return Err(vec!["Source subtree contains a missing folder.".into()]);
        };
        if validate_drive_name(&source_dir.name).is_err() || !identity_ok(&source_dir.id) {
            return Err(vec!["Source subtree contains an invalid directory.".into()]);
        }
        let mut sibling_names = HashSet::new();
        if let Some(kids) = children.get(&Some(id)) {
            for kid in kids {
                if !sibling_names.insert(portable_name_key(&kid.name)) {
                    return Err(vec![
                        "Source subtree has ambiguous sibling names; clone refused.".into(),
                    ]);
                }
            }
        }
        steps.push(CloneStep {
            source_id: id.into(),
            source_parent_id: source_dir.parent_id.clone(),
            name: if depth == 1 {
                request.new_root_name.clone()
            } else {
                source_dir.name.clone()
            },
            relative_path: path.clone(),
        });
        if let Some(kids) = children.get(&Some(id)) {
            for kid in kids.iter().rev() {
                let next = format!("{path}/{}", kid.name);
                stack.push((&kid.id, depth + 1, next));
            }
        }
    }
    // A structure copy NEVER includes file payloads, unfinished writes or
    // storage objects. Return exact visible counts so the user can consent.
    let not_copied_files = snapshot
        .files
        .iter()
        .filter(|f| {
            f.directory_id
                .as_deref()
                .is_some_and(|id| visited.contains(id))
        })
        .count();
    let not_copied_pending = snapshot
        .pending
        .iter()
        .filter(|p| {
            p.directory_id
                .as_deref()
                .is_some_and(|id| visited.contains(id))
        })
        .count();
    Ok(ClonePlan {
        steps,
        not_copied_files,
        not_copied_pending,
    })
}
fn preview(
    conn: &sqlite::Connection,
    request: &FolderCloneRequest,
) -> Result<FolderClonePreview, String> {
    let snapshot = drive_snapshot(conn)?;
    match validate(&snapshot, request) {
        Ok(plan) => Ok(FolderClonePreview {
            revision: snapshot.revision,
            ready: true,
            count: plan.steps.len(),
            not_copied_files: plan.not_copied_files,
            not_copied_pending: plan.not_copied_pending,
            paths: plan
                .steps
                .iter()
                .take(MAX_DISPLAYED_PATHS)
                .map(|s| s.relative_path.clone())
                .collect(),
            truncated: plan.steps.len() > MAX_DISPLAYED_PATHS,
            problems: vec![],
        }),
        Err(problems) => Ok(FolderClonePreview {
            revision: snapshot.revision,
            ready: false,
            count: 0,
            not_copied_files: 0,
            not_copied_pending: 0,
            paths: vec![],
            truncated: false,
            problems,
        }),
    }
}
fn apply(
    conn: &sqlite::Connection,
    request: &FolderCloneRequest,
) -> Result<FolderCloneCommit, String> {
    let snapshot = drive_snapshot(conn)?;
    let plan = validate(&snapshot, request).map_err(|errors| errors.join(" "))?;
    conn.execute("SAVEPOINT drive_folder_clone_tree")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut new_ids: HashMap<String, String> = HashMap::new();
        let mut entries = Vec::new();
        let mut inodes = Vec::new();
        for (i, step) in plan.steps.iter().enumerate() {
            let parent = if i == 0 {
                request.destination_parent_id.clone()
            } else {
                Some(
                    new_ids
                        .get(
                            step.source_parent_id
                                .as_deref()
                                .ok_or("Missing source parent")?,
                        )
                        .ok_or("Could not resolve new parent")?
                        .clone(),
                )
            };
            let created = create_drive_directory(conn, parent.as_deref(), &step.name)?;
            new_ids.insert(step.source_id.clone(), created.id.clone());
            entries.push((parent, created.name.clone()));
            inodes.push(("dir".to_owned(), created.id));
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= snapshot.revision {
            return Err("Drive revision did not advance during folder cloning.".into());
        }
        let by_id: HashMap<_, _> = after
            .directories
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        for (step, (_, new_id)) in plan.steps.iter().zip(inodes.iter()) {
            let expected_parent = if step.source_id == request.source_id {
                request.destination_parent_id.as_deref()
            } else {
                Some(
                    new_ids
                        .get(step.source_parent_id.as_deref().ok_or("Missing parent")?)
                        .ok_or("Missing mapped parent")?
                        .as_str(),
                )
            };
            if !matches!(by_id.get(new_id.as_str()), Some(directory)
                if directory.name == step.name
                    && directory.parent_id.as_deref() == expected_parent
                    && directory.trashed_at.is_none())
            {
                return Err(
                    "A folder creation was ignored or altered; rolling back all new folders."
                        .into(),
                );
            }
        }
        let root_id = new_ids
            .get(&request.source_id)
            .ok_or("Missing new root folder")?
            .clone();
        Ok((
            FolderCloneApplied {
                created: plan.steps.len(),
                root_id,
                revision: after.revision,
                dirty: after.dirty,
            },
            entries,
            inodes,
        ))
    })();
    match result {
        Ok(value) => {
            if let Err(e) = conn.execute("RELEASE drive_folder_clone_tree") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_folder_clone_tree; RELEASE drive_folder_clone_tree",
                );
                return Err(e.to_string());
            }
            Ok(value)
        }
        Err(reason) => {
            conn.execute("ROLLBACK TO drive_folder_clone_tree; RELEASE drive_folder_clone_tree")
                .map_err(|e| format!("{reason}; rollback failed: {e}"))?;
            Err(reason)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_folder_clone_preview(
    db_pool: State<'_, DbConnection>,
    request: FolderCloneRequest,
) -> Result<FolderClonePreview, String> {
    let conn = db_pool
        .lock()
        .map_err(|_| "Drive database unavailable".to_owned())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_folder_clone_apply(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
    request: FolderCloneRequest,
) -> Result<FolderCloneApplied, String> {
    let (applied, entries, inodes) = {
        let conn = db_pool
            .lock()
            .map_err(|_| "Drive database unavailable".to_owned())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&entries, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&entries, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Drive clone committed but Windows refresh was unavailable: {error}");
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Drive clone committed but macOS refresh was unavailable: {error}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app_handle;
    Ok(applied)
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
                created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');"
        ).unwrap();
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES
                ('source',NULL,'Source',1,1),
                ('child','source','Docs',1,1),
                ('leaf','child','Reports',1,1),
                ('dest',NULL,'Destination',1,1);",
        )
        .unwrap();
        conn
    }
    fn request(conn: &sqlite::Connection, target: Option<&str>, title: &str) -> FolderCloneRequest {
        FolderCloneRequest {
            revision: drive_snapshot(conn).unwrap().revision,
            source_id: "source".into(),
            expected_source_name: "Source".into(),
            expected_source_parent_id: None,
            destination_parent_id: target.map(str::to_owned),
            new_root_name: title.into(),
        }
    }
    #[test]
    fn deep_hierarchy_is_copied_to_new_ids_without_mutating_source() {
        let db = fixture();
        let req = request(&db, Some("dest"), "Project 2027");
        let checked = preview(&db, &req).unwrap();
        assert!(checked.ready);
        assert_eq!(checked.count, 3);
        assert_eq!(
            checked.paths,
            vec![
                "Project 2027",
                "Project 2027/Docs",
                "Project 2027/Docs/Reports"
            ]
        );
        let (done, _, _) = apply(&db, &req).unwrap();
        assert_eq!(done.created, 3);
        let current = drive_snapshot(&db).unwrap();
        assert_eq!(current.directories.len(), 7);
        assert!(current
            .directories
            .iter()
            .any(|d| d.id == "source" && d.name == "Source"));
        let clone = current
            .directories
            .iter()
            .find(|d| d.id == done.root_id)
            .unwrap();
        assert_eq!(clone.parent_id.as_deref(), Some("dest"));
        let child = current
            .directories
            .iter()
            .find(|d| d.parent_id.as_deref() == Some(done.root_id.as_str()))
            .unwrap();
        let leaf = current
            .directories
            .iter()
            .find(|d| d.parent_id.as_deref() == Some(child.id.as_str()))
            .unwrap();
        assert_eq!(child.name, "Docs");
        assert_eq!(leaf.name, "Reports");
    }
    #[test]
    fn destination_collision_and_recursive_destination_are_rejected() {
        let db = fixture();
        assert!(
            !preview(&db, &request(&db, None, "Destination"))
                .unwrap()
                .ready
        );
        assert!(
            !preview(&db, &request(&db, Some("child"), "Nested"))
                .unwrap()
                .ready
        );
    }
    #[test]
    fn stale_revision_and_stale_source_fail_closed() {
        let db = fixture();
        let mut req = request(&db, Some("dest"), "Copied");
        req.revision = -1;
        assert!(!preview(&db, &req).unwrap().ready);
        req.revision = drive_snapshot(&db).unwrap().revision;
        req.expected_source_name = "Renamed elsewhere".into();
        assert!(!preview(&db, &req).unwrap().ready);
    }
    #[test]
    fn late_insert_failure_rolls_back_all_new_directories() {
        let db = fixture();
        let req = request(&db, Some("dest"), "Copy");
        db.execute(
            "CREATE TRIGGER qa_clone_abort BEFORE INSERT ON drive_directories
             WHEN NEW.name = 'Reports' BEGIN SELECT RAISE(ABORT, 'test-failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &req).is_err());
        let current = drive_snapshot(&db).unwrap();
        assert_eq!(current.directories.len(), 4);
        assert_eq!(current.revision, req.revision);
    }
    #[test]
    fn silently_ignored_insert_cannot_be_reported_as_a_success() {
        let db = fixture();
        let req = request(&db, Some("dest"), "Copy");
        db.execute(
            "CREATE TRIGGER qa_clone_ignore BEFORE INSERT ON drive_directories
             WHEN NEW.name = 'Reports' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &req).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 4);
    }
    #[test]
    fn occupied_pending_and_symlink_names_block_clone_destination() {
        use crate::commands::drive_metadata::DrivePendingRecord;
        use crate::drive_posix::DriveSymlinkRecord;
        let db = fixture();
        let req = request(&db, Some("dest"), "Copy");
        let mut snapshot = drive_snapshot(&db).unwrap();
        snapshot.pending.push(DrivePendingRecord {
            id: "unfinished".into(),
            directory_id: Some("dest".into()),
            display_name: "copy".into(),
            staging_path: "unused".into(),
            size: 1,
            format_version: 2,
            backing_channel_id: 0,
            base_entry_id: None,
            encryption_version: 2,
            crypto_id: None,
            closed_at: None,
            created_at: 1,
            updated_at: 1,
        });
        assert!(validate(&snapshot, &req).is_err());
        snapshot.pending.clear();
        snapshot.symlinks.push(DriveSymlinkRecord {
            id: "link".into(),
            directory_id: Some("dest".into()),
            display_name: "COPY".into(),
            target: "../source".into(),
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        assert!(validate(&snapshot, &req).is_err());
    }
    #[test]
    fn source_hierarchy_limit_and_directory_cycle_fail_closed() {
        let db = fixture();
        let req = request(&db, Some("dest"), "Copy");
        let mut snapshot = drive_snapshot(&db).unwrap();
        for n in 0..MAX_CLONE_FOLDERS {
            snapshot.directories.push(DriveDirectoryRecord {
                id: format!("many-{n}"),
                parent_id: Some("source".into()),
                name: format!("Folder {n}"),
                backing_logical_channel_id: None,
                created_at: 1,
                updated_at: 1,
                trashed_at: None,
            });
        }
        assert!(validate(&snapshot, &req).is_err());
        let mut cyclic = drive_snapshot(&db).unwrap();
        cyclic
            .directories
            .iter_mut()
            .find(|d| d.id == "source")
            .unwrap()
            .parent_id = Some("child".into());
        let mut changed = req;
        changed.expected_source_parent_id = Some("child".into());
        assert!(validate(&cyclic, &changed).is_err());
    }
    #[test]
    fn invalid_subtree_name_and_ambiguous_siblings_are_rejected() {
        let db = fixture();
        db.execute("UPDATE drive_directories SET name='NUL' WHERE id='child'")
            .unwrap();
        assert!(
            !preview(&db, &request(&db, Some("dest"), "Copy"))
                .unwrap()
                .ready
        );
        db.execute("UPDATE drive_directories SET name='Docs' WHERE id='child'; INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES ('sibling','source','docs',1,1);").unwrap();
        assert!(
            !preview(&db, &request(&db, Some("dest"), "Copy"))
                .unwrap()
                .ready
        );
    }
}
