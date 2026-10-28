//! Atomic creation of a user-defined, empty Drive folder hierarchy.
//! Every intermediate folder must be specified. Preview and commit share the
//! same revision-gated plan and never touch files or Telegram objects.
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

const MAX_FOLDERS: usize = 512;
const MAX_DEPTH: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PREVIEW: usize = 80;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderBlueprint {
    pub revision: i64,
    pub destination_parent_id: Option<String>,
    /// Slash-delimited relative paths, not absolute host filesystem paths.
    /// Every parent must appear as a separate entry in this same blueprint.
    pub paths: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BlueprintPreview {
    pub revision: i64,
    pub ready: bool,
    pub count: usize,
    pub paths: Vec<String>,
    pub truncated: bool,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BlueprintApplied {
    pub created: usize,
    pub revision: i64,
    pub dirty: bool,
}
#[derive(Clone)]
struct Step {
    path: String,
    parent_path: Option<String>,
    name: String,
}
type Commit = (
    BlueprintApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn validate(snapshot: &DriveSnapshot, req: &FolderBlueprint) -> Result<Vec<Step>, Vec<String>> {
    if req.revision != snapshot.revision {
        return Err(vec![
            "Drive changed after the blueprint was prepared. Refresh first.".into(),
        ]);
    }
    if req.paths.is_empty() || req.paths.len() > MAX_FOLDERS {
        return Err(vec!["Blueprint must contain 1–512 new directories.".into()]);
    }
    let folders: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut problems = Vec::new();
    if let Some(parent) = req.destination_parent_id.as_deref() {
        if !valid_id(parent) || !folders.contains_key(parent) {
            problems.push("Destination parent is unavailable or trashed.".into());
        } else {
            let mut cursor = Some(parent);
            let mut visited = HashSet::new();
            while let Some(id) = cursor {
                if !visited.insert(id) || visited.len() > 512 {
                    problems.push("Destination ancestry is cyclic or too deep.".into());
                    break;
                }
                cursor = folders.get(id).and_then(|d| d.parent_id.as_deref());
                if cursor.is_some_and(|id| !folders.contains_key(id)) {
                    problems.push("Destination ancestry contains a missing folder.".into());
                    break;
                }
            }
        }
    }
    let mut provided = HashSet::new();
    let mut paths = Vec::new();
    for (index, path) in req.paths.iter().enumerate() {
        let row = index + 1;
        if path.is_empty()
            || path.len() > MAX_PATH_BYTES
            || path.starts_with('/')
            || path.ends_with('/')
            || path.contains('\\')
        {
            problems.push(format!(
                "Row {row}: paths must be relative and at most 4096 bytes."
            ));
            continue;
        }
        let components: Vec<_> = path.split('/').collect();
        if components.len() > MAX_DEPTH
            || components
                .iter()
                .any(|part| validate_drive_name(part).is_err())
        {
            problems.push(format!(
                "Row {row}: invalid folder name or hierarchy deeper than 64 levels."
            ));
            continue;
        }
        let portable = components
            .iter()
            .map(|part| portable_name_key(part))
            .collect::<Vec<_>>()
            .join("/");
        if !provided.insert(portable) {
            problems.push(format!(
                "Row {row}: duplicate or portable-equivalent folder path."
            ));
            continue;
        }
        let parent_path = path.rsplit_once('/').map(|(parent, _)| parent.to_owned());
        paths.push(Step {
            path: path.clone(),
            name: components.last().unwrap().to_string(),
            parent_path,
        });
    }
    let all_paths: HashSet<_> = paths.iter().map(|p| p.path.as_str()).collect();
    let all_portable: HashSet<_> = paths
        .iter()
        .map(|p| {
            p.path
                .split('/')
                .map(portable_name_key)
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    for step in &paths {
        if let Some(parent) = step.parent_path.as_deref() {
            if !all_paths.contains(parent) {
                // Do not silently reuse differently-cased paths, existing folders or
                // implicit parents. The blueprint must be unambiguous and complete.
                problems.push(format!(
                    "{}: include parent folder {parent} on its own line.",
                    step.path
                ));
            }
        } else {
            let key = portable_name_key(&step.name);
            let destination = &req.destination_parent_id;
            let occupied = snapshot.directories.iter().any(|d| {
                d.trashed_at.is_none()
                    && &d.parent_id == destination
                    && portable_name_key(&d.name) == key
            }) || snapshot.files.iter().any(|f| {
                &f.directory_id == destination && portable_name_key(&f.display_name) == key
            }) || snapshot.pending.iter().any(|p| {
                &p.directory_id == destination && portable_name_key(&p.display_name) == key
            }) || snapshot.symlinks.iter().any(|s| {
                s.trashed_at.is_none()
                    && &s.directory_id == destination
                    && portable_name_key(&s.display_name) == key
            });
            if occupied {
                problems.push(format!(
                    "{}: root name occupied by a folder, file or pending item.",
                    step.path
                ));
            }
        }
    }
    if all_portable.len() != paths.len() {
        problems.push("Blueprint contains portable-equivalent paths.".into());
    }
    if !problems.is_empty() {
        problems.truncate(100);
        return Err(problems);
    }
    paths.sort_by(|a, b| {
        a.path
            .matches('/')
            .count()
            .cmp(&b.path.matches('/').count())
            .then(a.path.cmp(&b.path))
    });
    Ok(paths)
}

fn preview(conn: &sqlite::Connection, req: &FolderBlueprint) -> Result<BlueprintPreview, String> {
    let snap = drive_snapshot(conn)?;
    Ok(match validate(&snap, req) {
        Ok(steps) => BlueprintPreview {
            revision: snap.revision,
            ready: true,
            count: steps.len(),
            paths: steps
                .iter()
                .take(MAX_PREVIEW)
                .map(|s| s.path.clone())
                .collect(),
            truncated: steps.len() > MAX_PREVIEW,
            problems: vec![],
        },
        Err(problems) => BlueprintPreview {
            revision: snap.revision,
            ready: false,
            count: req.paths.len(),
            paths: vec![],
            truncated: false,
            problems,
        },
    })
}

fn apply(conn: &sqlite::Connection, req: &FolderBlueprint) -> Result<Commit, String> {
    let before = drive_snapshot(conn)?;
    let steps = validate(&before, req).map_err(|e| e.join(" "))?;
    conn.execute("SAVEPOINT drive_folder_blueprint")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut entries = Vec::new();
        let mut inodes = Vec::new();
        for step in &steps {
            let parent = match step.parent_path.as_deref() {
                Some(path) => Some(
                    ids.get(path)
                        .ok_or("Missing newly created parent ID")?
                        .clone(),
                ),
                None => req.destination_parent_id.clone(),
            };
            let created = create_drive_directory(conn, parent.as_deref(), &step.name)?;
            ids.insert(step.path.clone(), created.id.clone());
            entries.push((parent, created.name.clone()));
            inodes.push(("dir".to_owned(), created.id));
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= before.revision {
            return Err("Blueprint did not advance Drive metadata revision.".into());
        }
        let index: HashMap<_, &DriveDirectoryRecord> = after
            .directories
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        for (step, (_, id)) in steps.iter().zip(inodes.iter()) {
            let parent = match step.parent_path.as_deref() {
                Some(path) => Some(
                    ids.get(path)
                        .ok_or("Missing created parent mapping")?
                        .as_str(),
                ),
                None => req.destination_parent_id.as_deref(),
            };
            if !matches!(index.get(id.as_str()), Some(d)
                if d.trashed_at.is_none() && d.name == step.name
                    && d.parent_id.as_deref() == parent)
            {
                return Err(
                    "A directory insert was ignored or altered; rolling back the blueprint.".into(),
                );
            }
        }
        Ok((
            BlueprintApplied {
                created: steps.len(),
                revision: after.revision,
                dirty: after.dirty,
            },
            entries,
            inodes,
        ))
    })();
    match outcome {
        Ok(result) => {
            if let Err(err) = conn.execute("RELEASE drive_folder_blueprint") {
                let _ = conn
                    .execute("ROLLBACK TO drive_folder_blueprint; RELEASE drive_folder_blueprint");
                return Err(err.to_string());
            }
            Ok(result)
        }
        Err(reason) => {
            conn.execute("ROLLBACK TO drive_folder_blueprint; RELEASE drive_folder_blueprint")
                .map_err(|e| format!("{reason}; rollback failed: {e}"))?;
            Err(reason)
        }
    }
}

#[tauri::command]
pub fn cmd_drive_folder_blueprint_preview(
    db: State<'_, DbConnection>,
    request: FolderBlueprint,
) -> Result<BlueprintPreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_folder_blueprint_apply(
    app_handle: AppHandle,
    db: State<'_, DbConnection>,
    request: FolderBlueprint,
) -> Result<BlueprintApplied, String> {
    let (result, entries, inodes) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&entries, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&entries, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(err) = crate::drive_windows::refresh() {
        log::warn!("Folder blueprint committed, Windows refresh unavailable: {err}");
    }
    #[cfg(target_os = "macos")]
    if let Err(err) = crate::drive_provider_ipc::signal_changes(&app_handle) {
        log::warn!("Folder blueprint committed, macOS refresh unavailable: {err}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app_handle;
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
        )
        .unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
            VALUES ('existing',NULL,'Existing',1,1);",
        )
        .unwrap();
        db
    }
    fn request(db: &sqlite::Connection, paths: &[&str]) -> FolderBlueprint {
        FolderBlueprint {
            revision: drive_snapshot(db).unwrap().revision,
            destination_parent_id: Some("existing".into()),
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }
    #[test]
    fn deep_blueprint_creates_all_new_directories_under_existing_destination() {
        let db = fixture();
        let req = request(
            &db,
            &[
                "Product/API/Tests",
                "Product",
                "Product/API",
                "Product/Docs",
            ],
        );
        let before = drive_snapshot(&db).unwrap();
        assert_eq!(preview(&db, &req).unwrap().count, 4);
        let (out, names, inodes) = apply(&db, &req).unwrap();
        assert_eq!(out.created, 4);
        assert_eq!(names.len(), 4);
        assert_eq!(inodes.len(), 4);
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.directories.len(), before.directories.len() + 4);
        assert!(after.revision > before.revision);
        let product = after
            .directories
            .iter()
            .find(|d| d.name == "Product")
            .unwrap();
        assert_eq!(product.parent_id.as_deref(), Some("existing"));
        let api = after.directories.iter().find(|d| d.name == "API").unwrap();
        assert_eq!(api.parent_id.as_deref(), Some(product.id.as_str()));
        let tests = after
            .directories
            .iter()
            .find(|d| d.name == "Tests")
            .unwrap();
        assert_eq!(tests.parent_id.as_deref(), Some(api.id.as_str()));
    }
    #[test]
    fn missing_parent_and_duplicate_portable_paths_rejected() {
        let db = fixture();
        assert!(!preview(&db, &request(&db, &["A/B"])).unwrap().ready);
        assert!(!preview(&db, &request(&db, &["A", "a"])).unwrap().ready);
        assert!(!preview(&db, &request(&db, &["A", "A/CON"])).unwrap().ready);
        assert!(!preview(&db, &request(&db, &["A", "A/../B"])).unwrap().ready);
    }
    #[test]
    fn stale_revision_rejected_without_any_mutation() {
        let db = fixture();
        let mut req = request(&db, &["A", "A/B"]);
        req.revision = -1;
        assert!(apply(&db, &req).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 1);
    }
    #[test]
    fn top_level_collisions_reject_without_mkdir() {
        let db = fixture();
        let mut req = request(&db, &["Existing"]);
        req.destination_parent_id = None;
        assert!(!preview(&db, &req).unwrap().ready);
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 1);
    }
    #[test]
    fn partial_failure_rolls_back_all_new_folders() {
        let db = fixture();
        let req = request(&db, &["A", "A/B", "A/B/C"]);
        db.execute(
            "CREATE TRIGGER qa_blueprint_abort BEFORE INSERT ON drive_directories
          WHEN NEW.name = 'C' BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &req).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 1);
        assert_eq!(drive_snapshot(&db).unwrap().revision, req.revision);
    }
    #[test]
    fn ignored_insert_must_not_report_success() {
        let db = fixture();
        let req = request(&db, &["A", "A/B"]);
        db.execute(
            "CREATE TRIGGER qa_blueprint_ignore BEFORE INSERT ON drive_directories
          WHEN NEW.name = 'B' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &req).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 1);
    }
    #[test]
    fn preview_enforces_count_and_depth() {
        let db = fixture();
        let too_many: Vec<String> = (0..513).map(|i| format!("folder-{i}")).collect();
        let mut req = request(&db, &[]);
        req.paths = too_many;
        assert!(!preview(&db, &req).unwrap().ready);
        req.paths = vec![std::iter::repeat_n("X", 65).collect::<Vec<_>>().join("/")];
        assert!(!preview(&db, &req).unwrap().ready);
    }
}
