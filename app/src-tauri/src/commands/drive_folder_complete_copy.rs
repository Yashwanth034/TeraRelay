//! Atomic complete folder copy: nested directories AND immutable completed file references.
//! Incomplete edits and symlinks are rejected rather than silently omitted.
use crate::commands::drive_file_copy::{fresh_id, insert_copy, source_kind};
use crate::commands::drive_metadata::{
    create_drive_directory, drive_snapshot, portable_name_key, validate_drive_name,
    DriveDirectoryRecord, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteCopyRequest {
    pub revision: i64,
    pub source_id: String,
    pub expected_source_name: String,
    pub expected_source_parent_id: Option<String>,
    pub destination_parent_id: Option<String>,
    pub new_root_name: String,
}
#[derive(Debug, Serialize)]
pub struct CompleteCopyPreview {
    pub revision: i64,
    pub ready: bool,
    pub folders: usize,
    pub files: usize,
    pub referenced_bytes: u64,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct CompleteCopyApplied {
    pub revision: i64,
    pub dirty: bool,
    pub root_id: String,
    pub folders: usize,
    pub files: usize,
}
struct FolderStep {
    old_id: String,
    old_parent: Option<String>,
    name: String,
}
struct Plan {
    folders: Vec<FolderStep>,
    files: Vec<DriveFileRecord>,
    bytes: u64,
}
type Commit = (
    CompleteCopyApplied,
    Vec<(Option<String>, String)>,
    Vec<(String, String)>,
);

fn ancestry(id: &str, index: &HashMap<&str, &DriveDirectoryRecord>) -> bool {
    let mut seen = HashSet::new();
    let mut cur = Some(id);
    while let Some(i) = cur {
        if !seen.insert(i) || seen.len() > 512 {
            return false;
        }
        let Some(folder) = index.get(i) else {
            return false;
        };
        cur = folder.parent_id.as_deref();
    }
    true
}

fn build_plan(
    conn: &sqlite::Connection,
    snap: &DriveSnapshot,
    r: &CompleteCopyRequest,
) -> Result<Plan, Vec<String>> {
    if snap.revision != r.revision {
        return Err(vec!["Drive changed; refresh before copying.".into()]);
    }
    let dirs: HashMap<&str, &DriveDirectoryRecord> = snap
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut failures = Vec::new();
    if !matches!(dirs.get(r.source_id.as_str()), Some(d)
        if d.name == r.expected_source_name && d.parent_id == r.expected_source_parent_id)
        || !ancestry(&r.source_id, &dirs)
    {
        failures.push("Source folder is missing, changed or malformed.".into());
    }
    if validate_drive_name(&r.new_root_name).is_err() {
        failures.push("Invalid new top folder name.".into());
    }
    if let Some(dest) = r.destination_parent_id.as_deref() {
        if !ancestry(dest, &dirs) {
            failures.push("Invalid destination ancestry.".into());
        }
        let mut visited = HashSet::new();
        let mut cursor = Some(dest);
        while let Some(id) = cursor {
            if id == r.source_id {
                failures.push("Cannot copy a tree into its own descendants.".into());
                break;
            }
            if !visited.insert(id) {
                break;
            }
            cursor = dirs.get(id).and_then(|d| d.parent_id.as_deref());
        }
    }
    let target_name = portable_name_key(&r.new_root_name);
    let target_parent = &r.destination_parent_id;
    if snap.directories.iter().any(|d| {
        d.trashed_at.is_none()
            && &d.parent_id == target_parent
            && portable_name_key(&d.name) == target_name
    }) || snap.files.iter().any(|f| {
        &f.directory_id == target_parent && portable_name_key(&f.display_name) == target_name
    }) || snap.pending.iter().any(|p| {
        &p.directory_id == target_parent && portable_name_key(&p.display_name) == target_name
    }) || snap.symlinks.iter().any(|s| {
        s.trashed_at.is_none()
            && &s.directory_id == target_parent
            && portable_name_key(&s.display_name) == target_name
    }) {
        failures.push("Destination name already occupied by a Drive entry.".into());
    }
    if !failures.is_empty() {
        return Err(failures);
    }

    let mut children: HashMap<Option<&str>, Vec<&DriveDirectoryRecord>> = HashMap::new();
    for d in dirs.values() {
        children.entry(d.parent_id.as_deref()).or_default().push(d);
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let mut stack = vec![(r.source_id.as_str(), 1usize, r.new_root_name.len())];
    let mut path_bytes: HashMap<&str, usize> = HashMap::new();
    let mut visited = HashSet::new();
    let mut folder_steps = Vec::new();
    while let Some((id, depth, relative_bytes)) = stack.pop() {
        if !visited.insert(id) {
            return Err(vec!["Cyclic folder tree.".into()]);
        }
        if depth > 64 || folder_steps.len() >= 512 || relative_bytes > 4096 {
            return Err(vec!["Copy exceeds 512 folders or 64 levels.".into()]);
        }
        let dir = dirs
            .get(id)
            .ok_or_else(|| vec!["Missing source child.".to_string()])?;
        if validate_drive_name(&dir.name).is_err() {
            return Err(vec!["Invalid source folder name.".into()]);
        }
        let mut names = HashSet::new();
        if let Some(kids) = children.get(&Some(id)) {
            for child in kids {
                if !names.insert(portable_name_key(&child.name)) {
                    return Err(vec!["Source contains ambiguous folder names.".into()]);
                }
            }
        }
        path_bytes.insert(id, relative_bytes);
        folder_steps.push(FolderStep {
            old_id: dir.id.clone(),
            old_parent: dir.parent_id.clone(),
            name: if id == r.source_id {
                r.new_root_name.clone()
            } else {
                dir.name.clone()
            },
        });
        if let Some(kids) = children.get(&Some(id)) {
            for kid in kids.iter().rev() {
                stack.push((
                    &kid.id,
                    depth + 1,
                    relative_bytes
                        .saturating_add(1)
                        .saturating_add(kid.name.len()),
                ));
            }
        }
    }

    // A copy-on-write pending row hides its original completed entry from
    // drive_snapshot. Guard that origin by reading its raw entry parent in
    // BOTH storage generations. Otherwise a tree could silently omit files.
    let mut pending_origins = conn
        .prepare(
            "SELECT legacy.directory_id, v2.directory_id
         FROM drive_pending_files p
         LEFT JOIN drive_file_entries legacy
           ON legacy.entry_id=p.base_entry_id AND legacy.trashed_at IS NULL
         LEFT JOIN drive_entries_v2 v2
           ON v2.entry_id=p.base_entry_id AND v2.trashed_at IS NULL
         WHERE p.base_entry_id IS NOT NULL",
        )
        .map_err(|e| vec![format!("Cannot inspect pending copy origins: {e}")])?;
    while let sqlite::State::Row = pending_origins
        .next()
        .map_err(|e| vec![format!("Cannot inspect pending copy origins: {e}")])?
    {
        for column in 0..2 {
            let v = pending_origins
                .read::<sqlite::Value, _>(column)
                .map_err(|e| vec![format!("Invalid pending origin: {e}")])?;
            match v {
                sqlite::Value::String(folder) if visited.contains(folder.as_str()) => {
                    return Err(vec![
                        "A source file is hidden by an unfinished copy-on-write edit. Recover it first.".into()
                    ]);
                }
                sqlite::Value::Null | sqlite::Value::String(_) => {}
                _ => return Err(vec!["Invalid pending origin folder type.".into()]),
            }
        }
    }
    drop(pending_origins);
    let mut authority: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in &snap.files {
        if !file
            .directory_id
            .as_deref()
            .is_some_and(|d| visited.contains(d))
        {
            continue;
        }
        authority
            .entry(file.entry_id.as_str())
            .and_modify(|old| {
                if file.storage_version > old.storage_version {
                    *old = file;
                }
            })
            .or_insert(file);
    }
    if authority.len() > 2000 {
        return Err(vec!["Copy exceeds 2,000 completed files.".into()]);
    }
    // Do not silently omit an active file whose backing object or legacy
    // status has become unreadable and therefore disappeared from snapshot.
    let mut existing = conn
        .prepare(
            "SELECT entry_id,directory_id FROM drive_file_entries WHERE trashed_at IS NULL
         UNION ALL
         SELECT entry_id,directory_id FROM drive_entries_v2 WHERE trashed_at IS NULL",
        )
        .map_err(|e| vec![format!("Cannot verify full source inventory: {e}")])?;
    while let sqlite::State::Row = existing
        .next()
        .map_err(|e| vec![format!("Cannot verify full source inventory: {e}")])?
    {
        let id = existing
            .read::<String, _>(0)
            .map_err(|e| vec![format!("Invalid source entry identity: {e}")])?;
        let parent = existing
            .read::<sqlite::Value, _>(1)
            .map_err(|e| vec![format!("Invalid source entry parent: {e}")])?;
        let in_tree = match parent {
            sqlite::Value::String(id) => visited.contains(id.as_str()),
            sqlite::Value::Null => false,
            _ => return Err(vec!["Invalid source file parent type.".into()]),
        };
        if in_tree && !authority.contains_key(id.as_str()) {
            return Err(vec![
                "Source contains an unavailable completed-file entry; refusing incomplete copy."
                    .into(),
            ]);
        }
    }
    drop(existing);
    if snap.pending.iter().any(|p| {
        p.directory_id
            .as_deref()
            .is_some_and(|id| visited.contains(id))
            || p.base_entry_id
                .as_deref()
                .is_some_and(|id| authority.contains_key(id))
    }) {
        return Err(vec![
            "Unfinished writes in subtree. Finish/recover before copying.".into(),
        ]);
    }
    if snap.symlinks.iter().any(|s| {
        s.trashed_at.is_none()
            && s.directory_id
                .as_deref()
                .is_some_and(|id| visited.contains(id))
    }) {
        return Err(vec![
            "Subtree contains symlinks; refusing a partial folder copy.".into(),
        ]);
    }
    let mut names = HashSet::new();
    let mut bytes = 0u64;
    for file in authority.values() {
        if file.inode_id.is_some()
            || validate_drive_name(&file.display_name).is_err()
            || file
                .directory_id
                .as_deref()
                .and_then(|id| path_bytes.get(id))
                .is_none_or(|bytes| {
                    bytes
                        .saturating_add(1)
                        .saturating_add(file.display_name.len())
                        > 4096
                })
            || file.entry_id.is_empty()
            || file.entry_id.len() > 256
        {
            return Err(vec![
                "Source contains an unsafe file identity or shared hardlink.".into(),
            ]);
        }
        let key = (
            file.directory_id.clone(),
            portable_name_key(&file.display_name),
        );
        if !names.insert(key.clone())
            || snap.directories.iter().any(|d| {
                d.trashed_at.is_none()
                    && d.parent_id == file.directory_id
                    && portable_name_key(&d.name) == key.1
            })
        {
            return Err(vec!["Source has portable name collisions.".into()]);
        }
        bytes = bytes
            .checked_add(file.plaintext_size.unwrap_or(file.total_size))
            .ok_or_else(|| vec!["Logical file size total overflow.".to_string()])?;
    }
    let mut files: Vec<_> = authority.into_values().cloned().collect();
    files.sort_by(|a, b| {
        a.directory_id
            .cmp(&b.directory_id)
            .then(portable_name_key(&a.display_name).cmp(&portable_name_key(&b.display_name)))
            .then(a.entry_id.cmp(&b.entry_id))
    });
    Ok(Plan {
        folders: folder_steps,
        files,
        bytes,
    })
}
fn preview(
    conn: &sqlite::Connection,
    request: &CompleteCopyRequest,
) -> Result<CompleteCopyPreview, String> {
    let snap = drive_snapshot(conn)?;
    Ok(match build_plan(conn, &snap, request) {
        Ok(p) => CompleteCopyPreview {
            revision: snap.revision,
            ready: true,
            folders: p.folders.len(),
            files: p.files.len(),
            referenced_bytes: p.bytes,
            problems: vec![],
        },
        Err(problems) => CompleteCopyPreview {
            revision: snap.revision,
            ready: false,
            folders: 0,
            files: 0,
            referenced_bytes: 0,
            problems,
        },
    })
}

fn apply(conn: &sqlite::Connection, r: &CompleteCopyRequest) -> Result<Commit, String> {
    let original = drive_snapshot(conn)?;
    let planned = build_plan(conn, &original, r).map_err(|p| p.join(" "))?;
    conn.execute("SAVEPOINT drive_complete_copy")
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let mut directories: HashMap<String, String> = HashMap::new();
        let mut namespace = Vec::new();
        let mut inodes = Vec::new();
        for folder in &planned.folders {
            let target_parent = if folder.old_id == r.source_id {
                r.destination_parent_id.clone()
            } else {
                Some(
                    directories
                        .get(folder.old_parent.as_deref().ok_or("Missing old parent")?)
                        .ok_or("Unmapped copied parent")?
                        .clone(),
                )
            };
            let new = create_drive_directory(conn, target_parent.as_deref(), &folder.name)?;
            directories.insert(folder.old_id.clone(), new.id.clone());
            namespace.push((target_parent, new.name));
            inodes.push(("dir".to_owned(), new.id));
        }
        for file in &planned.files {
            let old_parent = file.directory_id.as_deref().ok_or("File parent missing")?;
            let new_parent = directories
                .get(old_parent)
                .ok_or("Unmapped file destination")?;
            let kind = source_kind(conn, file)?;
            let id = fresh_id(conn)?;
            insert_copy(conn, kind, &id, file, Some(new_parent), &file.display_name)?;
            namespace.push((Some(new_parent.clone()), file.display_name.clone()));
            inodes.push(("file".to_owned(), id));
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= original.revision {
            return Err("Copy did not advance Drive revision.".into());
        }
        for folder in &planned.folders {
            let id = directories
                .get(&folder.old_id)
                .ok_or("Copied folder missing")?;
            let parent = if folder.old_id == r.source_id {
                r.destination_parent_id.as_deref()
            } else {
                Some(
                    directories
                        .get(folder.old_parent.as_deref().ok_or("No parent")?)
                        .ok_or("Missing mapped parent")?
                        .as_str(),
                )
            };
            if !after.directories.iter().any(|d| {
                d.id == *id
                    && d.name == folder.name
                    && d.parent_id.as_deref() == parent
                    && d.trashed_at.is_none()
            }) {
                return Err("Copied directory was ignored or changed; rollback.".into());
            }
        }
        for (file, (_, id)) in planned
            .files
            .iter()
            .zip(inodes.iter().skip(planned.folders.len()))
        {
            let dest = directories
                .get(
                    file.directory_id
                        .as_deref()
                        .ok_or("Missing source directory")?,
                )
                .ok_or("Missing destination")?;
            if !after.files.iter().any(|f| {
                f.entry_id == *id
                    && f.file_id == file.file_id
                    && f.inode_id.is_none()
                    && f.directory_id.as_deref() == Some(dest.as_str())
                    && f.display_name == file.display_name
                    && f.total_size == file.total_size
                    && f.encryption_version == file.encryption_version
                    && f.crypto_id == file.crypto_id
                    && f.content_fingerprint == file.content_fingerprint
            }) {
                return Err("Copied file was ignored or changed; rollback.".into());
            }
        }
        Ok((
            CompleteCopyApplied {
                revision: after.revision,
                dirty: after.dirty,
                root_id: directories
                    .get(&r.source_id)
                    .ok_or("New root missing")?
                    .clone(),
                folders: planned.folders.len(),
                files: planned.files.len(),
            },
            namespace,
            inodes,
        ))
    })();
    match outcome {
        Ok(v) => {
            if let Err(e) = conn.execute("RELEASE drive_complete_copy") {
                let _ =
                    conn.execute("ROLLBACK TO drive_complete_copy; RELEASE drive_complete_copy");
                return Err(e.to_string());
            }
            Ok(v)
        }
        Err(reason) => {
            conn.execute("ROLLBACK TO drive_complete_copy; RELEASE drive_complete_copy")
                .map_err(|e| format!("{reason}; rollback failed: {e}"))?;
            Err(reason)
        }
    }
}

#[tauri::command]
pub fn cmd_drive_complete_copy_preview(
    db: State<'_, DbConnection>,
    request: CompleteCopyRequest,
) -> Result<CompleteCopyPreview, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_complete_copy_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: CompleteCopyRequest,
) -> Result<CompleteCopyApplied, String> {
    let (done, namespace, inodes) = {
        let conn = db
            .lock()
            .map_err(|_| "Drive database unavailable".to_string())?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&namespace, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&namespace, &inodes);
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Completed tree copy committed but Windows refresh unavailable: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Completed tree copy committed but macOS refresh unavailable: {e}");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = &app;
    Ok(done)
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
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at) VALUES
               ('src',NULL,'Projects',1,1),('docs','src','Docs',1,1),
               ('target',NULL,'Destination',1,1);
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
               VALUES('qa',47,'QA','owner',1,1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
               total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
               VALUES('data','qa',9,'report.txt',16,'text/plain',1,'hash',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
               VALUES('entry','data','docs','report.txt',1,1);",
        ).unwrap();
        db
    }
    fn request(db: &sqlite::Connection) -> CompleteCopyRequest {
        CompleteCopyRequest {
            revision: drive_snapshot(db).unwrap().revision,
            source_id: "src".into(),
            expected_source_name: "Projects".into(),
            expected_source_parent_id: None,
            destination_parent_id: Some("target".into()),
            new_root_name: "Copied Projects".into(),
        }
    }
    #[test]
    fn complete_tree_copy_reuses_bytes_with_distinct_ids() {
        let db = fixture();
        let r = request(&db);
        let p = preview(&db, &r).unwrap();
        assert!(p.ready);
        assert_eq!(p.folders, 2);
        assert_eq!(p.files, 1);
        assert_eq!(p.referenced_bytes, 16);
        let (done, entries, inodes) = apply(&db, &r).unwrap();
        assert_eq!(done.folders, 2);
        assert_eq!(done.files, 1);
        assert_eq!(entries.len(), 3);
        assert_eq!(inodes.len(), 3);
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.directories.len(), 5);
        assert_eq!(snap.files.len(), 2);
        assert!(snap
            .files
            .iter()
            .any(|f| f.entry_id == "entry" && f.directory_id.as_deref() == Some("docs")));
        let new_file = snap.files.iter().find(|f| f.entry_id != "entry").unwrap();
        assert_eq!(new_file.file_id, "data");
        assert_eq!(new_file.display_name, "report.txt");
        assert!(new_file.inode_id.is_none());
        assert_ne!(new_file.directory_id.as_deref(), Some("docs"));
    }
    #[test]
    fn authoritative_v2_content_inside_folder_is_copied_once() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_objects
             (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
             VALUES('object-v2',16,0,NULL,'hash-v2',0,1);
             INSERT INTO drive_entries_v2
             (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
             VALUES('entry','object-v2',NULL,'docs','newer.txt',1,2,NULL);",
        ).unwrap();
        let r = request(&db);
        let p = preview(&db, &r).unwrap();
        assert!(p.ready);
        assert_eq!(p.files, 1);
        let (done, _, _) = apply(&db, &r).unwrap();
        assert_eq!(done.files, 1);
        let snapshot = drive_snapshot(&db).unwrap();
        assert_eq!(
            snapshot
                .files
                .iter()
                .filter(|f| f.file_id == "object-v2"
                    && f.entry_id != "entry"
                    && f.display_name == "newer.txt")
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .files
                .iter()
                .filter(|f| f.entry_id != "entry" && f.file_id == "data")
                .count(),
            0
        );
    }
    #[test]
    fn unfinished_file_in_subtree_rejects_entire_copy() {
        let db = fixture();
        db.execute(
            "INSERT INTO drive_pending_files
            (id,directory_id,display_name,staging_path,size,format_version,
             backing_channel_id,created_at,updated_at)
            VALUES('pending','docs','draft.bin','/not-read',1,1,47,1,1)",
        )
        .unwrap();
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }
    #[test]
    fn hidden_cow_base_inside_source_is_not_silently_skipped() {
        let db = fixture();
        // The staging overlay can target a different folder while referring
        // to a completed entry under the copied source. drive_snapshot hides
        // the original, but the raw source ancestry still reveals the alias.
        db.execute(
            "INSERT INTO drive_pending_files
          (id,directory_id,display_name,staging_path,size,format_version,
           backing_channel_id,base_entry_id,created_at,updated_at)
          VALUES('copyonwrite','target','other.txt','not-read',1,1,47,'entry',1,1)",
        )
        .unwrap();
        let current = drive_snapshot(&db).unwrap();
        assert!(current.files.iter().all(|f| f.entry_id != "entry"));
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }
    #[test]
    fn missing_backing_content_is_not_silently_skipped() {
        let db = fixture();
        db.execute("UPDATE logical_files SET status='missing' WHERE file_id='data'")
            .unwrap();
        let current = drive_snapshot(&db).unwrap();
        assert!(current.files.is_empty());
        assert!(!preview(&db, &request(&db)).unwrap().ready);
    }
    #[test]
    fn root_collision_self_descendant_and_stale_revision_reject() {
        let db = fixture();
        let mut r = request(&db);
        r.destination_parent_id = None;
        r.new_root_name = "Destination".into();
        assert!(!preview(&db, &r).unwrap().ready);
        r = request(&db);
        r.destination_parent_id = Some("docs".into());
        assert!(!preview(&db, &r).unwrap().ready);
        r = request(&db);
        r.revision -= 1;
        assert!(apply(&db, &r).is_err());
        assert_eq!(drive_snapshot(&db).unwrap().directories.len(), 3);
    }
    #[test]
    fn injected_late_file_insert_error_rolls_back_all_folders() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_complete_abort BEFORE INSERT ON drive_file_entries
            WHEN NEW.entry_id != 'entry' BEGIN SELECT RAISE(ABORT,'late failure'); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.directories.len(), 3);
        assert_eq!(snap.files.len(), 1);
        assert_eq!(snap.revision, r.revision);
    }
    #[test]
    fn silent_file_insert_ignore_cannot_claim_success() {
        let db = fixture();
        let r = request(&db);
        db.execute(
            "CREATE TRIGGER qa_complete_ignore BEFORE INSERT ON drive_file_entries
            WHEN NEW.entry_id != 'entry' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(apply(&db, &r).is_err());
        let snap = drive_snapshot(&db).unwrap();
        assert_eq!(snap.directories.len(), 3);
        assert_eq!(snap.files.len(), 1);
        assert_eq!(snap.revision, r.revision);
    }
}
