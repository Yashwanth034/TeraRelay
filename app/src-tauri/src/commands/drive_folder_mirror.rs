//! Additive, preview-gated mirror between two existing Drive folder trees.
//! Never removes or overwrites a target. Copies completed content by independent
//! metadata references, without reading/re-uploading any Telegram file bytes.
use crate::commands::drive_file_copy::{fresh_id, insert_copy, source_kind};
use crate::commands::drive_metadata::{
    create_drive_directory, drive_snapshot, mark_drive_dirty, portable_name_key,
    validate_drive_name, DriveDirectoryRecord, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_FOLDERS: usize = 512;
const MAX_FILES: usize = 2000;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorRequest {
    pub revision: i64,
    pub source_id: String,
    pub target_id: String,
}
#[derive(Debug, Serialize)]
pub struct MirrorPreview {
    pub revision: i64,
    pub ready: bool,
    pub add_folders: usize,
    pub add_files: usize,
    pub matching_files: usize,
    pub added_logical_bytes: u64,
    pub examples: Vec<String>,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct MirrorApplied {
    pub revision: i64,
    pub dirty: bool,
    pub folders_added: usize,
    pub files_added: usize,
    pub files_matched: usize,
}
#[derive(Clone)]
struct FolderStep {
    relative: String,
    parent_relative: String,
    name: String,
}
#[derive(Clone)]
struct FileStep {
    parent_relative: String,
    file: DriveFileRecord,
}
struct Tree {
    folders: HashMap<String, String>, // portable relative path -> folder ID
    names: HashMap<String, String>,   // portable relative path -> exact folder name
    order: Vec<FolderStep>,           // parent first, excludes existing root
    files: HashMap<String, DriveFileRecord>, // portable relative file path -> authoritative file
}
struct Plan {
    existing: HashMap<String, String>,
    folders: Vec<FolderStep>,
    files: Vec<FileStep>,
    matched: usize,
    bytes: u64,
    examples: Vec<String>,
}
pub(crate) type Commit = (
    MirrorApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);

fn id_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn within_ancestry(
    start: &str,
    ancestor: &str,
    dirs: &HashMap<&str, &DriveDirectoryRecord>,
) -> Result<bool, String> {
    let mut seen = HashSet::new();
    let mut cursor = Some(start);
    while let Some(id) = cursor {
        if !seen.insert(id) || seen.len() > MAX_FOLDERS {
            return Err("A Drive folder has invalid, cyclic or overly deep ancestry".into());
        }
        if id == ancestor {
            return Ok(true);
        }
        let dir = dirs
            .get(id)
            .ok_or("Source/target ancestry references a missing directory")?;
        cursor = dir.parent_id.as_deref();
    }
    Ok(false)
}
fn same_content(a: &DriveFileRecord, b: &DriveFileRecord) -> bool {
    a.storage_version == b.storage_version
        && a.file_id == b.file_id
        && a.total_size == b.total_size
        && a.plaintext_size == b.plaintext_size
        && a.encryption_version == b.encryption_version
        && a.crypto_id == b.crypto_id
        && a.content_fingerprint == b.content_fingerprint
}

/// Full bounded subtree inventory. Any hidden COW origin, missing backing
/// content, symlink, case-fold collision or broken directory aborts the mirror.
fn tree(conn: &sqlite::Connection, snapshot: &DriveSnapshot, root: &str) -> Result<Tree, String> {
    let active: HashMap<&str, &DriveDirectoryRecord> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if !id_valid(root) || !active.contains_key(root) {
        return Err("Selected mirror tree is missing or trashed".into());
    }
    let mut adjacency: HashMap<Option<&str>, Vec<&DriveDirectoryRecord>> = HashMap::new();
    for dir in active.values() {
        adjacency
            .entry(dir.parent_id.as_deref())
            .or_default()
            .push(dir);
    }
    let mut folders = HashMap::from([(String::new(), root.to_owned())]);
    let mut names = HashMap::from([(String::new(), String::new())]);
    let mut order = Vec::new();
    let mut encountered = HashSet::new();
    let mut queue = VecDeque::from([(root.to_string(), String::new(), 0usize)]);
    while let Some((id, parent_path, depth)) = queue.pop_front() {
        if !encountered.insert(id.clone()) || encountered.len() > MAX_FOLDERS || depth > 64 {
            return Err("Mirror tree is cyclic, too deep, or exceeds 512 folders".into());
        }
        let folder = active.get(id.as_str()).ok_or("Missing mirror directory")?;
        let relative = if depth == 0 {
            String::new()
        } else {
            validate_drive_name(&folder.name)?;
            let part = portable_name_key(&folder.name);
            let relative = if parent_path.is_empty() {
                part
            } else {
                format!("{parent_path}/{part}")
            };
            if relative.len() > 4096 || folders.insert(relative.clone(), id.clone()).is_some() {
                return Err("Mirror folder paths collide on a portable filesystem".into());
            }
            names.insert(relative.clone(), folder.name.clone());
            order.push(FolderStep {
                relative: relative.clone(),
                parent_relative: parent_path.clone(),
                name: folder.name.clone(),
            });
            relative
        };
        if let Some(children) = adjacency.get(&Some(id.as_str())) {
            for child in children {
                queue.push_back((child.id.clone(), relative.clone(), depth + 1));
            }
        }
    }
    // A COW entry is hidden from drive_snapshot, so check its raw origin.
    let mut cow = conn
        .prepare(
            "SELECT v.directory_id, l.directory_id FROM drive_pending_files p
         LEFT JOIN drive_entries_v2 v ON v.entry_id=p.base_entry_id AND v.trashed_at IS NULL
         LEFT JOIN drive_file_entries l ON l.entry_id=p.base_entry_id AND l.trashed_at IS NULL
         WHERE p.base_entry_id IS NOT NULL",
        )
        .map_err(|e| e.to_string())?;
    while let sqlite::State::Row = cow.next().map_err(|e| e.to_string())? {
        for column in 0..2 {
            let value = cow
                .read::<sqlite::Value, _>(column)
                .map_err(|e| e.to_string())?;
            if let sqlite::Value::String(parent) = value {
                if encountered.contains(&parent) {
                    return Err("Mirror tree has a hidden unfinished copy-on-write source".into());
                }
            }
        }
    }
    drop(cow);
    if snapshot.pending.iter().any(|p| {
        p.directory_id
            .as_deref()
            .is_some_and(|id| encountered.contains(id))
    }) {
        return Err("Mirror tree has unfinished uploads or staged edits".into());
    }
    if snapshot.symlinks.iter().any(|l| {
        l.trashed_at.is_none()
            && l.directory_id
                .as_deref()
                .is_some_and(|id| encountered.contains(id))
    }) {
        return Err("Mirror tree contains symbolic links that cannot be mirrored".into());
    }
    let mut raw = conn
        .prepare(
            "SELECT e.entry_id,e.directory_id,EXISTS(
            SELECT 1 FROM logical_files f JOIN logical_channels c
              ON c.logical_id=f.logical_channel_id
             WHERE f.file_id=e.file_id AND f.status='complete'),
         EXISTS(SELECT 1 FROM drive_entries_v2 shadow
                 WHERE shadow.entry_id=e.entry_id AND shadow.trashed_at IS NOT NULL)
         FROM drive_file_entries e
         WHERE e.trashed_at IS NULL AND NOT EXISTS(
           SELECT 1 FROM drive_entries_v2 v
            WHERE v.entry_id=e.entry_id AND v.trashed_at IS NULL)
         UNION ALL
         SELECT e.entry_id,e.directory_id,EXISTS(
             SELECT 1 FROM drive_objects o WHERE o.object_id=e.object_id),0
         FROM drive_entries_v2 e WHERE e.trashed_at IS NULL",
        )
        .map_err(|e| e.to_string())?;
    let mut raw_ids = HashSet::new();
    while let sqlite::State::Row = raw.next().map_err(|e| e.to_string())? {
        let id = raw.read::<String, _>(0).map_err(|e| e.to_string())?;
        let parent = raw.read::<sqlite::Value, _>(1).map_err(|e| e.to_string())?;
        let ready = raw.read::<i64, _>(2).map_err(|e| e.to_string())? == 1;
        let tombstoned_v2 = raw.read::<i64, _>(3).map_err(|e| e.to_string())? != 0;
        if let sqlite::Value::String(parent) = parent {
            if encountered.contains(&parent) {
                // Never resurrect a legacy fallback when its authoritative V2
                // generation was explicitly trashed, even if backing vanished.
                if tombstoned_v2 {
                    return Err("Mirror tree has a tombstoned authoritative V2 file".into());
                }
                if !ready || !raw_ids.insert(id) {
                    return Err(
                        "Mirror tree has unavailable or ambiguous completed-file backing".into(),
                    );
                }
                if raw_ids.len() > MAX_FILES {
                    return Err("Mirror tree exceeds the 2,000 completed-file limit".into());
                }
            }
        }
    }
    drop(raw);
    let mut authority: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snapshot.files {
        if !file
            .directory_id
            .as_deref()
            .is_some_and(|id| encountered.contains(id))
        {
            continue;
        }
        authority
            .entry(file.entry_id.as_str())
            .and_modify(|prior| {
                if file.storage_version > prior.storage_version {
                    *prior = file;
                }
            })
            .or_insert(file);
    }
    if authority.len() != raw_ids.len() || authority.keys().any(|id| !raw_ids.contains(*id)) {
        return Err("Mirror snapshot omits at least one raw active file in the tree".into());
    }
    let folder_paths: HashMap<&str, &str> = folders
        .iter()
        .map(|(path, id)| (id.as_str(), path.as_str()))
        .collect();
    let mut files = HashMap::new();
    for file in authority.into_values() {
        if file.inode_id.is_some() || validate_drive_name(&file.display_name).is_err() {
            return Err("Mirror source includes unsafe file names or shared inode aliases".into());
        }
        let parent = folder_paths
            .get(file.directory_id.as_deref().ok_or("File lacks parent")?)
            .ok_or("Mirror file has unresolved folder parent")?;
        let leaf = portable_name_key(&file.display_name);
        let relative = if parent.is_empty() {
            leaf
        } else {
            format!("{parent}/{leaf}")
        };
        if relative.len() > 4096
            || files.insert(relative.clone(), file.clone()).is_some()
            || folders.contains_key(&relative)
        {
            return Err("Mirror tree contains colliding files or directory names".into());
        }
    }
    Ok(Tree {
        folders,
        names,
        order,
        files,
    })
}

fn plan(conn: &sqlite::Connection, request: &MirrorRequest) -> Result<Plan, String> {
    let snap = drive_snapshot(conn)?;
    if snap.revision != request.revision {
        return Err("Drive changed after mirror selection; refresh and preview again".into());
    }
    if !id_valid(&request.source_id)
        || !id_valid(&request.target_id)
        || request.source_id == request.target_id
    {
        return Err("Choose two different existing Drive folder trees".into());
    }
    let dirs: HashMap<&str, &DriveDirectoryRecord> = snap
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if within_ancestry(&request.source_id, &request.target_id, &dirs)?
        || within_ancestry(&request.target_id, &request.source_id, &dirs)?
    {
        return Err("Source and backup destinations must be unrelated folder trees".into());
    }
    let source = tree(conn, &snap, &request.source_id)?;
    let target = tree(conn, &snap, &request.target_id)?;
    let mut new_folders = Vec::new();
    let existing = target.folders.clone();
    // Root always maps to an already-existing backup root.
    for folder in &source.order {
        if target.files.contains_key(&folder.relative) {
            return Err(format!(
                "Mirror conflict: target file blocks source folder {}",
                folder.name
            ));
        }
        if existing.contains_key(&folder.relative) {
            if target.names.get(&folder.relative) != Some(&folder.name) {
                return Err(format!(
                    "Mirror conflict: folder differs by portable case at {}",
                    folder.name
                ));
            }
        } else {
            new_folders.push(folder.clone());
        }
    }
    let mut new_files = Vec::new();
    let mut matched = 0usize;
    let mut bytes = 0u64;
    let mut examples = Vec::new();
    let mut relative_files = source.files.into_iter().collect::<Vec<_>>();
    relative_files.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, file) in relative_files {
        if target.folders.contains_key(&path) {
            return Err(format!(
                "Mirror conflict: target folder occupies source file {path}"
            ));
        }
        if let Some(other) = target.files.get(&path) {
            if file.display_name != other.display_name || !same_content(&file, other) {
                return Err(format!(
                    "Mirror conflict: target file contents differ at {path}; nothing overwritten"
                ));
            }
            matched += 1;
        } else {
            let parent_relative = path
                .rsplit_once('/')
                .map(|(p, _)| p.to_owned())
                .unwrap_or_default();
            if !source.folders.contains_key(&parent_relative) {
                return Err("Mirror file has missing source parent".into());
            }
            bytes = bytes
                .checked_add(file.plaintext_size.unwrap_or(file.total_size))
                .ok_or("Mirrored logical bytes exceed numeric limit")?;
            if examples.len() < 25 {
                examples.push(path);
            }
            new_files.push(FileStep {
                parent_relative,
                file,
            });
        }
    }
    for folder in &new_folders {
        if examples.len() < 25 {
            examples.push(format!("{}/", folder.relative));
        }
    }
    Ok(Plan {
        existing,
        folders: new_folders,
        files: new_files,
        matched,
        bytes,
        examples,
    })
}
pub(crate) fn preview(
    conn: &sqlite::Connection,
    request: &MirrorRequest,
) -> Result<MirrorPreview, String> {
    let snap = drive_snapshot(conn)?;
    Ok(match plan(conn, request) {
        Ok(p) => MirrorPreview {
            revision: snap.revision,
            ready: true,
            add_folders: p.folders.len(),
            add_files: p.files.len(),
            matching_files: p.matched,
            added_logical_bytes: p.bytes,
            examples: p.examples,
            problems: vec![],
        },
        Err(problem) => MirrorPreview {
            revision: snap.revision,
            ready: false,
            add_folders: 0,
            add_files: 0,
            matching_files: 0,
            added_logical_bytes: 0,
            examples: vec![],
            problems: vec![problem],
        },
    })
}
pub(crate) fn apply(conn: &sqlite::Connection, request: &MirrorRequest) -> Result<Commit, String> {
    let before = drive_snapshot(conn)?;
    let mut plan = plan(conn, request)?;
    if plan.folders.is_empty() && plan.files.is_empty() {
        return Err("Drive backup mirror is already up to date; no changes to apply".into());
    }
    conn.execute("SAVEPOINT drive_incremental_mirror")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let mut locations = Vec::new();
        let mut inode_ids = Vec::new();
        let mut created_dirs = Vec::new();
        let mut created_files = Vec::new();
        for folder in &plan.folders {
            let parent = plan
                .existing
                .get(&folder.parent_relative)
                .ok_or("Missing mirrored destination parent; aborting")?
                .clone();
            let new = create_drive_directory(conn, Some(parent.as_str()), &folder.name)?;
            plan.existing
                .insert(folder.relative.clone(), new.id.clone());
            locations.push((Some(parent.clone()), folder.name.clone()));
            inode_ids.push(("dir".into(), new.id.clone()));
            created_dirs.push((new.id, folder.relative.clone(), parent, folder.name.clone()));
        }
        for step in &plan.files {
            let parent = plan
                .existing
                .get(&step.parent_relative)
                .ok_or("Missing mirrored file parent")?;
            let kind = source_kind(conn, &step.file)?;
            let id = fresh_id(conn)?;
            insert_copy(
                conn,
                kind,
                &id,
                &step.file,
                Some(parent),
                &step.file.display_name,
            )?;
            locations.push((Some(parent.clone()), step.file.display_name.clone()));
            inode_ids.push(("file".into(), id.clone()));
            created_files.push((id, parent.clone(), step.file.clone()));
        }
        if plan.folders.is_empty() {
            mark_drive_dirty(conn)?;
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= before.revision {
            return Err("Mirror did not advance Drive revision; rollback".into());
        }
        for (id, _, parent, name) in &created_dirs {
            if !after.directories.iter().any(|d| {
                d.id == *id
                    && d.parent_id.as_deref() == Some(parent.as_str())
                    && d.name == *name
                    && d.trashed_at.is_none()
            }) {
                return Err("A mirrored folder was ignored or changed; rollback".into());
            }
        }
        for (id, parent, source) in &created_files {
            if !after.files.iter().any(|file| {
                file.entry_id == *id
                    && file.inode_id.is_none()
                    && file.directory_id.as_deref() == Some(parent)
                    && file.display_name == source.display_name
                    && same_content(file, source)
            }) {
                return Err("A mirrored completed file was ignored or changed; rollback".into());
            }
        }
        Ok((
            MirrorApplied {
                revision: after.revision,
                dirty: after.dirty,
                folders_added: created_dirs.len(),
                files_added: created_files.len(),
                files_matched: plan.matched,
            },
            locations,
            inode_ids,
        ))
    })();
    match outcome {
        Ok(result) => {
            if let Err(e) = conn.execute("RELEASE drive_incremental_mirror") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_incremental_mirror; RELEASE drive_incremental_mirror",
                );
                return Err(e.to_string());
            }
            Ok(result)
        }
        Err(problem) => {
            conn.execute("ROLLBACK TO drive_incremental_mirror; RELEASE drive_incremental_mirror")
                .map_err(|e| format!("{problem}; rollback failed: {e}"))?;
            Err(problem)
        }
    }
}
#[tauri::command]
pub fn cmd_drive_folder_mirror_preview(
    db: State<'_, DbConnection>,
    request: MirrorRequest,
) -> Result<MirrorPreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_owned())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_folder_mirror_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: MirrorRequest,
) -> Result<MirrorApplied, String> {
    let (result, entries, inodes) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_owned())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&entries, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&entries, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(err) = crate::drive_windows::refresh() {
        log::warn!("Incremental Drive mirror committed but Windows refresh failed: {err}");
    }
    #[cfg(target_os = "macos")]
    if let Err(err) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Incremental Drive mirror committed but macOS refresh failed: {err}");
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
               first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,
               mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
               storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
               status TEXT NOT NULL DEFAULT 'complete');",
        ).unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES
              ('src',NULL,'Project',1,1),
              ('docs','src','Docs',1,1),
              ('dest',NULL,'Backup',1,1);
             INSERT INTO logical_channels
              (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
              VALUES('chan',900,'qa','owner',1,1,1);
             INSERT INTO logical_files
              (file_id,logical_channel_id,first_message_id,display_name,total_size,
               mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
              VALUES('data','chan',7,'first.txt',14,'text/plain',1,'h1',1,1,'complete');
             INSERT INTO drive_file_entries
              (entry_id,file_id,directory_id,display_name,created_at,updated_at)
              VALUES('file','data','docs','first.txt',1,1);",
        )
        .unwrap();
        db
    }
    fn request(db: &sqlite::Connection) -> MirrorRequest {
        MirrorRequest {
            revision: drive_snapshot(db).unwrap().revision,
            source_id: "src".into(),
            target_id: "dest".into(),
        }
    }
    #[test]
    fn first_mirror_copies_only_missing_dirs_and_files_then_is_noop() {
        let db = fixture();
        let r = request(&db);
        let p = preview(&db, &r).unwrap();
        assert!(p.ready);
        assert_eq!(p.add_folders, 1);
        assert_eq!(p.add_files, 1);
        assert_eq!(p.added_logical_bytes, 14);
        let (out, _, _) = apply(&db, &r).unwrap();
        assert_eq!(out.folders_added, 1);
        assert_eq!(out.files_added, 1);
        let next = request(&db);
        let p = preview(&db, &next).unwrap();
        assert!(p.ready);
        assert_eq!((p.add_folders, p.add_files, p.matching_files), (0, 0, 1));
        assert!(apply(&db, &next).is_err());
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.files.len(), 2);
        assert_ne!(snap.files[0].entry_id, snap.files[1].entry_id);
        assert_eq!(snap.files[0].file_id, snap.files[1].file_id);
    }
    #[test]
    fn second_mirror_adds_one_new_file_but_does_not_duplicate_old() {
        let db = fixture();
        apply(&db, &request(&db)).unwrap();
        db.execute(
            "INSERT INTO logical_files
              (file_id,logical_channel_id,first_message_id,display_name,total_size,
               mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
              VALUES('second-data','chan',8,'second.txt',8,'text/plain',1,'h2',1,1,'complete');
             INSERT INTO drive_file_entries
              (entry_id,file_id,directory_id,display_name,created_at,updated_at)
              VALUES('second-file','second-data','docs','second.txt',1,1);",
        )
        .unwrap();
        let req = request(&db);
        let p = preview(&db, &req).unwrap();
        assert_eq!(p.add_files, 1);
        assert_eq!(p.matching_files, 1);
        assert_eq!(p.add_folders, 0);
        let (out, _, _) = apply(&db, &req).unwrap();
        assert_eq!(out.files_added, 1);
        assert_eq!(drive_snapshot(&db).unwrap().files.len(), 4);
    }
    #[test]
    fn different_existing_content_is_conflict_not_overwritten() {
        let db = fixture();
        apply(&db, &request(&db)).unwrap();
        db.execute(
            "UPDATE drive_file_entries SET file_id='changed'
             WHERE entry_id IN (
                 SELECT e.entry_id FROM drive_file_entries e
                 JOIN drive_directories d ON d.id=e.directory_id
                 WHERE d.name='Docs' AND d.id!='docs');",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }
    #[test]
    fn injected_failure_rolls_back_all_new_folders_and_files() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_mirror_abort BEFORE INSERT ON drive_file_entries
             WHEN NEW.entry_id != 'file' BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.revision, r.revision);
        assert_eq!(snap.directories.len(), 3);
        assert_eq!(snap.files.len(), 1);
    }
    #[test]
    fn pending_source_and_overlapping_roots_fail_before_mutation() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_pending_files
              (id,directory_id,display_name,staging_path,size,format_version,
               backing_channel_id,created_at,updated_at)
             VALUES('stage','docs','unfinished.txt','not-read',1,1,900,1,1);",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
        let r = MirrorRequest {
            revision: drive_snapshot(&db).unwrap().revision,
            source_id: "src".into(),
            target_id: "docs".into(),
        };
        assert!(!preview(&db, &r).unwrap().ready);
    }
    #[test]
    fn hidden_missing_backing_refuses_incomplete_mirror() {
        let db = fixture();
        db.execute("UPDATE logical_files SET status='missing' WHERE file_id='data';")
            .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }

    #[test]
    fn authoritative_v2_shadows_legacy_and_mirrors_without_upload() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_objects
                (object_id,plaintext_size,encryption_version,crypto_id,
                 content_fingerprint,block_count,created_at)
             VALUES ('v2-object',14,0,NULL,'v2-checksum',0,1);
             INSERT INTO drive_entries_v2
                (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at)
             VALUES ('file','v2-object',NULL,'docs','first.txt',1,2);",
        )
        .unwrap();
        let p = preview(&db, &request(&db)).unwrap();
        assert!(p.ready);
        assert_eq!((p.add_folders, p.add_files), (1, 1));
        apply(&db, &request(&db)).unwrap();
        let snap = drive_snapshot(&db).unwrap();
        let copied = snap
            .files
            .iter()
            .find(|f| f.directory_id.as_deref() != Some("docs") && f.display_name == "first.txt")
            .unwrap();
        assert_eq!(copied.storage_version, 2);
        assert_eq!(copied.file_id, "v2-object");
        assert_ne!(copied.entry_id, "file");
        let second = preview(&db, &request(&db)).unwrap();
        assert!(second.ready);
        assert_eq!(
            (second.add_folders, second.add_files, second.matching_files),
            (0, 0, 1)
        );
    }

    #[test]
    fn ignored_file_insert_rolls_back_new_folder_and_dirty_revision() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_mirror_ignore BEFORE INSERT ON drive_file_entries
             BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        let after = drive_snapshot(&db).unwrap();
        assert_eq!(after.revision, r.revision);
        assert_eq!(after.directories.len(), 3);
        assert_eq!(after.files.len(), 1);
    }

    #[test]
    fn hidden_copy_on_write_origin_blocks_mirror_even_if_pending_elsewhere() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_pending_files
                (id,directory_id,display_name,staging_path,size,format_version,
                 backing_channel_id,base_entry_id,created_at,updated_at)
             VALUES ('hidden-stage',NULL,'other.txt','unused',0,1,900,'file',1,1);",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }

    #[test]
    fn symlink_and_file_directory_collision_block_without_mutation() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_symlinks
                (id,directory_id,display_name,target,created_at,updated_at)
             VALUES ('link','docs','alias','target.txt',1,1);",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);

        let other = fixture();
        other
            .execute(
                "INSERT INTO drive_file_entries
                (entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES ('collision','data','dest','Docs',1,1);",
            )
            .unwrap();
        let r = request(&other);
        assert!(!preview(&other, &r).unwrap().ready);
        assert!(apply(&other, &r).is_err());
        assert_eq!(drive_snapshot(&other).unwrap().revision, r.revision);
    }

    #[test]
    fn stale_revision_fails_closed_with_no_mutation() {
        let db = fixture();
        let mut stale = request(&db);
        stale.revision -= 1;
        assert!(!preview(&db, &stale).unwrap().ready);
        assert!(apply(&db, &stale).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 3);
    }

    #[test]
    fn tombstoned_v2_blocks_stale_legacy_fallback_without_resurrection() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_objects
                (object_id,plaintext_size,encryption_version,crypto_id,
                 content_fingerprint,block_count,created_at)
             VALUES ('tombstone-object',14,0,NULL,'content',0,1);
             INSERT INTO drive_entries_v2
                (entry_id,object_id,inode_id,directory_id,display_name,
                 created_at,updated_at,trashed_at)
             VALUES ('file','tombstone-object',NULL,'docs','first.txt',1,2,9);",
        )
        .unwrap();
        let r = request(&db);
        let checked = preview(&db, &r).unwrap();
        assert!(!checked.ready);
        assert!(checked.problems.iter().any(|p| p.contains("tombstoned")));
        assert!(apply(&db, &r).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 3);
    }
}
