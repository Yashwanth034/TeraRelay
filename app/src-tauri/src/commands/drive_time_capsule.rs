//! On-device point-in-time Drive metadata captures and historical change comparisons.
//! Metadata only: never reads file bytes or remote content, never mutates Drive entries.
use crate::commands::drive_metadata::{drive_snapshot, validate_drive_name};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use tauri::State;

const MAX_ENTRIES: usize = 5000;
const MAX_RAW: usize = 5001;
const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_HISTORY: usize = 12;
const SQL: &str = "CREATE TABLE IF NOT EXISTS drive_local_time_capsules (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    captured_at INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    dirty INTEGER NOT NULL,
    item_count INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    payload TEXT NOT NULL
)";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) backing: String,
    pub(crate) size: u64,
    pub(crate) fingerprint: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capsule {
    pub(crate) version: u32,
    pub(crate) revision: i64,
    pub(crate) dirty: bool,
    pub(crate) entries: Vec<Entry>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CapsuleInfo {
    pub seq: i64,
    pub captured_at: i64,
    pub revision: i64,
    pub dirty: bool,
    pub item_count: usize,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct CapsuleChange {
    pub change: &'static str,
    pub kind: String,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub previous_backing: Option<String>,
    pub current_backing: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CapsuleComparison {
    pub before: CapsuleInfo,
    pub after: CapsuleInfo,
    pub created: usize,
    pub removed: usize,
    pub moved: usize,
    pub renamed: usize,
    pub ancestor_path_changed: usize,
    pub content_metadata_changed: usize,
    pub availability_changed: usize,
    pub backing_state_changed: usize,
    pub changes: Vec<CapsuleChange>,
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s != "."
        && s != ".."
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn path_of(
    parent: Option<&str>,
    name: &str,
    dirs: &HashMap<&str, (Option<&str>, &str)>,
) -> Result<String, String> {
    validate_drive_name(name)?;
    let mut path = vec![name];
    let mut visited = HashSet::new();
    let mut cursor = parent;
    while let Some(id) = cursor {
        if !valid_id(id) || !visited.insert(id) || visited.len() > 512 {
            return Err("Drive folder history contains a cycle or unsupported depth".into());
        }
        let (parent, name) = dirs
            .get(id)
            .ok_or("Drive item has orphaned active parent")?;
        validate_drive_name(name)?;
        path.push(name);
        cursor = *parent;
    }
    path.reverse();
    let joined = format!("/{}", path.join("/"));
    if joined.len() > 4096 {
        return Err("Drive historical item path exceeds 4096 bytes".into());
    }
    Ok(joined)
}
fn insert_unique(map: &mut BTreeMap<(String, String), Entry>, entry: Entry) -> Result<(), String> {
    if !valid_id(&entry.id)
        || entry.path.len() > 4096
        || entry.name.len() > 255
        || entry.fingerprint.len() > 128
    {
        return Err(
            "Drive historical metadata contains invalid identity or oversized fields".into(),
        );
    }
    if map
        .insert((entry.kind.clone(), entry.id.clone()), entry)
        .is_some()
    {
        return Err("Drive historical metadata contains duplicate kind/identity".into());
    }
    if map.len() > MAX_ENTRIES {
        return Err("Drive inventory exceeds 5000 items; history capture refused".into());
    }
    Ok(())
}
pub(crate) fn capture_snapshot(conn: &sqlite::Connection) -> Result<Capsule, String> {
    let snap = drive_snapshot(conn)?;
    if snap.directories.len() + snap.pending.len() + snap.symlinks.len() > MAX_ENTRIES {
        return Err("Drive namespace too large for complete history capture".into());
    }
    let dirs: HashMap<_, _> = snap
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), (d.parent_id.as_deref(), d.name.as_str())))
        .collect();
    if dirs.len()
        != snap
            .directories
            .iter()
            .filter(|d| d.trashed_at.is_none())
            .count()
    {
        return Err("Drive historical folders have duplicate identities".into());
    }
    let mut entries = BTreeMap::new();
    for d in snap.directories.iter().filter(|d| d.trashed_at.is_none()) {
        insert_unique(
            &mut entries,
            Entry {
                kind: "folder".into(),
                id: d.id.clone(),
                parent: d.parent_id.clone(),
                name: d.name.clone(),
                path: path_of(d.parent_id.as_deref(), &d.name, &dirs)?,
                backing: "metadata".into(),
                size: 0,
                fingerprint: String::new(),
            },
        )?;
    }
    // Raw active records include completed, unavailable, or shadowed content.
    // This prevents falsely calling a file "deleted" solely because Drive's
    // visible snapshot has hidden an unavailable remote object.
    let mut raw = conn
        .prepare(
            "SELECT source,entry_id,directory_id,display_name,available,size,content_hash FROM (
          SELECT 1 AS source,e.entry_id,e.directory_id,e.display_name,
            CASE WHEN f.file_id IS NOT NULL AND f.status='complete'
                  AND c.logical_id IS NOT NULL THEN 1 ELSE 0 END AS available,
            CASE WHEN f.file_id IS NULL THEN 0 ELSE f.total_size END AS size,
            COALESCE(f.whole_sha256,'') AS content_hash
          FROM drive_file_entries e
          LEFT JOIN logical_files f ON f.file_id=e.file_id
          LEFT JOIN logical_channels c ON c.logical_id=f.logical_channel_id
          WHERE e.trashed_at IS NULL AND NOT EXISTS (
            SELECT 1 FROM drive_entries_v2 newer
            WHERE newer.entry_id=e.entry_id AND newer.trashed_at IS NOT NULL
          )
          UNION ALL
          SELECT 2 AS source,e.entry_id,e.directory_id,e.display_name,
            CASE WHEN o.object_id IS NOT NULL THEN 1 ELSE 0 END AS available,
            CASE WHEN o.object_id IS NULL THEN 0 ELSE o.plaintext_size END AS size,
            COALESCE(o.content_fingerprint,'') AS content_hash
          FROM drive_entries_v2 e LEFT JOIN drive_objects o ON o.object_id=e.object_id
          WHERE e.trashed_at IS NULL
        ) ORDER BY entry_id,source LIMIT ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    raw.bind((1, MAX_RAW as i64)).map_err(|e| e.to_string())?;
    let mut files: BTreeMap<String, (u8, Entry)> = BTreeMap::new();
    let mut seen = 0;
    while raw.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        seen += 1;
        if seen > MAX_ENTRIES {
            return Err("Raw Drive file inventory exceeds bounded historical capture".into());
        }
        let source = raw.read::<i64, _>(0).map_err(|e| e.to_string())? as u8;
        if source != 1 && source != 2 {
            return Err("Unsupported Drive storage generation".into());
        }
        let id = raw.read::<String, _>(1).map_err(|e| e.to_string())?;
        if !valid_id(&id) {
            return Err("Drive file has invalid entry identity".into());
        }
        let parent = match raw.read::<sqlite::Value, _>(2).map_err(|e| e.to_string())? {
            sqlite::Value::Null => None,
            sqlite::Value::String(v) => Some(v),
            _ => return Err("Corrupt Drive file parent".into()),
        };
        let name = raw.read::<String, _>(3).map_err(|e| e.to_string())?;
        let available = raw.read::<i64, _>(4).map_err(|e| e.to_string())? == 1;
        let size = raw.read::<i64, _>(5).map_err(|e| e.to_string())?;
        if size < 0 {
            return Err("Corrupt Drive historical file size".into());
        }
        let fingerprint = raw.read::<String, _>(6).map_err(|e| e.to_string())?;
        let entry = Entry {
            kind: "file".into(),
            id: id.clone(),
            parent: parent.clone(),
            name: name.clone(),
            path: path_of(parent.as_deref(), &name, &dirs)?,
            backing: match (source, available) {
                (1, true) => "legacy",
                (2, true) => "v2",
                (1, false) => "legacy_unavailable",
                _ => "v2_unavailable",
            }
            .into(),
            size: size as u64,
            fingerprint,
        };
        match files.entry(id) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((source, entry));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if source == slot.get().0 {
                    return Err(
                        "Duplicate raw Drive file identity for one storage generation".into(),
                    );
                }
                if source > slot.get().0 {
                    slot.insert((source, entry));
                }
            }
        }
    }
    drop(raw);
    for (_, (_, entry)) in files {
        insert_unique(&mut entries, entry)?;
    }
    for l in snap.symlinks.iter().filter(|l| l.trashed_at.is_none()) {
        insert_unique(
            &mut entries,
            Entry {
                kind: "link".into(),
                id: l.id.clone(),
                parent: l.directory_id.clone(),
                name: l.display_name.clone(),
                path: path_of(l.directory_id.as_deref(), &l.display_name, &dirs)?,
                backing: "metadata".into(),
                size: 0,
                fingerprint: String::new(),
            },
        )?;
    }
    for p in &snap.pending {
        insert_unique(
            &mut entries,
            Entry {
                kind: "pending".into(),
                id: p.id.clone(),
                parent: p.directory_id.clone(),
                name: p.display_name.clone(),
                path: path_of(p.directory_id.as_deref(), &p.display_name, &dirs)?,
                backing: if p.closed_at.is_some() {
                    "closed"
                } else {
                    "open"
                }
                .into(),
                size: p.size,
                fingerprint: p.base_entry_id.clone().unwrap_or_default(),
            },
        )?;
    }
    let capsule = Capsule {
        version: 1,
        revision: snap.revision,
        dirty: snap.dirty,
        entries: entries.into_values().collect(),
    };
    validate_capsule(&capsule)?;
    Ok(capsule)
}
pub(crate) fn validate_capsule(c: &Capsule) -> Result<(), String> {
    if c.version != 1 || c.revision < 0 || c.entries.len() > MAX_ENTRIES {
        return Err("Invalid Drive history schema, revision or count".into());
    }
    let mut keys = HashSet::new();
    for entry in &c.entries {
        if !["file", "folder", "pending", "link"].contains(&entry.kind.as_str())
            || !valid_id(&entry.id)
            || validate_drive_name(&entry.name).is_err()
            || !entry.path.starts_with('/')
            || entry.path.len() > 4096
            || entry
                .path
                .split('/')
                .skip(1)
                .any(|name| validate_drive_name(name).is_err())
            || entry.parent.as_deref().is_some_and(|id| !valid_id(id))
            || ![
                "metadata",
                "legacy",
                "v2",
                "legacy_unavailable",
                "v2_unavailable",
                "open",
                "closed",
            ]
            .contains(&entry.backing.as_str())
            || entry.fingerprint.len() > 128
            || !keys.insert((entry.kind.as_str(), entry.id.as_str()))
        {
            return Err("Drive history payload has unsafe or duplicate entries".into());
        }
    }
    Ok(())
}
fn fingerprint(c: &str) -> String {
    format!("{:x}", Sha256::digest(c.as_bytes()))
}
fn decode(payload: &str, expected: &str) -> Result<Capsule, String> {
    if payload.len() > MAX_PAYLOAD || fingerprint(payload) != expected {
        return Err("Historical Drive snapshot checksum or payload length mismatch".into());
    }
    let value: Capsule =
        serde_json::from_str(payload).map_err(|_| "Historical Drive metadata is corrupt")?;
    validate_capsule(&value)?;
    Ok(value)
}
fn list_infos(conn: &sqlite::Connection) -> Result<Vec<CapsuleInfo>, String> {
    conn.execute(SQL)
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = conn
        .prepare(
            "SELECT seq,captured_at,revision,dirty,item_count,fingerprint
         FROM drive_local_time_capsules ORDER BY seq DESC LIMIT 13",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut out = Vec::new();
    while rows.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if out.len() == MAX_HISTORY {
            return Err("Too many retained Drive history records".into());
        }
        let seq = rows.read::<i64, _>(0).map_err(|e| e.to_string())?;
        let count = rows.read::<i64, _>(4).map_err(|e| e.to_string())?;
        let dirty = rows.read::<i64, _>(3).map_err(|e| e.to_string())?;
        let fp = rows.read::<String, _>(5).map_err(|e| e.to_string())?;
        if seq < 1
            || count < 0
            || count as usize > MAX_ENTRIES
            || !matches!(dirty, 0 | 1)
            || fp.len() != 64
            || !fp.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Corrupt Drive snapshot history index".into());
        }
        out.push(CapsuleInfo {
            seq,
            captured_at: rows.read::<i64, _>(1).map_err(|e| e.to_string())?,
            revision: rows.read::<i64, _>(2).map_err(|e| e.to_string())?,
            dirty: dirty == 1,
            item_count: count as usize,
            fingerprint: fp,
        });
    }
    Ok(out)
}
fn load(conn: &sqlite::Connection, id: i64) -> Result<(CapsuleInfo, Capsule), String> {
    let info = list_infos(conn)?
        .into_iter()
        .find(|i| i.seq == id)
        .ok_or("Local Drive history snapshot not found")?;
    let mut query = conn
        .prepare("SELECT payload FROM drive_local_time_capsules WHERE seq=?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    query.bind((1, id)).map_err(|e| e.to_string())?;
    if query.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Err("Drive history payload disappeared".into());
    }
    let payload = query.read::<String, _>(0).map_err(|e| e.to_string())?;
    let capsule = decode(&payload, &info.fingerprint)?;
    if capsule.revision != info.revision
        || capsule.dirty != info.dirty
        || capsule.entries.len() != info.item_count
    {
        return Err("Drive history index differs from payload".into());
    }
    Ok((info, capsule))
}
fn save(conn: &sqlite::Connection, c: Capsule) -> Result<CapsuleInfo, String> {
    validate_capsule(&c)?;
    let payload = serde_json::to_string(&c).map_err(|e| e.to_string())?;
    if payload.len() > MAX_PAYLOAD {
        return Err("Drive history exceeds 4 MiB private storage limit".into());
    }
    let digest = fingerprint(&payload);
    conn.execute(SQL)
        .map_err(|e: sqlite::Error| e.to_string())?;
    conn.execute("SAVEPOINT drive_time_capsule_capture")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut statement = conn
            .prepare(
                "INSERT INTO drive_local_time_capsules
            (captured_at,revision,dirty,item_count,fingerprint,payload) VALUES(?,?,?,?,?,?)",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        statement
            .bind((1, chrono::Utc::now().timestamp_millis()))
            .map_err(|e| e.to_string())?;
        statement.bind((2, c.revision)).map_err(|e| e.to_string())?;
        statement
            .bind((3, i64::from(c.dirty)))
            .map_err(|e| e.to_string())?;
        statement
            .bind((4, c.entries.len() as i64))
            .map_err(|e| e.to_string())?;
        statement
            .bind((5, digest.as_str()))
            .map_err(|e| e.to_string())?;
        statement
            .bind((6, payload.as_str()))
            .map_err(|e| e.to_string())?;
        statement.next().map_err(|e| e.to_string())?;
        drop(statement);
        conn.execute(
            "DELETE FROM drive_local_time_capsules
             WHERE seq NOT IN (
                SELECT seq FROM drive_local_time_capsules ORDER BY seq DESC LIMIT 12
             )",
        )
        .map_err(|e| e.to_string())?;
        let latest = list_infos(conn)?
            .into_iter()
            .next()
            .ok_or("Saved Drive history record was not persisted")?;
        if latest.fingerprint != digest || latest.revision != c.revision {
            return Err("New Drive history snapshot was not committed correctly".into());
        }
        Ok(latest)
    })();
    match result {
        Ok(info) => {
            conn.execute("RELEASE drive_time_capsule_capture")
                .map_err(|e| e.to_string())?;
            Ok(info)
        }
        Err(err) => {
            conn.execute(
                "ROLLBACK TO drive_time_capsule_capture; RELEASE drive_time_capsule_capture",
            )
            .map_err(|e| format!("Drive snapshot failed: {err}; rollback: {e}"))?;
            Err(err)
        }
    }
}
pub(crate) fn compare(
    before: CapsuleInfo,
    left: Capsule,
    after: CapsuleInfo,
    right: Capsule,
) -> CapsuleComparison {
    let old: BTreeMap<_, _> = left
        .entries
        .iter()
        .map(|e| ((e.kind.as_str(), e.id.as_str()), e))
        .collect();
    let new: BTreeMap<_, _> = right
        .entries
        .iter()
        .map(|e| ((e.kind.as_str(), e.id.as_str()), e))
        .collect();
    let mut result = CapsuleComparison {
        before,
        after,
        created: 0,
        removed: 0,
        moved: 0,
        renamed: 0,
        ancestor_path_changed: 0,
        content_metadata_changed: 0,
        availability_changed: 0,
        backing_state_changed: 0,
        changes: Vec::new(),
    };
    for (key, entry) in &new {
        match old.get(key) {
            None => {
                result.created += 1;
                result.changes.push(CapsuleChange {
                    change: "created",
                    kind: entry.kind.clone(),
                    old_path: None,
                    new_path: Some(entry.path.clone()),
                    previous_backing: None,
                    current_backing: Some(entry.backing.clone()),
                });
            }
            Some(previous) => {
                let change = if previous.parent != entry.parent {
                    "moved"
                } else if previous.name != entry.name {
                    "renamed"
                } else if previous.path != entry.path {
                    "ancestor_path_changed"
                } else if previous.backing != entry.backing {
                    if previous.backing.ends_with("_unavailable")
                        != entry.backing.ends_with("_unavailable")
                    {
                        "availability_changed"
                    } else {
                        "backing_state_changed"
                    }
                } else if previous.size != entry.size || previous.fingerprint != entry.fingerprint {
                    "content_metadata_changed"
                } else {
                    continue;
                };
                match change {
                    "moved" => result.moved += 1,
                    "renamed" => result.renamed += 1,
                    "ancestor_path_changed" => result.ancestor_path_changed += 1,
                    "availability_changed" => result.availability_changed += 1,
                    "backing_state_changed" => result.backing_state_changed += 1,
                    _ => result.content_metadata_changed += 1,
                }
                result.changes.push(CapsuleChange {
                    change,
                    kind: entry.kind.clone(),
                    old_path: Some(previous.path.clone()),
                    new_path: Some(entry.path.clone()),
                    previous_backing: Some(previous.backing.clone()),
                    current_backing: Some(entry.backing.clone()),
                });
            }
        }
    }
    for (key, entry) in old {
        if !new.contains_key(&key) {
            result.removed += 1;
            result.changes.push(CapsuleChange {
                change: "removed",
                kind: entry.kind.clone(),
                old_path: Some(entry.path.clone()),
                new_path: None,
                previous_backing: Some(entry.backing.clone()),
                current_backing: None,
            });
        }
    }
    result.changes.sort_by(|a, b| {
        a.change
            .cmp(b.change)
            .then(a.new_path.cmp(&b.new_path))
            .then(a.old_path.cmp(&b.old_path))
    });
    result
}
#[tauri::command]
pub fn cmd_drive_time_capsule_list(
    db: State<'_, DbConnection>,
) -> Result<Vec<CapsuleInfo>, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    list_infos(&conn)
}
#[tauri::command]
pub fn cmd_drive_time_capsule_capture(db: State<'_, DbConnection>) -> Result<CapsuleInfo, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    let c = capture_snapshot(&conn)?;
    save(&conn, c)
}
#[tauri::command]
pub fn cmd_drive_time_capsule_compare(
    db: State<'_, DbConnection>,
    before_seq: i64,
    after_seq: i64,
) -> Result<CapsuleComparison, String> {
    if before_seq <= 0 || after_seq <= before_seq {
        return Err("Choose earlier and later local Drive captures".into());
    }
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    let (before, left) = load(&conn, before_seq)?;
    let (after, right) = load(&conn, after_seq)?;
    Ok(compare(before, left, after, right))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;
    fn db() -> sqlite::Connection {
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
                status TEXT NOT NULL DEFAULT 'complete');"
        ).unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
                VALUES('channel',5,'QA','owner',1,1,1);
             INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
                VALUES('docs',NULL,'Docs',1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
                total_size,chunk_count,created_at,status)
                VALUES('good','channel',9,'safe.txt',16,1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
                VALUES('entry1','good','docs','safe.txt',1,1);"
        ).unwrap();
        db
    }
    #[test]
    fn captures_raw_unavailable_backing_without_false_deletion() {
        let db = db();
        let before = capture_snapshot(&db).unwrap();
        db.execute("UPDATE logical_files SET status='missing' WHERE file_id='good'")
            .unwrap();
        let after = capture_snapshot(&db).unwrap();
        let file = after.entries.iter().find(|e| e.id == "entry1").unwrap();
        assert_eq!(file.backing, "legacy_unavailable");
        let compare = compare(
            CapsuleInfo {
                seq: 1,
                captured_at: 1,
                revision: 0,
                dirty: false,
                item_count: before.entries.len(),
                fingerprint: "".into(),
            },
            before,
            CapsuleInfo {
                seq: 2,
                captured_at: 2,
                revision: 0,
                dirty: false,
                item_count: after.entries.len(),
                fingerprint: "".into(),
            },
            after,
        );
        assert_eq!(compare.removed, 0);
        assert_eq!(compare.availability_changed, 1);
    }
    #[test]
    fn historical_metadata_rename_and_move_show_stable_identity() {
        let db = db();
        let original = capture_snapshot(&db).unwrap();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
                VALUES('archive',NULL,'Archive',1,1);
             UPDATE drive_file_entries SET directory_id='archive',display_name='renamed.txt'
                WHERE entry_id='entry1';",
        )
        .unwrap();
        let changed = capture_snapshot(&db).unwrap();
        let diff = compare(
            CapsuleInfo {
                seq: 1,
                captured_at: 1,
                revision: 0,
                dirty: false,
                item_count: 0,
                fingerprint: String::new(),
            },
            original,
            CapsuleInfo {
                seq: 2,
                captured_at: 2,
                revision: 0,
                dirty: false,
                item_count: 0,
                fingerprint: String::new(),
            },
            changed,
        );
        assert_eq!(diff.moved, 1);
        assert_eq!(diff.created, 1);
        assert!(diff.changes.iter().any(|c| c.change == "moved"
            && c.old_path.as_deref() == Some("/Docs/safe.txt")
            && c.new_path.as_deref() == Some("/Archive/renamed.txt")));
    }
    #[test]
    fn ancestor_rename_is_not_misreported_as_individual_file_rename() {
        let db = db();
        let before = capture_snapshot(&db).unwrap();
        db.execute("UPDATE drive_directories SET name='Documents' WHERE id='docs'")
            .unwrap();
        let after = capture_snapshot(&db).unwrap();
        let result = compare(
            CapsuleInfo {
                seq: 1,
                captured_at: 1,
                revision: 0,
                dirty: false,
                item_count: 0,
                fingerprint: String::new(),
            },
            before,
            CapsuleInfo {
                seq: 2,
                captured_at: 2,
                revision: 0,
                dirty: false,
                item_count: 0,
                fingerprint: String::new(),
            },
            after,
        );
        assert_eq!(result.renamed, 1);
        assert_eq!(result.ancestor_path_changed, 1);
        assert!(result
            .changes
            .iter()
            .any(|x| x.change == "ancestor_path_changed"
                && x.old_path.as_deref() == Some("/Docs/safe.txt")
                && x.new_path.as_deref() == Some("/Documents/safe.txt")));
    }
    #[test]
    fn corruption_checksum_fails_closed() {
        let db = db();
        let saved = save(&db, capture_snapshot(&db).unwrap()).unwrap();
        let (_, capsule) = load(&db, saved.seq).unwrap();
        assert_eq!(capsule.entries.len(), 2);
        db.execute("UPDATE drive_local_time_capsules SET payload='{}'")
            .unwrap();
        assert!(load(&db, saved.seq).is_err());
    }
    #[test]
    fn retains_only_twelve_capsules_and_compares_saved_pair() {
        let db = db();
        for _ in 0..14 {
            save(&db, capture_snapshot(&db).unwrap()).unwrap();
        }
        let history = list_infos(&db).unwrap();
        assert_eq!(history.len(), 12);
        assert!(load(&db, 1).is_err());
        let (a, x) = load(&db, history[1].seq).unwrap();
        let (b, y) = load(&db, history[0].seq).unwrap();
        let diff = compare(a, x, b, y);
        assert_eq!(diff.changes.len(), 0);
    }
    #[test]
    fn cycle_or_orphan_fails_without_publishing_capture() {
        let db = db();
        db.execute("UPDATE drive_directories SET parent_id='docs' WHERE id='docs'")
            .unwrap();
        assert!(capture_snapshot(&db).is_err());
        assert!(list_infos(&db).unwrap().is_empty());
    }
    #[test]
    fn v2_tombstone_hides_stale_legacy_fallback_from_history() {
        let db = db();
        db.execute(
            "INSERT INTO drive_objects(object_id,plaintext_size,encryption_version,
              crypto_id,content_fingerprint,block_count,created_at)
             VALUES('object2',16,0,NULL,'hash',0,1);
             INSERT INTO drive_entries_v2(entry_id,object_id,inode_id,directory_id,
              display_name,created_at,updated_at,trashed_at)
             VALUES('entry1','object2',NULL,'docs','safe.txt',1,4,100);",
        )
        .unwrap();
        let snapshot = capture_snapshot(&db).unwrap();
        assert!(!snapshot.entries.iter().any(|e| e.id == "entry1"));
    }
    #[test]
    fn authoritative_v2_keeps_identity_without_dual_counting() {
        let db = db();
        db.execute(
            "INSERT INTO drive_objects(object_id,plaintext_size,encryption_version,
               crypto_id,content_fingerprint,block_count,created_at)
             VALUES('object2',16,0,NULL,'hash',0,1);
             INSERT INTO drive_entries_v2(entry_id,object_id,inode_id,directory_id,
               display_name,created_at,updated_at)
             VALUES('entry1','object2',NULL,'docs','safe.txt',1,3);",
        )
        .unwrap();
        let capture = capture_snapshot(&db).unwrap();
        let entries = capture
            .entries
            .iter()
            .filter(|e| e.id == "entry1")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].backing, "v2");
    }
}
