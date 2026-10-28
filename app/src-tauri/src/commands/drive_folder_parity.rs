//! Read-only comparison of two complete Drive folder trees, including hidden
//! active file records. Never mutates Drive or claims byte-for-byte proof.
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, validate_drive_name, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use tauri::State;

const MAX_TREE: usize = 512;
const MAX_FILES: usize = 2000;
const MAX_DIFFS: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct FolderParityDifference {
    pub kind: &'static str,
    pub relative_path: String,
    pub left: Option<String>,
    pub right: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct FolderParityReport {
    pub revision: i64,
    pub dirty: bool,
    pub left_id: String,
    pub right_id: String,
    pub left_folders: usize,
    pub right_folders: usize,
    pub left_files: usize,
    pub right_files: usize,
    pub matching_file_metadata: usize,
    pub unverified_file_content: usize,
    pub only_left: usize,
    pub only_right: usize,
    pub different: usize,
    pub truncated_differences: bool,
    pub differences: Vec<FolderParityDifference>,
    pub read_only: bool,
}
#[derive(Clone, Debug)]
struct Node {
    kind: &'static str, // folder, file, link
    info: String,       // non-sensitive content metadata / link destination
    verified_metadata: bool,
}
#[derive(Clone, Debug)]
struct Tree {
    directories: HashMap<String, String>, // ID -> relative path, includes root as ""
    nodes: BTreeMap<String, Node>,
    folders: usize,
    files: usize,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn fold(path: &str) -> String {
    path.split('/')
        .map(portable_name_key)
        .collect::<Vec<_>>()
        .join("/")
}
fn insert(
    tree: &mut Tree,
    occupied: &mut HashSet<String>,
    path: String,
    kind: &'static str,
    info: String,
    verified_metadata: bool,
) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 4096
        || path.split('/').any(|p| validate_drive_name(p).is_err())
    {
        return Err("Drive parity found an unsafe or oversized relative path".into());
    }
    if !occupied.insert(fold(&path)) {
        return Err("Drive parity found duplicate or case-insensitive colliding names".into());
    }
    if tree
        .nodes
        .insert(
            path,
            Node {
                kind,
                info,
                verified_metadata,
            },
        )
        .is_some()
    {
        return Err("Drive parity discovered duplicate namespace entries".into());
    }
    Ok(())
}
fn collect_dirs(s: &DriveSnapshot, root: &str) -> Result<Tree, String> {
    if !valid_id(root) {
        return Err("Invalid selected Drive folder identity".into());
    }
    let dir_index: HashMap<_, _> = s
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    if !dir_index.contains_key(root) {
        return Err("Selected Drive folder is missing, unavailable or in Trash".into());
    }
    if dir_index.len()
        != s.directories
            .iter()
            .filter(|d| d.trashed_at.is_none())
            .count()
    {
        return Err("Ambiguous Drive folder identities".into());
    }
    let mut tree = Tree {
        directories: HashMap::new(),
        nodes: BTreeMap::new(),
        folders: 0,
        files: 0,
    };
    let mut children: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    for folder in dir_index.values() {
        children
            .entry(folder.parent_id.as_deref())
            .or_default()
            .push(folder.id.as_str());
    }
    let mut queue = VecDeque::from([(root.to_owned(), String::new(), 0usize)]);
    let mut visited = HashSet::new();
    let mut occupied = HashSet::new();
    while let Some((id, parent_path, depth)) = queue.pop_front() {
        if depth > 256 || !visited.insert(id.clone()) || tree.directories.len() >= MAX_TREE {
            return Err("Selected folder tree is cyclic, too deep, or exceeds 512 folders".into());
        }
        let folder = dir_index
            .get(id.as_str())
            .ok_or("Drive directory disappeared")?;
        if depth > 0 {
            validate_drive_name(&folder.name)?;
            insert(
                &mut tree,
                &mut occupied,
                parent_path.clone(),
                "folder",
                String::new(),
                true,
            )?;
            tree.folders += 1;
        }
        tree.directories.insert(id.clone(), parent_path.clone());
        if let Some(kids) = children.get(&Some(id.as_str())) {
            for child in kids {
                let f = dir_index
                    .get(*child)
                    .ok_or("Drive folder child is unavailable")?;
                validate_drive_name(&f.name)?;
                let path = if parent_path.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}/{}", parent_path, f.name)
                };
                queue.push_back(((*child).to_owned(), path, depth + 1));
            }
        }
    }
    Ok(tree)
}
fn tree_files(
    s: &DriveSnapshot,
    conn: &sqlite::Connection,
    left: &mut Tree,
    right: &mut Tree,
) -> Result<(), String> {
    // A raw query is essential: the visible snapshot intentionally omits
    // unavailable backing and COW-shadowed files. Never call those "missing".
    // Scope the read to the selected trees; unrelated Drive files do not
    // exhaust the scan budget.
    let mut q = conn
        .prepare(
            "WITH RECURSIVE scoped(id,side) AS (
           SELECT ?1,1 UNION SELECT ?2,2
           UNION SELECT child.id, scoped.side
             FROM drive_directories child JOIN scoped ON child.parent_id=scoped.id
             WHERE child.trashed_at IS NULL
         )
         SELECT e.entry_id,e.directory_id,e.display_name,1 AS storage,
            CASE WHEN f.file_id IS NOT NULL AND f.status='complete'
                      AND c.logical_id IS NOT NULL THEN 1 ELSE 0 END AS ready,
            f.total_size,COALESCE(f.whole_sha256,''),e.encryption_version,
            COALESCE(e.crypto_id,''),scoped.side
         FROM drive_file_entries e
         JOIN scoped ON e.directory_id=scoped.id
         LEFT JOIN logical_files f ON f.file_id=e.file_id
         LEFT JOIN logical_channels c ON c.logical_id=f.logical_channel_id
         WHERE e.trashed_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM drive_entries_v2 v
                WHERE v.entry_id=e.entry_id)
         UNION ALL
         SELECT e.entry_id,e.directory_id,e.display_name,2,
            CASE WHEN o.object_id IS NOT NULL THEN 1 ELSE 0 END,
            o.plaintext_size,COALESCE(o.content_fingerprint,''),
            COALESCE(o.encryption_version,0),COALESCE(o.crypto_id,''),scoped.side
         FROM drive_entries_v2 e JOIN scoped ON e.directory_id=scoped.id
         LEFT JOIN drive_objects o ON o.object_id=e.object_id
         WHERE e.trashed_at IS NULL LIMIT ?3",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    // Root identities always differ; paths may not be nested.
    let root_left = left
        .directories
        .iter()
        .find_map(|(id, path)| {
            if path.is_empty() {
                Some(id.clone())
            } else {
                None
            }
        })
        .ok_or("Left Drive folder root missing")?;
    let root_right = right
        .directories
        .iter()
        .find_map(|(id, path)| {
            if path.is_empty() {
                Some(id.clone())
            } else {
                None
            }
        })
        .ok_or("Right Drive folder root missing")?;
    q.bind((1, root_left.as_str())).map_err(|e| e.to_string())?;
    q.bind((2, root_right.as_str()))
        .map_err(|e| e.to_string())?;
    q.bind((3, (MAX_FILES * 2 + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut occupied_left = left.nodes.keys().map(|p| fold(p)).collect::<HashSet<_>>();
    let mut occupied_right = right.nodes.keys().map(|p| fold(p)).collect::<HashSet<_>>();
    let mut seen_left = HashSet::new();
    let mut seen_right = HashSet::new();
    while q.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        let id = q.read::<String, _>(0).map_err(|e| e.to_string())?;
        let parent = q.read::<String, _>(1).map_err(|e| e.to_string())?;
        let name = q.read::<String, _>(2).map_err(|e| e.to_string())?;
        let storage = q.read::<i64, _>(3).map_err(|e| e.to_string())?;
        let ready = q.read::<i64, _>(4).map_err(|e| e.to_string())? == 1;
        let size = q.read::<sqlite::Value, _>(5).map_err(|e| e.to_string())?;
        let digest = q.read::<String, _>(6).map_err(|e| e.to_string())?;
        let encryption = q.read::<i64, _>(7).map_err(|e| e.to_string())?;
        let crypto = q.read::<String, _>(8).map_err(|e| e.to_string())?;
        let side = q.read::<i64, _>(9).map_err(|e| e.to_string())?;
        let (tree, occupied, seen) = match side {
            1 => (&mut *left, &mut occupied_left, &mut seen_left),
            2 => (&mut *right, &mut occupied_right, &mut seen_right),
            _ => return Err("Invalid Drive parity tree side".into()),
        };
        tree.files += 1;
        if tree.files > MAX_FILES || !valid_id(&id) || !seen.insert(id.clone()) {
            return Err("Folder parity raw file count exceeds 2000 or identities repeat".into());
        }
        if !ready {
            return Err("One compared folder contains an active file with unavailable backing; run Namespace Audit before comparing".into());
        }
        if storage != 1 && storage != 2 {
            return Err("Invalid Drive storage generation".into());
        }
        let size = match size {
            sqlite::Value::Integer(v) if v >= 0 => v as u64,
            _ => return Err("Invalid file size in Drive parity inventory".into()),
        };
        let parent_rel = tree
            .directories
            .get(&parent)
            .ok_or("A Drive file lies outside its requested folder tree")?;
        let rel = if parent_rel.is_empty() {
            name.clone()
        } else {
            format!("{parent_rel}/{name}")
        };
        if s.pending
            .iter()
            .any(|p| p.base_entry_id.as_deref() == Some(id.as_str()))
        {
            return Err("A compared file has pending copy-on-write changes".into());
        }
        if crypto.len() > 256 || digest.len() > 256 || encryption < 0 {
            return Err("Malformed or oversized Drive file backing metadata".into());
        }
        let strong =
            storage == 1 && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
        // Typed serialization avoids delimiter-collision false matches.
        let metadata = serde_json::to_string(&(storage, size, encryption, crypto, digest))
            .map_err(|e| e.to_string())?;
        insert(tree, occupied, rel, "file", metadata, strong)?;
    }
    Ok(())
}
fn collect_links_and_pending(s: &DriveSnapshot, tree: &mut Tree) -> Result<(), String> {
    let mut occupied = tree.nodes.keys().map(|x| fold(x)).collect::<HashSet<_>>();
    for p in &s.pending {
        if tree
            .directories
            .contains_key(p.directory_id.as_deref().unwrap_or(""))
        {
            return Err(
                "Folder parity cannot check folders containing unfinished uploads or edits".into(),
            );
        }
    }
    for link in s.symlinks.iter().filter(|x| x.trashed_at.is_none()) {
        if let Some(parent) = link
            .directory_id
            .as_deref()
            .and_then(|id| tree.directories.get(id))
        {
            if link.target.len() > 4096 {
                return Err("Drive symlink metadata too large to compare".into());
            }
            let rel = if parent.is_empty() {
                link.display_name.clone()
            } else {
                format!("{parent}/{}", link.display_name)
            };
            insert(tree, &mut occupied, rel, "link", link.target.clone(), true)?;
        }
    }
    Ok(())
}
pub(crate) fn compare(
    s: &DriveSnapshot,
    conn: &sqlite::Connection,
    left_id: &str,
    right_id: &str,
) -> Result<FolderParityReport, String> {
    if !valid_id(left_id) || !valid_id(right_id) || left_id == right_id {
        return Err("Choose two different Drive folders".into());
    }
    let mut left = collect_dirs(s, left_id)?;
    let mut right = collect_dirs(s, right_id)?;
    if left.directories.contains_key(right_id) || right.directories.contains_key(left_id) {
        return Err("Compare independent folder trees, not a folder and its descendant".into());
    }
    // Prevent malformed pending paths from hiding a file without a visible
    // completed shadow; native raw scan catches the shadowed base too.
    collect_links_and_pending(s, &mut left)?;
    collect_links_and_pending(s, &mut right)?;
    tree_files(s, conn, &mut left, &mut right)?;
    let mut report = FolderParityReport {
        revision: s.revision,
        dirty: s.dirty,
        left_id: left_id.to_owned(),
        right_id: right_id.to_owned(),
        left_folders: left.folders,
        right_folders: right.folders,
        left_files: left.files,
        right_files: right.files,
        matching_file_metadata: 0,
        unverified_file_content: 0,
        only_left: 0,
        only_right: 0,
        different: 0,
        truncated_differences: false,
        differences: Vec::new(),
        read_only: true,
    };
    let mut keys = left
        .nodes
        .keys()
        .chain(right.nodes.keys())
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    for path in keys {
        let l = left.nodes.get(path);
        let r = right.nodes.get(path);
        let kind = match (l, r) {
            (Some(_), None) => {
                report.only_left += 1;
                "only_left"
            }
            (None, Some(_)) => {
                report.only_right += 1;
                "only_right"
            }
            (Some(l), Some(r)) if l.kind != r.kind || l.info != r.info => {
                report.different += 1;
                "metadata_difference"
            }
            (Some(l), Some(r))
                if l.kind == "file" && (!l.verified_metadata || !r.verified_metadata) =>
            {
                report.unverified_file_content += 1;
                "content_unverified"
            }
            (Some(l), Some(_)) => {
                if l.kind == "file" {
                    report.matching_file_metadata += 1;
                }
                continue;
            }
            _ => continue,
        };
        if report.differences.len() == MAX_DIFFS {
            report.truncated_differences = true;
            continue;
        }
        report.differences.push(FolderParityDifference {
            kind,
            relative_path: (*path).clone(),
            // Do not leak cryptographic IDs or content fingerprints to UI.
            left: l.map(|n| n.kind.to_owned()),
            right: r.map(|n| n.kind.to_owned()),
        });
    }
    Ok(report)
}
#[tauri::command]
pub fn cmd_drive_folder_parity(
    db: State<'_, DbConnection>,
    left_id: String,
    right_id: String,
) -> Result<FolderParityReport, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    let snapshot = drive_snapshot(&conn)?;
    compare(&snapshot, &conn, &left_id, &right_id)
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
        let digest = "a".repeat(64);
        conn.execute(
            "INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
             VALUES('qa',5,'Test','owner',1,1,1);
             INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('left',NULL,'Original',1,1),('right',NULL,'Backup',1,1),
             ('nested-left','left','Docs',1,1),('nested-right','right','Docs',1,1);"
        ).unwrap();
        for (id, msg) in [("one", 11), ("two", 12)] {
            let mut q = conn
                .prepare(
                    "INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,
                 display_name,total_size,chunk_count,whole_sha256,storage_version,created_at,status)
                 VALUES(?, 'qa', ?, 'report.txt', 5, 1, ?, 1, 1,'complete')",
                )
                .unwrap();
            q.bind((1, id)).unwrap();
            q.bind((2, msg as i64)).unwrap();
            q.bind((3, digest.as_str())).unwrap();
            q.next().unwrap();
        }
        conn.execute(
            "INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('f-left','one','nested-left','report.txt',1,1),
                   ('f-right','two','nested-right','report.txt',1,1);"
        ).unwrap();
        conn
    }
    #[test]
    fn matching_nested_folders_and_sha_metadata_are_detected() {
        let c = fixture();
        let s = drive_snapshot(&c).unwrap();
        let r = compare(&s, &c, "left", "right").unwrap();
        assert_eq!(r.left_folders, 1);
        assert_eq!(r.right_folders, 1);
        assert_eq!(r.left_files, 1);
        assert_eq!(r.right_files, 1);
        assert_eq!(r.matching_file_metadata, 1);
        assert_eq!(r.different, 0);
        assert_eq!(r.unverified_file_content, 0);
        assert!(r.differences.is_empty());
    }
    #[test]
    fn absent_destination_file_is_identified_without_changing_database() {
        let c = fixture();
        c.execute("UPDATE drive_file_entries SET trashed_at=5 WHERE entry_id='f-right'")
            .unwrap();
        let s = drive_snapshot(&c).unwrap();
        let r = compare(&s, &c, "left", "right").unwrap();
        assert_eq!(r.only_left, 1);
        assert!(r
            .differences
            .iter()
            .any(|d| d.kind == "only_left" && d.relative_path == "Docs/report.txt"));
        assert_eq!(drive_snapshot(&c).unwrap().revision, s.revision);
    }
    #[test]
    fn content_mismatch_is_not_falsely_called_matching() {
        let c = fixture();
        c.execute("UPDATE logical_files SET whole_sha256='other-sha' WHERE file_id='two'")
            .unwrap();
        let s = drive_snapshot(&c).unwrap();
        let r = compare(&s, &c, "left", "right").unwrap();
        assert_eq!(r.different, 1);
        assert_eq!(r.matching_file_metadata, 0);
    }
    #[test]
    fn missing_backing_blocks_entire_parity_claim() {
        let c = fixture();
        c.execute("UPDATE logical_files SET status='missing' WHERE file_id='two'")
            .unwrap();
        let s = drive_snapshot(&c).unwrap();
        assert!(compare(&s, &c, "left", "right")
            .unwrap_err()
            .contains("unavailable backing"));
    }
    #[test]
    fn pending_unfinished_edit_refuses_comparison() {
        let c = fixture();
        c.execute(
            "INSERT INTO drive_pending_files(id,directory_id,display_name,staging_path,
               size,format_version,backing_channel_id,created_at,updated_at)
             VALUES('pending','nested-right','report.txt','/nowhere',1,1,5,1,1)",
        )
        .unwrap();
        let s = drive_snapshot(&c).unwrap();
        assert!(compare(&s, &c, "left", "right")
            .unwrap_err()
            .contains("unfinished"));
    }
    #[test]
    fn overlapping_trees_and_same_root_are_refused() {
        let c = fixture();
        let s = drive_snapshot(&c).unwrap();
        assert!(compare(&s, &c, "left", "nested-left").is_err());
        assert!(compare(&s, &c, "right", "right").is_err());
    }
    #[test]
    fn casefold_collision_is_refused() {
        let c = fixture();
        c.execute(
            "INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,
             display_name,total_size,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES('third','qa',13,'REPORT.TXT',5,1,'aaa',1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
             VALUES('f-third','third','nested-right','REPORT.TXT',1,1)"
        ).unwrap();
        let s = drive_snapshot(&c).unwrap();
        assert!(compare(&s, &c, "left", "right")
            .unwrap_err()
            .contains("colliding"));
    }
    #[test]
    fn v2_generation_supersedes_legacy_same_identity() {
        let c = fixture();
        c.execute(
            "INSERT INTO drive_objects(object_id,plaintext_size,encryption_version,
             crypto_id,content_fingerprint,block_count,created_at)
             VALUES('object',5,0,NULL,'v2-fingerprint',0,1);
             INSERT INTO drive_entries_v2(entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at)
             VALUES('f-left','object',NULL,'nested-left','report.txt',1,2)"
        ).unwrap();
        let s = drive_snapshot(&c).unwrap();
        let r = compare(&s, &c, "left", "right").unwrap();
        assert_eq!(r.left_files, 1);
        assert_eq!(r.different, 1);
    }
}
