//! Recoverable, all-or-nothing Trash for a *nonempty* Drive folder tree.
//! Exact local journal allows complete, non-renaming restoration of that tree.
//! Never deletes Telegram content or touches local mounted files.
use crate::commands::drive_file_copy::source_kind;
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, restore_drive_trash_item, trash_drive_directory,
    trash_drive_file, validate_drive_name, DriveDirectoryRecord, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_DIRS: usize = 512;
const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 64;
type Changed = Vec<(Option<String>, String)>;
type Inodes = Vec<(String, String)>;
type Commit = (TreeApplied, Changed, Inodes);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeRequest {
    revision: i64,
    root_id: String,
}
#[derive(Debug, Serialize)]
pub struct TreePreview {
    revision: i64,
    ready: bool,
    folders: usize,
    files: usize,
    logical_bytes: u64,
    examples: Vec<String>,
    problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct TreeApplied {
    revision: i64,
    dirty: bool,
    folders: usize,
    files: usize,
}
#[derive(Debug, Serialize)]
pub struct TreeRun {
    root_id: String,
    name: String,
    folder_count: usize,
    file_count: usize,
    created_at: i64,
}
#[derive(Debug, Serialize)]
pub struct TreeRuns {
    revision: i64,
    runs: Vec<TreeRun>,
}
#[derive(Clone)]
struct Member {
    kind: String,
    id: String,
    parent: Option<String>,
    name: String,
    deleted_at: i64,
    storage: i64,
    content_id: Option<String>,
}
struct Plan {
    dirs: Vec<DriveDirectoryRecord>,
    files: Vec<DriveFileRecord>,
    bytes: u64,
    examples: Vec<String>,
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.trim() == s
        && s != "."
        && s != ".."
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn sql_error(e: sqlite::Error) -> String {
    e.to_string()
}
pub(crate) fn init(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_tree_trash_runs (
            root_id TEXT PRIMARY KEY,
            original_name TEXT NOT NULL,
            original_parent_id TEXT,
            created_at INTEGER NOT NULL,
            folder_count INTEGER NOT NULL,
            file_count INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS drive_tree_trash_members (
            root_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('folder','file')),
            entry_id TEXT NOT NULL,
            parent_id TEXT,
            display_name TEXT NOT NULL,
            deleted_at INTEGER NOT NULL,
            storage_version INTEGER NOT NULL,
            content_id TEXT,
            PRIMARY KEY(root_id,kind,entry_id),
            FOREIGN KEY(root_id) REFERENCES drive_tree_trash_runs(root_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_drive_tree_trash_members_root
         ON drive_tree_trash_members(root_id);",
    )
    .map_err(sql_error)
}
fn has_run(conn: &sqlite::Connection, id: &str) -> Result<bool, String> {
    let mut st = conn
        .prepare("SELECT 1 FROM drive_tree_trash_runs WHERE root_id=? LIMIT 1")
        .map_err(sql_error)?;
    st.bind((1, id)).map_err(sql_error)?;
    Ok(st.next().map_err(sql_error)? == sqlite::State::Row)
}
fn read_dirs(snapshot: &DriveSnapshot, id: &str) -> Result<Vec<DriveDirectoryRecord>, String> {
    let active: HashMap<&str, &DriveDirectoryRecord> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if !valid_id(id) || !active.contains_key(id) {
        return Err("Folder to Trash is not active or is unavailable".into());
    }
    // An orphan or cyclic source must not acquire new trash semantics.
    let mut ancestors = HashSet::new();
    let mut cursor = Some(id);
    while let Some(key) = cursor {
        if !ancestors.insert(key) || ancestors.len() > MAX_DIRS {
            return Err("Folder ancestor chain is cyclic or too deep".into());
        }
        let row = active
            .get(key)
            .ok_or("Source has unavailable parent directory")?;
        cursor = row.parent_id.as_deref();
    }
    let mut children: HashMap<Option<&str>, Vec<&DriveDirectoryRecord>> = HashMap::new();
    for dir in active.values() {
        children
            .entry(dir.parent_id.as_deref())
            .or_default()
            .push(dir);
    }
    let mut seen = HashSet::new();
    let mut names = HashSet::new();
    let mut queue = VecDeque::from([(id.to_owned(), String::new(), 0usize)]);
    let mut output = Vec::new();
    while let Some((next, path, depth)) = queue.pop_front() {
        if !seen.insert(next.clone()) || seen.len() > MAX_DIRS || depth > MAX_DEPTH {
            return Err("Folder tree is cyclic, too deep, or exceeds 512 folders".into());
        }
        let folder = active
            .get(next.as_str())
            .ok_or("Subfolder became unavailable")?;
        validate_drive_name(&folder.name)?;
        let relative = if path.is_empty() {
            folder.name.clone()
        } else {
            format!("{path}/{}", folder.name)
        };
        if relative.len() > 4096
            || !names.insert((folder.parent_id.clone(), portable_name_key(&folder.name)))
        {
            return Err("Folder tree has colliding or overlong portable paths".into());
        }
        output.push((**folder).clone());
        if let Some(nested) = children.get(&Some(next.as_str())) {
            for child in nested {
                queue.push_back((child.id.clone(), relative.clone(), depth + 1));
            }
        }
    }
    Ok(output)
}
fn tree(conn: &sqlite::Connection, request: &TreeRequest) -> Result<Plan, String> {
    let snap = drive_snapshot(conn)?;
    if snap.revision != request.revision {
        return Err("Drive revision changed; reload folders and preview again".into());
    }
    if has_run(conn, &request.root_id)? {
        return Err("This folder already has a pending recursive Trash recovery journal".into());
    }
    let dirs = read_dirs(&snap, &request.root_id)?;
    let ids: HashSet<&str> = dirs.iter().map(|d| d.id.as_str()).collect();
    if snap
        .pending
        .iter()
        .any(|p| p.directory_id.as_deref().is_some_and(|id| ids.contains(id)))
    {
        return Err("Folder tree contains unfinished uploads or pending edits".into());
    }
    if snap.symlinks.iter().any(|s| {
        s.trashed_at.is_none() && s.directory_id.as_deref().is_some_and(|id| ids.contains(id))
    }) {
        return Err("Folder tree contains a symlink; move it separately before Trash".into());
    }
    // Pending copy-on-write originals are hidden from snapshots. Reject
    // even when their pending overlay is outside the selected subtree.
    let mut cow = conn
        .prepare(
            "SELECT v.directory_id,l.directory_id FROM drive_pending_files p
         LEFT JOIN drive_entries_v2 v ON v.entry_id=p.base_entry_id AND v.trashed_at IS NULL
         LEFT JOIN drive_file_entries l ON l.entry_id=p.base_entry_id AND l.trashed_at IS NULL
         WHERE p.base_entry_id IS NOT NULL",
        )
        .map_err(sql_error)?;
    while cow.next().map_err(sql_error)? == sqlite::State::Row {
        for idx in 0..2 {
            if let sqlite::Value::String(parent) =
                cow.read::<sqlite::Value, _>(idx).map_err(sql_error)?
            {
                if ids.contains(parent.as_str()) {
                    return Err("Folder tree contains hidden copy-on-write content".into());
                }
            }
        }
    }
    drop(cow);
    // Audit raw active records independently of the visible snapshot.
    // V2 with a matching entry ID wins; a tombstoned V2 must not allow an
    // older legacy entry to reappear.
    let mut raw = conn
        .prepare(
            "SELECT e.entry_id,e.directory_id,
           EXISTS(SELECT 1 FROM logical_files f JOIN logical_channels c
                    ON c.logical_id=f.logical_channel_id
                    WHERE f.file_id=e.file_id AND f.status='complete'),
           EXISTS(SELECT 1 FROM drive_entries_v2 v
                    WHERE v.entry_id=e.entry_id AND v.trashed_at IS NOT NULL)
         FROM drive_file_entries e WHERE e.trashed_at IS NULL
           AND NOT EXISTS(SELECT 1 FROM drive_entries_v2 v
                          WHERE v.entry_id=e.entry_id AND v.trashed_at IS NULL)
         UNION ALL
         SELECT e.entry_id,e.directory_id,
           EXISTS(SELECT 1 FROM drive_objects o WHERE o.object_id=e.object_id),0
         FROM drive_entries_v2 e WHERE e.trashed_at IS NULL",
        )
        .map_err(sql_error)?;
    let mut raw_ids = HashSet::new();
    while raw.next().map_err(sql_error)? == sqlite::State::Row {
        let entry = raw.read::<String, _>(0).map_err(sql_error)?;
        let parent = raw.read::<sqlite::Value, _>(1).map_err(sql_error)?;
        let ready = raw.read::<i64, _>(2).map_err(sql_error)? != 0;
        let tombstone = raw.read::<i64, _>(3).map_err(sql_error)? != 0;
        if let sqlite::Value::String(id) = parent {
            if ids.contains(id.as_str()) {
                if !ready || tombstone || !raw_ids.insert(entry) {
                    return Err(
                        "Folder tree contains missing, tombstoned or ambiguous backing".into(),
                    );
                }
                if raw_ids.len() > MAX_FILES {
                    return Err("Folder tree exceeds 2,000 completed files".into());
                }
            }
        }
    }
    drop(raw);
    let mut authority: HashMap<&str, &DriveFileRecord> = HashMap::new();
    for file in snap
        .files
        .iter()
        .filter(|f| f.directory_id.as_deref().is_some_and(|id| ids.contains(id)))
    {
        authority
            .entry(file.entry_id.as_str())
            .and_modify(|prior| {
                if file.storage_version > prior.storage_version {
                    *prior = file;
                }
            })
            .or_insert(file);
    }
    if raw_ids.len() != authority.len() || authority.keys().any(|id| !raw_ids.contains(*id)) {
        return Err("Folder tree snapshot omits raw active completed-file entries".into());
    }
    let mut seen = HashSet::new();
    for folder in &dirs {
        if !seen.insert((folder.parent_id.clone(), portable_name_key(&folder.name))) {
            return Err("Folder tree contains portable name collisions".into());
        }
    }
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for file in authority.into_values() {
        validate_drive_name(&file.display_name)?;
        if file.inode_id.is_some() {
            return Err("Shared-inode file cannot be recursively trashed safely".into());
        }
        source_kind(conn, file)?;
        if !seen.insert((
            file.directory_id.clone(),
            portable_name_key(&file.display_name),
        )) {
            return Err("Folder tree has a file/folder name collision".into());
        }
        bytes = bytes
            .checked_add(file.plaintext_size.unwrap_or(file.total_size))
            .ok_or("Folder tree logical byte total overflows")?;
        files.push(file.clone());
    }
    files.sort_by(|a, b| {
        a.directory_id
            .cmp(&b.directory_id)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    let mut examples = dirs
        .iter()
        .take(12)
        .map(|d| format!("{}/", d.name))
        .collect::<Vec<_>>();
    examples.extend(files.iter().take(13).map(|f| f.display_name.clone()));
    examples.truncate(25);
    Ok(Plan {
        dirs,
        files,
        bytes,
        examples,
    })
}
fn preview(conn: &sqlite::Connection, request: &TreeRequest) -> Result<TreePreview, String> {
    init(conn)?;
    let snap = drive_snapshot(conn)?;
    Ok(match tree(conn, request) {
        Ok(p) => TreePreview {
            revision: snap.revision,
            ready: true,
            folders: p.dirs.len(),
            files: p.files.len(),
            logical_bytes: p.bytes,
            examples: p.examples,
            problems: vec![],
        },
        Err(e) => TreePreview {
            revision: snap.revision,
            ready: false,
            folders: 0,
            files: 0,
            logical_bytes: 0,
            examples: vec![],
            problems: vec![e],
        },
    })
}
fn current_row(
    conn: &sqlite::Connection,
    kind: &str,
    id: &str,
    storage: i64,
) -> Result<(Option<String>, String, Option<i64>), String> {
    let table = match (kind, storage) {
        ("folder", 0) => "drive_directories",
        ("file", 1) => "drive_file_entries",
        ("file", 2) => "drive_entries_v2",
        _ => return Err("Unrecognized folder recovery journal member".into()),
    };
    let sql = if kind == "folder" {
        format!("SELECT parent_id,name,trashed_at FROM {table} WHERE id=?")
    } else {
        format!("SELECT directory_id,display_name,trashed_at FROM {table} WHERE entry_id=?")
    };
    let mut s = conn.prepare(&sql).map_err(sql_error)?;
    s.bind((1, id)).map_err(sql_error)?;
    if s.next().map_err(sql_error)? != sqlite::State::Row {
        return Err("Trashed folder tree member has disappeared".into());
    }
    let parent = match s.read::<sqlite::Value, _>(0).map_err(sql_error)? {
        sqlite::Value::Null => None,
        sqlite::Value::String(v) => Some(v),
        _ => return Err("Invalid trashed member parent".into()),
    };
    let name = s.read::<String, _>(1).map_err(sql_error)?;
    let deleted = match s.read::<sqlite::Value, _>(2).map_err(sql_error)? {
        sqlite::Value::Null => None,
        sqlite::Value::Integer(v) => Some(v),
        _ => return Err("Invalid trash timestamp".into()),
    };
    Ok((parent, name, deleted))
}
fn insert_member(
    conn: &sqlite::Connection,
    root: &str,
    kind: &str,
    id: &str,
    storage: i64,
    content_id: Option<&str>,
) -> Result<Member, String> {
    let (parent, name, deleted) = current_row(conn, kind, id, storage)?;
    let deleted_at = deleted.ok_or("Trashed tree member did not enter Recently Deleted")?;
    let mut st = conn
        .prepare(
            "INSERT INTO drive_tree_trash_members
         (root_id,kind,entry_id,parent_id,display_name,deleted_at,storage_version,content_id)
         VALUES (?,?,?,?,?,?,?,?)",
        )
        .map_err(sql_error)?;
    st.bind((1, root)).map_err(sql_error)?;
    st.bind((2, kind)).map_err(sql_error)?;
    st.bind((3, id)).map_err(sql_error)?;
    st.bind((4, parent.as_deref())).map_err(sql_error)?;
    st.bind((5, name.as_str())).map_err(sql_error)?;
    st.bind((6, deleted_at)).map_err(sql_error)?;
    st.bind((7, storage)).map_err(sql_error)?;
    st.bind((8, content_id)).map_err(sql_error)?;
    st.next().map_err(sql_error)?;
    Ok(Member {
        kind: kind.to_owned(),
        id: id.to_owned(),
        parent,
        name,
        deleted_at,
        storage,
        content_id: content_id.map(str::to_owned),
    })
}
fn journal_count(conn: &sqlite::Connection, root: &str) -> Result<usize, String> {
    let mut st = conn
        .prepare("SELECT COUNT(*) FROM drive_tree_trash_members WHERE root_id=?")
        .map_err(sql_error)?;
    st.bind((1, root)).map_err(sql_error)?;
    if st.next().map_err(sql_error)? != sqlite::State::Row {
        return Err("Folder recovery journal could not be verified".into());
    }
    usize::try_from(st.read::<i64, _>(0).map_err(sql_error)?)
        .map_err(|_| "Invalid folder recovery journal count".into())
}
fn apply_trash(conn: &sqlite::Connection, request: &TreeRequest) -> Result<Commit, String> {
    init(conn)?;
    let before = drive_snapshot(conn)?;
    let plan = tree(conn, request)?;
    let root = plan.dirs.first().ok_or("Missing root folder")?;
    conn.execute("SAVEPOINT drive_tree_trash_atomic")
        .map_err(sql_error)?;
    let result = (|| {
        let mut record = conn
            .prepare(
                "INSERT INTO drive_tree_trash_runs
             (root_id,original_name,original_parent_id,created_at,folder_count,file_count)
             VALUES (?,?,?,?,?,?)",
            )
            .map_err(sql_error)?;
        record.bind((1, root.id.as_str())).map_err(sql_error)?;
        record.bind((2, root.name.as_str())).map_err(sql_error)?;
        record
            .bind((3, root.parent_id.as_deref()))
            .map_err(sql_error)?;
        record
            .bind((4, chrono::Utc::now().timestamp_millis()))
            .map_err(sql_error)?;
        record
            .bind((
                5,
                i64::try_from(plan.dirs.len()).map_err(|e| e.to_string())?,
            ))
            .map_err(sql_error)?;
        record
            .bind((
                6,
                i64::try_from(plan.files.len()).map_err(|e| e.to_string())?,
            ))
            .map_err(sql_error)?;
        record.next().map_err(sql_error)?;
        drop(record);
        let mut entries = Vec::new();
        let mut inodes = Vec::new();
        let mut members = Vec::new();
        for f in &plan.files {
            trash_drive_file(conn, &f.entry_id)?;
            members.push(insert_member(
                conn,
                &root.id,
                "file",
                &f.entry_id,
                i64::from(f.storage_version),
                Some(f.file_id.as_str()),
            )?);
            entries.push((f.directory_id.clone(), f.display_name.clone()));
            inodes.push(("file".to_owned(), f.entry_id.clone()));
        }
        // Deleting child folders first honours native nonempty-folder guards.
        for d in plan.dirs.iter().rev() {
            trash_drive_directory(conn, &d.id)?;
            members.push(insert_member(conn, &root.id, "folder", &d.id, 0, None)?);
            entries.push((d.parent_id.clone(), d.name.clone()));
            inodes.push(("dir".to_owned(), d.id.clone()));
        }
        if journal_count(conn, &root.id)? != members.len() {
            return Err("Partial or ignored folder journal insert; rolling back all Trash".into());
        }
        for m in &members {
            let (parent, name, deleted) = current_row(conn, &m.kind, &m.id, m.storage)?;
            if parent != m.parent || name != m.name || deleted != Some(m.deleted_at) {
                return Err("A Drive item changed during recursive Trash; rollback".into());
            }
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty
            || after.revision <= before.revision
            || after
                .directories
                .iter()
                .any(|d| plan.dirs.iter().any(|old| old.id == d.id) && d.trashed_at.is_none())
        {
            return Err("Recursive Trash failed to publish complete folder changes".into());
        }
        Ok((
            TreeApplied {
                revision: after.revision,
                dirty: after.dirty,
                folders: plan.dirs.len(),
                files: plan.files.len(),
            },
            entries,
            inodes,
        ))
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_tree_trash_atomic", result)
}
fn read_members(conn: &sqlite::Connection, root: &str) -> Result<Vec<Member>, String> {
    let mut st = conn
        .prepare(
            "SELECT kind,entry_id,parent_id,display_name,deleted_at,storage_version,content_id
         FROM drive_tree_trash_members WHERE root_id=?",
        )
        .map_err(sql_error)?;
    st.bind((1, root)).map_err(sql_error)?;
    let mut output = Vec::new();
    while st.next().map_err(sql_error)? == sqlite::State::Row {
        let parent = match st.read::<sqlite::Value, _>(2).map_err(sql_error)? {
            sqlite::Value::Null => None,
            sqlite::Value::String(v) => Some(v),
            _ => return Err("Invalid persisted journal parent".into()),
        };
        output.push(Member {
            kind: st.read::<String, _>(0).map_err(sql_error)?,
            id: st.read::<String, _>(1).map_err(sql_error)?,
            parent,
            name: st.read::<String, _>(3).map_err(sql_error)?,
            deleted_at: st.read::<i64, _>(4).map_err(sql_error)?,
            storage: st.read::<i64, _>(5).map_err(sql_error)?,
            content_id: match st.read::<sqlite::Value, _>(6).map_err(sql_error)? {
                sqlite::Value::Null => None,
                sqlite::Value::String(v) => Some(v),
                _ => return Err("Invalid stored content identity".into()),
            },
        });
        if output.len() > MAX_DIRS + MAX_FILES {
            return Err("Recovery journal exceeds safe tree limits".into());
        }
    }
    Ok(output)
}
fn restoration(conn: &sqlite::Connection, request: &TreeRequest) -> Result<Plan, String> {
    init(conn)?;
    let snapshot = drive_snapshot(conn)?;
    if snapshot.revision != request.revision || !valid_id(&request.root_id) {
        return Err("Drive revision changed; refresh and preview recovery again".into());
    }
    let members = read_members(conn, &request.root_id)?;
    if members.is_empty() || !has_run(conn, &request.root_id)? {
        return Err("No durable recursive folder recovery journal remains".into());
    }
    // The original immutable member counts are an independent guard against
    // truncated/deleted journal rows turning partial recovery into success.
    let mut counts = conn
        .prepare(
            "SELECT folder_count,file_count,original_name,original_parent_id
         FROM drive_tree_trash_runs WHERE root_id=?",
        )
        .map_err(sql_error)?;
    counts
        .bind((1, request.root_id.as_str()))
        .map_err(sql_error)?;
    if counts.next().map_err(sql_error)? != sqlite::State::Row {
        return Err("Folder recovery journal header is missing".into());
    }
    let recorded_folders = usize::try_from(counts.read::<i64, _>(0).map_err(sql_error)?)
        .map_err(|_| "Invalid recorded folder count")?;
    let recorded_files = usize::try_from(counts.read::<i64, _>(1).map_err(sql_error)?)
        .map_err(|_| "Invalid recorded file count")?;
    let recorded_name = counts.read::<String, _>(2).map_err(sql_error)?;
    let recorded_parent = match counts.read::<sqlite::Value, _>(3).map_err(sql_error)? {
        sqlite::Value::Null => None,
        sqlite::Value::String(value) => Some(value),
        _ => return Err("Invalid recorded root parent".into()),
    };
    drop(counts);
    if recorded_folders == 0
        || recorded_folders > MAX_DIRS
        || recorded_files > MAX_FILES
        || recorded_folders + recorded_files != members.len()
    {
        return Err("Recursive recovery journal is incomplete or exceeds safe limits".into());
    }
    let mut ids = HashSet::new();
    let mut folders = HashMap::new();
    let mut expected_names = HashSet::new();
    let mut bytes = 0u64;
    for m in &members {
        validate_drive_name(&m.name)?;
        if !valid_id(&m.id)
            || !ids.insert((m.kind.as_str(), m.id.as_str()))
            || !expected_names.insert((m.parent.clone(), portable_name_key(&m.name)))
        {
            return Err("Recovery journal has duplicated identities or paths".into());
        }
        let (parent, name, deleted) = current_row(conn, &m.kind, &m.id, m.storage)?;
        if parent != m.parent || name != m.name || deleted != Some(m.deleted_at) {
            return Err(
                "Recently Deleted member no longer matches original recovery journal".into(),
            );
        }
        if m.kind == "folder" {
            if m.content_id.is_some() {
                return Err("Folder journal contains invalid content identity".into());
            }
            folders.insert(m.id.as_str(), m);
        } else if m.kind == "file" {
            let usable = match m.storage {
                1 => {
                    let mut q = conn
                        .prepare(
                            "SELECT f.total_size,e.file_id FROM drive_file_entries e
                         JOIN logical_files f ON f.file_id=e.file_id AND f.status='complete'
                         JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                         WHERE e.entry_id=? AND e.trashed_at IS NOT NULL AND NOT EXISTS(
                            SELECT 1 FROM drive_entries_v2 v WHERE v.entry_id=e.entry_id)",
                        )
                        .map_err(sql_error)?;
                    q.bind((1, m.id.as_str())).map_err(sql_error)?;
                    if q.next().map_err(sql_error)? == sqlite::State::Row {
                        Some((
                            q.read::<i64, _>(0).map_err(sql_error)?,
                            q.read::<String, _>(1).map_err(sql_error)?,
                        ))
                    } else {
                        None
                    }
                }
                2 => {
                    let mut q = conn
                        .prepare(
                            "SELECT o.plaintext_size,e.object_id FROM drive_entries_v2 e
                         JOIN drive_objects o ON o.object_id=e.object_id
                         WHERE e.entry_id=? AND e.trashed_at IS NOT NULL",
                        )
                        .map_err(sql_error)?;
                    q.bind((1, m.id.as_str())).map_err(sql_error)?;
                    if q.next().map_err(sql_error)? == sqlite::State::Row {
                        Some((
                            q.read::<i64, _>(0).map_err(sql_error)?,
                            q.read::<String, _>(1).map_err(sql_error)?,
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let (size, actual_content) =
                usable.ok_or("Recovered file backing is missing or no longer authoritative")?;
            if m.content_id.as_deref() != Some(actual_content.as_str()) {
                return Err("Trashed file content identity changed since original Trash".into());
            }
            bytes = bytes
                .checked_add(u64::try_from(size).map_err(|_| "Invalid recoverable file size")?)
                .ok_or("Recoverable file sizes overflow")?;
        } else {
            return Err("Invalid kind stored in folder recovery journal".into());
        }
    }
    if folders.len() > MAX_DIRS || members.len() - folders.len() > MAX_FILES {
        return Err("Folder recovery exceeds bounded tree size".into());
    }
    // Verify exactly one root, every descendant belongs to that root, and
    // ordering is independent of SQLite row order.
    let root = folders
        .get(request.root_id.as_str())
        .ok_or("Missing journal root directory")?;
    if root.name != recorded_name || root.parent != recorded_parent {
        return Err("Recovery root no longer matches its original durable journal".into());
    }
    if root.parent.as_deref() == Some(request.root_id.as_str()) {
        return Err("Recursive folder journal has cyclic root".into());
    }
    let active_dirs: HashMap<&str, &DriveDirectoryRecord> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if root
        .parent
        .as_deref()
        .is_some_and(|p| !active_dirs.contains_key(p))
    {
        return Err("Original root parent has been moved to Trash or is missing".into());
    }
    let mut ordered = Vec::new();
    let mut reached = HashSet::new();
    let mut queue = VecDeque::from([(request.root_id.as_str(), 0usize)]);
    while let Some((id, depth)) = queue.pop_front() {
        if !reached.insert(id) || depth > MAX_DEPTH {
            return Err("Recovery tree has invalid ancestry".into());
        }
        let folder = folders.get(id).ok_or("Missing nested recovery folder")?;
        ordered.push((**folder).clone());
        for (child, member) in &folders {
            if member.parent.as_deref() == Some(id) {
                queue.push_back((child, depth + 1));
            }
        }
    }
    if reached.len() != folders.len()
        || members.iter().any(|m| {
            m.kind == "file" && !m.parent.as_deref().is_some_and(|p| folders.contains_key(p))
        })
    {
        return Err("Recovery journal contains detached or foreign children".into());
    }
    // Do not rename collisions automatically. Even active rows inside trashed
    // ancestors must block restoration until manually resolved.
    let mut occupied = HashSet::new();
    for d in snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
    {
        occupied.insert((d.parent_id.clone(), portable_name_key(&d.name)));
    }
    for f in &snapshot.files {
        occupied.insert((f.directory_id.clone(), portable_name_key(&f.display_name)));
    }
    for p in &snapshot.pending {
        occupied.insert((p.directory_id.clone(), portable_name_key(&p.display_name)));
    }
    for l in snapshot.symlinks.iter().filter(|l| l.trashed_at.is_none()) {
        occupied.insert((l.directory_id.clone(), portable_name_key(&l.display_name)));
    }
    if members
        .iter()
        .any(|m| occupied.contains(&(m.parent.clone(), portable_name_key(&m.name))))
    {
        return Err(
            "Original recovery path is occupied; no overwrites or automatic renames".into(),
        );
    }
    let mut examples = members
        .iter()
        .take(25)
        .map(|m| m.name.clone())
        .collect::<Vec<_>>();
    examples.sort();
    // Plan contains authoritative restore order, and file IDs are resolved
    // from the persisted journal during apply, not guessed from snapshots.
    let directories = ordered
        .iter()
        .map(|m| DriveDirectoryRecord {
            id: m.id.clone(),
            parent_id: m.parent.clone(),
            name: m.name.clone(),
            backing_logical_channel_id: None,
            created_at: 0,
            updated_at: 0,
            trashed_at: Some(m.deleted_at),
        })
        .collect();
    let files = Vec::new();
    Ok(Plan {
        dirs: directories,
        files,
        bytes,
        examples,
    })
}
fn restore_apply(conn: &sqlite::Connection, request: &TreeRequest) -> Result<Commit, String> {
    let before = drive_snapshot(conn)?;
    let plan = restoration(conn, request)?;
    let members = read_members(conn, &request.root_id)?;
    let file_count = members.iter().filter(|m| m.kind == "file").count();
    conn.execute("SAVEPOINT drive_tree_restore_atomic")
        .map_err(sql_error)?;
    let result = (|| {
        let mut entries = Vec::new();
        let mut inodes = Vec::new();
        // Parents first, preserving the exact original hierarchy/identity.
        for dir in &plan.dirs {
            let restored = restore_drive_trash_item(conn, "folder", &dir.id)?;
            let (parent, name, deleted) = current_row(conn, "folder", &dir.id, 0)?;
            if restored != dir.name
                || name != dir.name
                || parent != dir.parent_id
                || deleted.is_some()
            {
                return Err(
                    "Native folder restore changed original path; complete rollback".into(),
                );
            }
            entries.push((dir.parent_id.clone(), dir.name.clone()));
            inodes.push(("dir".into(), dir.id.clone()));
        }
        for member in members.iter().filter(|m| m.kind == "file") {
            let restored = restore_drive_trash_item(conn, "file", &member.id)?;
            let (parent, name, deleted) = current_row(conn, "file", &member.id, member.storage)?;
            if restored != member.name
                || name != member.name
                || parent != member.parent
                || deleted.is_some()
            {
                return Err("Native file restore changed original path; complete rollback".into());
            }
            entries.push((member.parent.clone(), member.name.clone()));
            inodes.push(("file".into(), member.id.clone()));
        }
        let mut delete = conn
            .prepare("DELETE FROM drive_tree_trash_members WHERE root_id=?")
            .map_err(sql_error)?;
        delete
            .bind((1, request.root_id.as_str()))
            .map_err(sql_error)?;
        delete.next().map_err(sql_error)?;
        drop(delete);
        let mut del_run = conn
            .prepare("DELETE FROM drive_tree_trash_runs WHERE root_id=?")
            .map_err(sql_error)?;
        del_run
            .bind((1, request.root_id.as_str()))
            .map_err(sql_error)?;
        del_run.next().map_err(sql_error)?;
        drop(del_run);
        if has_run(conn, &request.root_id)? || journal_count(conn, &request.root_id)? != 0 {
            return Err("Recovery journal deletion was ignored; rolling back restore".into());
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= before.revision {
            return Err("Recovery did not publish a newer Drive revision".into());
        }
        Ok((
            TreeApplied {
                revision: after.revision,
                dirty: after.dirty,
                folders: plan.dirs.len(),
                files: file_count,
            },
            entries,
            inodes,
        ))
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_tree_restore_atomic", result)
}
fn restore_preview(
    conn: &sqlite::Connection,
    request: &TreeRequest,
) -> Result<TreePreview, String> {
    init(conn)?;
    let snap = drive_snapshot(conn)?;
    Ok(match restoration(conn, request) {
        Ok(plan) => {
            let files = read_members(conn, &request.root_id)?
                .iter()
                .filter(|m| m.kind == "file")
                .count();
            TreePreview {
                revision: snap.revision,
                ready: true,
                folders: plan.dirs.len(),
                files,
                logical_bytes: plan.bytes,
                examples: plan.examples,
                problems: vec![],
            }
        }
        Err(e) => TreePreview {
            revision: snap.revision,
            ready: false,
            folders: 0,
            files: 0,
            logical_bytes: 0,
            examples: vec![],
            problems: vec![e],
        },
    })
}
fn runs(conn: &sqlite::Connection) -> Result<TreeRuns, String> {
    init(conn)?;
    let revision = drive_snapshot(conn)?.revision;
    let mut st = conn
        .prepare(
            "SELECT root_id,original_name,created_at,folder_count,file_count
         FROM drive_tree_trash_runs ORDER BY created_at DESC LIMIT 100",
        )
        .map_err(sql_error)?;
    let mut output = Vec::new();
    while st.next().map_err(sql_error)? == sqlite::State::Row {
        let folders = st.read::<i64, _>(3).map_err(sql_error)?;
        let files = st.read::<i64, _>(4).map_err(sql_error)?;
        output.push(TreeRun {
            root_id: st.read::<String, _>(0).map_err(sql_error)?,
            name: st.read::<String, _>(1).map_err(sql_error)?,
            created_at: st.read::<i64, _>(2).map_err(sql_error)?,
            folder_count: usize::try_from(folders).map_err(|_| "Invalid journal folder count")?,
            file_count: usize::try_from(files).map_err(|_| "Invalid journal file count")?,
        });
    }
    Ok(TreeRuns {
        revision,
        runs: output,
    })
}
#[tauri::command]
pub fn cmd_drive_folder_tree_trash_preview(
    db: State<'_, DbConnection>,
    request: TreeRequest,
) -> Result<TreePreview, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_owned())?;
    preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_folder_tree_restore_preview(
    db: State<'_, DbConnection>,
    request: TreeRequest,
) -> Result<TreePreview, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_owned())?;
    restore_preview(&conn, &request)
}
#[tauri::command]
pub fn cmd_drive_folder_tree_trash_runs(db: State<'_, DbConnection>) -> Result<TreeRuns, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_owned())?;
    runs(&conn)
}
#[tauri::command]
pub fn cmd_drive_folder_tree_trash_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: TreeRequest,
) -> Result<TreeApplied, String> {
    let (done, changed, inodes) = {
        let conn = db.lock().map_err(|_| "Drive DB unavailable".to_owned())?;
        apply_trash(&conn, &request)?
    };
    notify(&app, &changed, &inodes);
    Ok(done)
}
#[tauri::command]
pub fn cmd_drive_folder_tree_restore_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: TreeRequest,
) -> Result<TreeApplied, String> {
    let (done, changed, inodes) = {
        let conn = db.lock().map_err(|_| "Drive DB unavailable".to_owned())?;
        restore_apply(&conn, &request)?
    };
    notify(&app, &changed, &inodes);
    Ok(done)
}
fn notify(app: &AppHandle, changed: &Changed, inodes: &Inodes) {
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(changed, inodes);
    }
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Recursive Tree Trash committed; Explorer refresh failed: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(app) {
        log::warn!("Recursive Tree Trash committed; Finder refresh failed: {e}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (changed, inodes);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = app;
}

/// Verify an imported portable receipt's complete current tombstoned tree
/// without restoring Drive content or changing the metadata revision.
pub(crate) fn validate_portable_tree_journal(
    conn: &sqlite::Connection,
    root_id: &str,
) -> Result<(usize, usize), String> {
    init(conn)?;
    let revision = drive_snapshot(conn)?.revision;
    let plan = restoration(
        conn,
        &TreeRequest {
            revision,
            root_id: root_id.to_owned(),
        },
    )?;
    let count = read_members(conn, root_id)?
        .iter()
        .filter(|m| m.kind == "file")
        .count();
    Ok((plan.dirs.len(), count))
}
pub(crate) fn ensure_portable_tree_journal_schema(conn: &sqlite::Connection) -> Result<(), String> {
    init(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
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
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES ('work',NULL,'Projects',1,1),
                    ('nested','work','Drafts',1,1),
                    ('other',NULL,'Other',1,1);
             INSERT INTO logical_channels
               (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES('qa',901,'qa','owner',1,1,1);
             INSERT INTO logical_files
               (file_id,logical_channel_id,first_message_id,display_name,total_size,
                mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('contents','qa',55,'notes.txt',14,'text/plain',1,'h',1,1,'complete');
             INSERT INTO drive_file_entries
               (entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('file-qa','contents','nested','notes.txt',1,1);",
        )
        .unwrap();
        conn
    }
    fn req(conn: &sqlite::Connection) -> TreeRequest {
        TreeRequest {
            revision: drive_snapshot(conn).unwrap().revision,
            root_id: "work".into(),
        }
    }
    #[test]
    fn complete_tree_moves_to_trash_and_exact_restore_is_atomic() {
        let c = fixture();
        let r = req(&c);
        let first = preview(&c, &r).unwrap();
        assert!(first.ready);
        assert_eq!(
            (first.folders, first.files, first.logical_bytes),
            (2, 1, 14)
        );
        let (trashed, _, _) = apply_trash(&c, &r).unwrap();
        assert_eq!((trashed.folders, trashed.files), (2, 1));
        let active = drive_snapshot(&c).unwrap();
        assert_eq!(
            active
                .directories
                .iter()
                .filter(|d| d.trashed_at.is_none())
                .count(),
            1
        );
        assert!(active.files.is_empty());
        let listing = runs(&c).unwrap();
        assert_eq!(listing.runs.len(), 1);
        assert_eq!(journal_count(&c, "work").unwrap(), 3);
        let restore = TreeRequest {
            revision: listing.revision,
            root_id: "work".into(),
        };
        let p = restore_preview(&c, &restore).unwrap();
        assert!(p.ready);
        assert_eq!((p.folders, p.files), (2, 1));
        let (done, _, _) = restore_apply(&c, &restore).unwrap();
        assert_eq!((done.folders, done.files), (2, 1));
        let after = drive_snapshot(&c).unwrap();
        assert_eq!(
            after
                .directories
                .iter()
                .filter(|d| d.trashed_at.is_none())
                .count(),
            3
        );
        assert_eq!(after.files.len(), 1);
        assert_eq!(after.files[0].entry_id, "file-qa");
        assert_eq!(after.files[0].directory_id.as_deref(), Some("nested"));
        assert_eq!(runs(&c).unwrap().runs.len(), 0);
    }
    #[test]
    fn pending_file_or_symlink_refuses_entire_trash_without_writes() {
        let c = fixture();
        let r = req(&c);
        c.execute(
            "INSERT INTO drive_pending_files
             (id,directory_id,display_name,staging_path,size,format_version,
              backing_channel_id,created_at,updated_at)
             VALUES('pending','nested','unfinished.txt','unused',1,1,901,1,1);",
        )
        .unwrap();
        assert!(!preview(&c, &r).unwrap().ready);
        assert!(apply_trash(&c, &r).is_err());
        assert_eq!(journal_count(&c, "work").unwrap(), 0);
        c.execute(
            "DELETE FROM drive_pending_files WHERE id='pending';
                   INSERT INTO drive_symlinks
                   (id,directory_id,display_name,target,created_at,updated_at)
                   VALUES('link','nested','shortcut','notes.txt',1,1)",
        )
        .unwrap();
        assert!(!preview(&c, &r).unwrap().ready);
        assert!(apply_trash(&c, &r).is_err());
        assert_eq!(drive_snapshot(&c).unwrap().files.len(), 1);
    }
    #[test]
    fn later_nested_folder_update_failure_rolls_back_earlier_file_trash() {
        let c = fixture();
        let r = req(&c);
        c.execute(
            "CREATE TRIGGER qa_abort_tree BEFORE UPDATE OF trashed_at ON drive_directories
             WHEN NEW.id='work' BEGIN SELECT RAISE(ABORT,'synthetic later failure'); END;",
        )
        .unwrap();
        assert!(apply_trash(&c, &r).is_err());
        let current = drive_snapshot(&c).unwrap();
        assert_eq!(current.revision, r.revision);
        assert_eq!(current.files.len(), 1);
        assert_eq!(
            current
                .directories
                .iter()
                .filter(|d| d.trashed_at.is_none())
                .count(),
            3
        );
        assert_eq!(journal_count(&c, "work").unwrap(), 0);
        assert!(!has_run(&c, "work").unwrap());
    }
    #[test]
    fn occupied_original_root_path_blocks_recovery_without_partial_changes() {
        let c = fixture();
        apply_trash(&c, &req(&c)).unwrap();
        c.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('occupied',NULL,'Projects',2,2)",
        )
        .unwrap();
        let r = req(&c);
        assert!(!restore_preview(&c, &r).unwrap().ready);
        assert!(restore_apply(&c, &r).is_err());
        assert!(has_run(&c, "work").unwrap());
        assert_eq!(journal_count(&c, "work").unwrap(), 3);
        assert!(current_row(&c, "folder", "work", 0).unwrap().2.is_some());
    }
    #[test]
    fn failed_mid_restore_rolls_back_root_recovery_and_preserves_journal() {
        let c = fixture();
        apply_trash(&c, &req(&c)).unwrap();
        let r = req(&c);
        c.execute(
            "CREATE TRIGGER qa_abort_recovery BEFORE UPDATE OF trashed_at ON drive_file_entries
             WHEN NEW.trashed_at IS NULL BEGIN SELECT RAISE(ABORT,'synthetic restore failure'); END;"
        ).unwrap();
        assert!(restore_apply(&c, &r).is_err());
        assert_eq!(drive_snapshot(&c).unwrap().revision, r.revision);
        assert!(current_row(&c, "folder", "work", 0).unwrap().2.is_some());
        assert!(current_row(&c, "folder", "nested", 0).unwrap().2.is_some());
        assert!(current_row(&c, "file", "file-qa", 1).unwrap().2.is_some());
        assert_eq!(journal_count(&c, "work").unwrap(), 3);
    }
    #[test]
    fn incomplete_recovery_journal_refuses_partial_recovery() {
        let c = fixture();
        apply_trash(&c, &req(&c)).unwrap();
        c.execute(
            "DELETE FROM drive_tree_trash_members
                   WHERE root_id='work' AND kind='file'",
        )
        .unwrap();
        let r = req(&c);
        let p = restore_preview(&c, &r).unwrap();
        assert!(!p.ready);
        assert!(p.problems.iter().any(|m| m.contains("incomplete")));
        assert!(restore_apply(&c, &r).is_err());
        assert!(current_row(&c, "folder", "work", 0).unwrap().2.is_some());
        assert_eq!(drive_snapshot(&c).unwrap().files.len(), 0);
    }

    #[test]
    fn changed_backing_identity_blocks_recovery_even_with_same_size() {
        let c = fixture();
        apply_trash(&c, &req(&c)).unwrap();
        c.execute(
            "INSERT INTO logical_files
               (file_id,logical_channel_id,first_message_id,display_name,total_size,
                mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('other-content','qa',56,'notes.txt',14,'text/plain',1,'other',1,1,'complete');
             UPDATE drive_file_entries SET file_id='other-content'
             WHERE entry_id='file-qa';",
        )
        .unwrap();
        let r = req(&c);
        let p = restore_preview(&c, &r).unwrap();
        assert!(!p.ready);
        assert!(p
            .problems
            .iter()
            .any(|reason| reason.contains("content identity")));
        assert!(restore_apply(&c, &r).is_err());
        assert_eq!(journal_count(&c, "work").unwrap(), 3);
    }

    #[test]
    fn inactive_backing_and_stale_revision_never_trash() {
        let c = fixture();
        let mut r = req(&c);
        r.revision -= 1;
        assert!(!preview(&c, &r).unwrap().ready);
        assert!(apply_trash(&c, &r).is_err());
        let c2 = fixture();
        c2.execute("UPDATE logical_files SET status='missing' WHERE file_id='contents';")
            .unwrap();
        assert!(!preview(&c2, &req(&c2)).unwrap().ready);
    }
    #[test]
    fn authoritative_v2_content_is_trashed_and_restored_not_legacy_fallback() {
        let c = fixture();
        c.execute(
            "INSERT INTO drive_objects
               (object_id,plaintext_size,encryption_version,crypto_id,
                content_fingerprint,block_count,created_at)
             VALUES('v2',14,0,NULL,'fingerprint',0,1);
             INSERT INTO drive_entries_v2
               (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at)
             VALUES('file-qa','v2',NULL,'nested','notes.txt',1,2);",
        )
        .unwrap();
        let r = req(&c);
        assert!(preview(&c, &r).unwrap().ready);
        apply_trash(&c, &r).unwrap();
        let recovery = req(&c);
        assert!(restore_preview(&c, &recovery).unwrap().ready);
        restore_apply(&c, &recovery).unwrap();
        let snapshot = drive_snapshot(&c).unwrap();
        assert!(snapshot
            .files
            .iter()
            .any(|f| f.entry_id == "file-qa" && f.storage_version == 2));
        assert!(!has_run(&c, "work").unwrap());
    }
}
