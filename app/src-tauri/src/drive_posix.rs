//! Durable filesystem metadata. Content objects and private staging never follow links.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[cfg(test)]
static PERMISSION_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_permission_call_count() {
    PERMISSION_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn permission_call_count() -> usize {
    PERMISSION_CALLS.load(std::sync::atomic::Ordering::SeqCst)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveSymlinkRecord {
    pub id: String,
    pub directory_id: Option<String>,
    pub display_name: String,
    pub target: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub trashed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DrivePermissionRecord {
    pub node_kind: String,
    pub node_id: String,
    pub mode: u32,
    pub updated_at: i64,
}

pub(crate) fn init_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_symlinks (
           id TEXT PRIMARY KEY, directory_id TEXT, display_name TEXT NOT NULL,
           target TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
           trashed_at INTEGER);
         CREATE INDEX IF NOT EXISTS drive_symlinks_name
           ON drive_symlinks(directory_id,display_name) WHERE trashed_at IS NULL;
         CREATE TABLE IF NOT EXISTS drive_permissions (
           node_kind TEXT NOT NULL, node_id TEXT NOT NULL, mode INTEGER NOT NULL,
           updated_at INTEGER NOT NULL, PRIMARY KEY(node_kind,node_id));",
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn validate_target(target: &str) -> Result<(), String> {
    if target.is_empty() || target.len() > 4095 || target.as_bytes().contains(&0) {
        return Err(
            "Invalid Drive symlink target (requires 1..4095 UTF-8 bytes without NUL)".into(),
        );
    }
    Ok(())
}

pub(crate) fn validate_permission(record: &DrivePermissionRecord) -> Result<(), String> {
    if !matches!(record.node_kind.as_str(), "file" | "directory" | "pending")
        || record.node_id.trim().is_empty()
        || record.node_id.contains('\0')
        || record.mode > 0o777
        || record.updated_at < 0
    {
        return Err("Invalid Drive permission metadata".into());
    }
    Ok(())
}

pub(crate) fn permission(
    conn: &sqlite::Connection,
    kind: &str,
    id: &str,
) -> Result<Option<u16>, String> {
    #[cfg(test)]
    PERMISSION_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut s = conn
        .prepare("SELECT mode FROM drive_permissions WHERE node_kind=? AND node_id=?")
        .map_err(|e| e.to_string())?;
    s.bind((1, kind)).map_err(|e| e.to_string())?;
    s.bind((2, id)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    // SQLite column affinity is not a type constraint. Do not coerce a
    // damaged REAL/TEXT/BLOB chmod value into a plausible permission mask.
    let mode = match s.read::<sqlite::Value, _>(0).map_err(|e| e.to_string())? {
        sqlite::Value::Integer(value) if (0..=0o777).contains(&value) => value as u16,
        _ => return Err("Invalid stored Drive permissions".into()),
    };
    Ok(Some(mode))
}

fn node_exists(conn: &sqlite::Connection, kind: &str, id: &str) -> Result<bool, String> {
    let query = match kind {
        "directory" => "SELECT 1 FROM drive_directories WHERE id=?1 AND trashed_at IS NULL",
        "pending" => "SELECT 1 FROM drive_pending_files WHERE id=?1",
        "file" => "SELECT 1 FROM drive_file_entries e
                   WHERE e.entry_id=?1 AND e.trashed_at IS NULL
                     AND NOT EXISTS (
                         SELECT 1 FROM drive_entries_v2 shadow
                         JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                         WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                     )
                   UNION ALL
                   SELECT 1 FROM drive_entries_v2 e
                   JOIN drive_objects o ON o.object_id = e.object_id
                   WHERE e.entry_id=?1 AND e.trashed_at IS NULL",
        _ => return Err("Invalid Drive permission node kind".into()),
    };
    let mut s = conn.prepare(query).map_err(|e| e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    Ok(s.next().map_err(|e| e.to_string())? == sqlite::State::Row)
}

pub(crate) fn upsert_permission(
    conn: &sqlite::Connection,
    record: &DrivePermissionRecord,
) -> Result<(), String> {
    validate_permission(record)?;
    let mut s = conn
        .prepare(
            "INSERT INTO drive_permissions(node_kind,node_id,mode,updated_at)
        VALUES (?,?,?,?) ON CONFLICT(node_kind,node_id) DO UPDATE SET
        mode=excluded.mode, updated_at=excluded.updated_at",
        )
        .map_err(|e| e.to_string())?;
    s.bind((1, record.node_kind.as_str()))
        .map_err(|e| e.to_string())?;
    s.bind((2, record.node_id.as_str()))
        .map_err(|e| e.to_string())?;
    s.bind((3, record.mode as i64)).map_err(|e| e.to_string())?;
    s.bind((4, record.updated_at)).map_err(|e| e.to_string())?;
    s.next().map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn set_permission(
    conn: &sqlite::Connection,
    kind: &str,
    id: &str,
    mode: u32,
) -> Result<(), String> {
    if mode > 0o777 {
        return Err("Invalid Drive permissions".into());
    }
    if kind == "file" {
        // File permissions belong to the inode rather than its directory
        // entry. Existing per-entry chmod must not split a future hardlink.
        crate::commands::drive_metadata::reject_unimplemented_shared_inode_mutation(conn, id)?;
    }
    if !node_exists(conn, kind, id)? {
        return Err("Drive permission node does not exist".into());
    }
    if permission(conn, kind, id)? == Some(mode as u16) {
        return Ok(());
    }
    let mut s = conn
        .prepare("SELECT updated_at FROM drive_permissions WHERE node_kind=? AND node_id=?")
        .map_err(|e| e.to_string())?;
    s.bind((1, kind)).map_err(|e| e.to_string())?;
    s.bind((2, id)).map_err(|e| e.to_string())?;
    let previous = if s.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        match s.read::<sqlite::Value, _>(0).map_err(|e| e.to_string())? {
            sqlite::Value::Integer(value) if value >= 0 => value,
            _ => return Err("Invalid stored Drive permission timestamp".into()),
        }
    } else {
        0
    };
    drop(s);
    // Saturation could reuse i64::MAX for a changed mode; that loses the
    // monotonic revision needed to propagate chmod to the Drive manifest.
    let next_timestamp = previous
        .checked_add(1)
        .ok_or_else(|| "Drive permission timestamp cannot advance".to_string())?;
    let record = DrivePermissionRecord {
        node_kind: kind.into(),
        node_id: id.into(),
        mode,
        updated_at: chrono::Utc::now().timestamp_millis().max(next_timestamp),
    };
    conn.execute("SAVEPOINT drive_chmod")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        upsert_permission(conn, &record)?;
        if kind != "pending" {
            crate::commands::drive_metadata::mark_drive_dirty(conn)?;
        }
        Ok::<_, String>(())
    })();
    finish_savepoint(conn, "drive_chmod", result)
}

pub(crate) fn finish_savepoint<T>(
    conn: &sqlite::Connection,
    name: &str,
    result: Result<T, String>,
) -> Result<T, String> {
    match result {
        Ok(value) => {
            if let Err(e) = conn.execute(format!("RELEASE {name}")) {
                let _ = conn.execute(format!("ROLLBACK TO {name}; RELEASE {name}"));
                return Err(e.to_string());
            }
            Ok(value)
        }
        Err(error) => {
            conn.execute(format!("ROLLBACK TO {name}; RELEASE {name}"))
                .map_err(|e| format!("{error}; rollback failed: {e}"))?;
            Err(error)
        }
    }
}

pub(crate) fn remove_pending_permission(conn: &sqlite::Connection, id: &str) -> Result<(), String> {
    let mut s = conn
        .prepare("DELETE FROM drive_permissions WHERE node_kind='pending' AND node_id=?")
        .map_err(|e| e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    s.next().map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn transfer_pending_permission(
    conn: &sqlite::Connection,
    pending: &str,
    entry: &str,
) -> Result<(), String> {
    if let Some(mode) = permission(conn, "pending", pending)? {
        set_permission(conn, "file", entry, mode as u32)?;
    }
    remove_pending_permission(conn, pending)
}

/// chmod is independent of content writes. Discarding an unchanged replacement
/// must retain its accepted mode while an already-trashed base needs no update.
pub(crate) fn retain_pending_permission(
    conn: &sqlite::Connection,
    pending: &str,
    base: &str,
) -> Result<(), String> {
    if node_exists(conn, "file", base)? {
        transfer_pending_permission(conn, pending, base)?;
    }
    Ok(())
}

// SQLite's read::<String> may coerce BLOB/integer storage or truncate at an
// embedded NUL. Preserve the exact UTF-8 identity that permission metadata
// associates with a Drive node; do not publish a shortened/invented key.
fn read_permission_identity(s: &sqlite::Statement<'_>, column: usize) -> Result<String, String> {
    match s
        .read::<sqlite::Value, _>(column)
        .map_err(|e| e.to_string())?
    {
        sqlite::Value::String(_) => {}
        _ => return Err("Invalid stored Drive permission identity type".into()),
    }
    let bytes = s.read::<Vec<u8>, _>(column).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err("Invalid stored Drive permission identity: NUL".into());
    }
    String::from_utf8(bytes).map_err(|_| "Invalid stored Drive permission UTF-8".into())
}

pub(crate) fn load_permissions(
    conn: &sqlite::Connection,
) -> Result<Vec<DrivePermissionRecord>, String> {
    // Pending modes must never be published as permissions for uncommitted bytes.
    // Only retained entry identities (including tombstones) travel in a manifest.
    let mut s=conn.prepare("SELECT p.node_kind,p.node_id,p.mode,p.updated_at FROM drive_permissions p
      WHERE (p.node_kind='directory' AND EXISTS(SELECT 1 FROM drive_directories d WHERE d.id=p.node_id))
         OR (p.node_kind='file' AND (EXISTS(SELECT 1 FROM drive_file_entries e WHERE e.entry_id=p.node_id)
             OR EXISTS(SELECT 1 FROM drive_entries_v2 e WHERE e.entry_id=p.node_id)))
      ORDER BY p.node_kind,p.node_id").map_err(|e|e.to_string())?;
    let mut records = Vec::new();
    while s.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        // Persisted mode and revision must be genuine SQLite integers.
        // Coercing corrupt BLOB/REAL/TEXT into an i64 can silently publish
        // a plausible chmod and overwrite another device's newer metadata.
        let mode = match s.read::<sqlite::Value, _>(2).map_err(|e| e.to_string())? {
            sqlite::Value::Integer(value) if (0..=0o777).contains(&value) => value as u32,
            _ => return Err("Invalid stored Drive permission mode".to_string()),
        };
        let updated_at = match s.read::<sqlite::Value, _>(3).map_err(|e| e.to_string())? {
            sqlite::Value::Integer(value) if value >= 0 => value,
            _ => return Err("Invalid stored Drive permission timestamp".to_string()),
        };
        let r = DrivePermissionRecord {
            node_kind: read_permission_identity(&s, 0)?,
            node_id: read_permission_identity(&s, 1)?,
            mode,
            updated_at,
        };
        validate_permission(&r)?;
        records.push(r);
    }
    Ok(records)
}

/// SQLite's dynamic typing must not silently coerce a corrupt REAL/TEXT/BLOB
/// timestamp into a valid-looking symlink mtime or tombstone value.
fn read_link_timestamp(s: &sqlite::Statement<'_>, column: &str) -> Result<i64, String> {
    match s
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive symlink {column}: {e}"))?
    {
        sqlite::Value::Integer(value) if value >= 0 => Ok(value),
        _ => Err(format!("Invalid Drive symlink {column} timestamp")),
    }
}

/// Read the exact UTF-8 bytes instead of relying on SQLite's text
/// conversion, which can truncate at embedded NUL before validation.
fn read_link_text(s: &sqlite::Statement<'_>, column: &str) -> Result<String, String> {
    match s
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive symlink {column}: {e}"))?
    {
        sqlite::Value::String(_) => {}
        _ => return Err(format!("Invalid Drive symlink {column} type")),
    };
    let bytes = s
        .read::<Vec<u8>, _>(column)
        .map_err(|e| format!("Invalid Drive symlink {column}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!("Invalid Drive symlink {column}: NUL byte"));
    }
    String::from_utf8(bytes).map_err(|_| format!("Invalid Drive symlink {column}: non-UTF-8"))
}

/// Decode a symlink from either the direct table or the indexed child projection.
/// Both native paths must apply exactly the same validation.
pub(crate) fn read_link_with_target(
    s: &sqlite::Statement<'_>,
    target_column: &str,
) -> Result<DriveSymlinkRecord, String> {
    let created_at = read_link_timestamp(s, "created_at")?;
    let updated_at = read_link_timestamp(s, "updated_at")?;
    if updated_at < created_at {
        return Err("Invalid Drive symlink timestamp order".into());
    }
    let trashed_at = match s
        .read::<sqlite::Value, _>("trashed_at")
        .map_err(|e| format!("Invalid Drive symlink trashed_at: {e}"))?
    {
        sqlite::Value::Null => None,
        sqlite::Value::Integer(value) if value >= 0 => Some(value),
        _ => return Err("Invalid Drive symlink trashed_at timestamp".into()),
    };
    let directory_id = match s
        .read::<sqlite::Value, _>("directory_id")
        .map_err(|e| format!("Invalid Drive symlink directory_id: {e}"))?
    {
        sqlite::Value::Null => None,
        sqlite::Value::String(_) => Some(read_link_text(s, "directory_id")?),
        _ => return Err("Invalid Drive symlink directory_id type".into()),
    };
    let link = DriveSymlinkRecord {
        id: read_link_text(s, "id")?,
        directory_id,
        display_name: read_link_text(s, "display_name")?,
        target: read_link_text(s, target_column)?,
        created_at,
        updated_at,
        trashed_at,
    };
    if link.id.trim().is_empty() {
        return Err("Invalid Drive symlink ID".into());
    }
    if link
        .directory_id
        .as_deref()
        .is_some_and(|parent| parent.trim().is_empty())
    {
        return Err("Invalid Drive symlink parent ID".into());
    }
    crate::commands::drive_metadata::validate_drive_name(&link.display_name)?;
    validate_target(&link.target)?;
    Ok(link)
}

fn read_link(s: &sqlite::Statement<'_>) -> Result<DriveSymlinkRecord, String> {
    read_link_with_target(s, "target")
}

pub(crate) fn load_symlinks(
    conn: &sqlite::Connection,
    include_trashed: bool,
) -> Result<Vec<DriveSymlinkRecord>, String> {
    let query = if include_trashed {
        "SELECT * FROM drive_symlinks ORDER BY id"
    } else {
        "SELECT * FROM drive_symlinks WHERE trashed_at IS NULL ORDER BY id"
    };
    let mut s = conn.prepare(query).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    while s.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        rows.push(read_link(&s)?);
    }
    Ok(rows)
}

pub(crate) fn symlink_by_id(
    conn: &sqlite::Connection,
    id: &str,
) -> Result<Option<DriveSymlinkRecord>, String> {
    let mut s = conn
        .prepare("SELECT * FROM drive_symlinks WHERE id=? AND trashed_at IS NULL")
        .map_err(|e| e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    Ok(Some(read_link(&s)?))
}

pub(crate) fn upsert_symlink(
    conn: &sqlite::Connection,
    link: &DriveSymlinkRecord,
) -> Result<(), String> {
    // The local writer must enforce the same constraints as the strict native
    // reader and portable manifest. Otherwise a malformed write can poison
    // subsequent mounted lookup and metadata synchronization.
    if link.id.trim().is_empty()
        || link.id.contains('\0')
        || link
            .directory_id
            .as_ref()
            .is_some_and(|parent| parent.trim().is_empty() || parent.contains('\0'))
    {
        return Err("Invalid Drive symlink identity or parent".into());
    }
    crate::commands::drive_metadata::validate_drive_name(&link.display_name)?;
    validate_target(&link.target)?;
    if link.created_at < 0
        || link.updated_at < link.created_at
        || link.trashed_at.is_some_and(|time| time < 0)
    {
        return Err("Invalid Drive symlink timestamps".into());
    }
    let mut s=conn.prepare("INSERT INTO drive_symlinks
        (id,directory_id,display_name,target,created_at,updated_at,trashed_at) VALUES(?,?,?,?,?,?,?)
        ON CONFLICT(id) DO UPDATE SET directory_id=excluded.directory_id,display_name=excluded.display_name,
        target=excluded.target,created_at=excluded.created_at,updated_at=excluded.updated_at,trashed_at=excluded.trashed_at")
        .map_err(|e|e.to_string())?;
    s.bind((1, link.id.as_str())).map_err(|e| e.to_string())?;
    s.bind((2, link.directory_id.as_deref()))
        .map_err(|e| e.to_string())?;
    s.bind((3, link.display_name.as_str()))
        .map_err(|e| e.to_string())?;
    s.bind((4, link.target.as_str()))
        .map_err(|e| e.to_string())?;
    s.bind((5, link.created_at)).map_err(|e| e.to_string())?;
    s.bind((6, link.updated_at)).map_err(|e| e.to_string())?;
    s.bind((7, link.trashed_at)).map_err(|e| e.to_string())?;
    s.next().map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn merge_records<T: Clone + Serialize>(
    left: &[T],
    right: &[T],
    key: impl Fn(&T) -> String,
    time: impl Fn(&T) -> i64,
    is_deleted: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut rows = BTreeMap::<String, T>::new();
    for row in left.iter().chain(right) {
        let k = key(row);
        let replace = rows
            .get(&k)
            .map(|old| {
                time(row) > time(old)
                    || (time(row) == time(old)
                        && (is_deleted(row) && !is_deleted(old)
                            || (is_deleted(row) == is_deleted(old)
                                && serde_json::to_vec(row).unwrap_or_default()
                                    > serde_json::to_vec(old).unwrap_or_default())))
            })
            .unwrap_or(true);
        if replace {
            rows.insert(k, row.clone());
        }
    }
    rows.into_values().collect()
}

pub(crate) fn validate_manifest(
    manifest: &crate::commands::drive_metadata::DriveManifestV1,
    directories: &HashSet<String>,
    files: &HashSet<String>,
    namespace: &mut HashSet<(Option<String>, String)>,
) -> Result<(), String> {
    let mut ids = HashSet::new();
    for link in &manifest.symlinks {
        if link.id.trim().is_empty()
            || directories.contains(&link.id)
            || files.contains(&link.id)
            || !ids.insert(link.id.clone())
            || link
                .directory_id
                .as_deref()
                .is_some_and(|parent| parent.trim().is_empty())
        {
            return Err("Invalid or duplicate Drive symlink ID".into());
        }
        crate::commands::drive_metadata::validate_drive_name(&link.display_name)?;
        validate_target(&link.target)?;
        if link.created_at < 0
            || link.updated_at < link.created_at
            || link.trashed_at.map(|v| v < 0).unwrap_or(false)
        {
            return Err("Invalid Drive symlink timestamps".into());
        }
        if link
            .directory_id
            .as_ref()
            .map(|p| !directories.contains(p))
            .unwrap_or(false)
        {
            return Err("Drive symlink references a missing parent folder".into());
        }
        if link.trashed_at.is_none()
            && !namespace.insert((
                link.directory_id.clone(),
                crate::commands::drive_metadata::portable_name_key(&link.display_name),
            ))
        {
            return Err("Drive metadata contains colliding sibling names".into());
        }
    }
    let mut keys = HashSet::new();
    for mode in &manifest.permissions {
        validate_permission(mode)?;
        if !keys.insert((mode.node_kind.clone(), mode.node_id.clone())) {
            return Err("Duplicate Drive permission metadata".into());
        }
        let exists = match mode.node_kind.as_str() {
            "directory" => directories.contains(&mode.node_id),
            "file" => files.contains(&mode.node_id),
            _ => false,
        };
        if !exists {
            return Err("Drive permission metadata references a missing node".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod permission_load_tests {
    use super::*;

    #[test]
    fn permission_identities_reject_malformed_values_without_coercion() {
        let conn = sqlite::open(":memory:").unwrap();
        init_schema(&conn).unwrap();
        let mut record = DrivePermissionRecord {
            node_kind: "directory".into(),
            node_id: "folder-id".into(),
            mode: 0o644,
            updated_at: 3,
        };
        validate_permission(&record).unwrap();
        for invalid in ["", " ", "\t\n", "folder\0forged"] {
            record.node_id = invalid.into();
            assert!(validate_permission(&record).is_err());
            assert!(upsert_permission(&conn, &record).is_err());
        }

        let mut s = conn
            .prepare(
                "SELECT 'directory' AS kind, 'folder-id' AS valid,
                    'folder' || char(0) || 'spoof' AS nul,
                    x'646972' AS blob, 42 AS integer, 1.5 AS real",
            )
            .unwrap();
        assert_eq!(s.next().unwrap(), sqlite::State::Row);
        assert_eq!(read_permission_identity(&s, 0).unwrap(), "directory");
        assert_eq!(read_permission_identity(&s, 1).unwrap(), "folder-id");
        for column in [2, 3, 4, 5] {
            assert!(read_permission_identity(&s, column).is_err());
        }
    }

    #[test]
    fn drive_permission_loader_rejects_corrupt_matching_node_identity() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE drive_directories(id TEXT PRIMARY KEY);
             CREATE TABLE drive_file_entries(entry_id TEXT);
             CREATE TABLE drive_entries_v2(entry_id TEXT);",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id) VALUES('folder-id');
             INSERT INTO drive_permissions(node_kind,node_id,mode,updated_at)
             VALUES('directory','folder-id',420,3);",
        )
        .unwrap();
        assert_eq!(load_permissions(&conn).unwrap().len(), 1);
        conn.execute(
            "UPDATE drive_directories SET id='folder-id'||char(0)||'spoof';
             UPDATE drive_permissions SET node_id=(SELECT id FROM drive_directories);",
        )
        .unwrap();
        assert!(
            load_permissions(&conn).is_err(),
            "permission loader must not truncate NUL identities to another node"
        );
    }

    #[test]
    fn drive_permission_loader_rejects_wrapped_sqlite_modes() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE drive_directories(id TEXT PRIMARY KEY);
             CREATE TABLE drive_file_entries(entry_id TEXT);
             CREATE TABLE drive_entries_v2(entry_id TEXT);",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id) VALUES('dir-a');
             INSERT INTO drive_permissions(node_kind,node_id,mode,updated_at)
             VALUES('directory','dir-a',420,1);",
        )
        .unwrap();

        let valid = load_permissions(&conn).unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].mode, 0o644);
        assert_eq!(valid[0].node_id, "dir-a");

        // A lossy i64 -> u32 cast can silently turn these corrupt SQLite
        // values into valid modes 1 or 0. Fail closed before serialization.
        for invalid in [-4_294_967_295_i64, -1, 4_294_967_296, 4_294_967_297] {
            conn.execute(format!(
                "UPDATE drive_permissions SET mode={invalid} WHERE node_id='dir-a'"
            ))
            .unwrap();
            assert!(
                load_permissions(&conn).is_err(),
                "invalid SQLite permission mode {invalid} was accepted"
            );
        }

        // Invalid storage classes must not be silently coerced into an
        // integer chmod or exported as a timestamp to other Drive devices.
        for invalid in ["420.5", "'invalid-mode'", "x'01'"] {
            conn.execute(format!(
                "UPDATE drive_permissions SET mode={invalid} WHERE node_id='dir-a'"
            ))
            .unwrap();
            assert!(
                load_permissions(&conn).is_err(),
                "coerced SQLite mode {invalid} was accepted"
            );
        }
        conn.execute("UPDATE drive_permissions SET mode=493 WHERE node_id='dir-a'")
            .unwrap();
        assert_eq!(load_permissions(&conn).unwrap()[0].mode, 0o755);

        for invalid in ["-1", "1.5", "'invalid-time'", "x'01'"] {
            conn.execute(format!(
                "UPDATE drive_permissions SET updated_at={invalid} WHERE node_id='dir-a'"
            ))
            .unwrap();
            assert!(
                load_permissions(&conn).is_err(),
                "coerced SQLite permission timestamp {invalid} was accepted"
            );
        }
        conn.execute("UPDATE drive_permissions SET updated_at=12 WHERE node_id='dir-a'")
            .unwrap();
        assert_eq!(load_permissions(&conn).unwrap()[0].updated_at, 12);
    }

    #[test]
    fn native_chmod_lookup_rejects_coerced_sqlite_modes() {
        let conn = sqlite::open(":memory:").unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_permissions(node_kind,node_id,mode,updated_at)
             VALUES('pending','pending-a',420,10)",
        )
        .unwrap();
        assert_eq!(
            permission(&conn, "pending", "pending-a").unwrap(),
            Some(0o644)
        );
        assert_eq!(permission(&conn, "pending", "missing").unwrap(), None);

        for invalid in ["-1", "512", "420.5", "'bad-mode'", "x'01'"] {
            conn.execute(format!(
                "UPDATE drive_permissions SET mode={invalid} WHERE node_id='pending-a'"
            ))
            .unwrap();
            assert!(
                permission(&conn, "pending", "pending-a").is_err(),
                "native chmod lookup coerced corrupt SQLite mode {invalid}"
            );
        }
        conn.execute("UPDATE drive_permissions SET mode=493 WHERE node_id='pending-a'")
            .unwrap();
        assert_eq!(
            permission(&conn, "pending", "pending-a").unwrap(),
            Some(0o755)
        );
    }

    #[test]
    fn native_chmod_rejects_corrupt_or_exhausted_revision_before_write() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute("CREATE TABLE drive_pending_files(id TEXT PRIMARY KEY)")
            .unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_pending_files(id) VALUES('pending-a');
             INSERT INTO drive_permissions(node_kind,node_id,mode,updated_at)
             VALUES('pending','pending-a',420,10);",
        )
        .unwrap();

        for invalid in ["-1", "10.5", "'bad-time'", "x'01'", "9223372036854775807"] {
            conn.execute(format!(
                "UPDATE drive_permissions SET updated_at={invalid} WHERE node_id='pending-a'"
            ))
            .unwrap();
            assert!(
                set_permission(&conn, "pending", "pending-a", 0o755).is_err(),
                "native chmod accepted corrupt revision {invalid}"
            );
            assert_eq!(
                permission(&conn, "pending", "pending-a").unwrap(),
                Some(0o644),
                "failed chmod must not change the existing mode"
            );
        }

        conn.execute("UPDATE drive_permissions SET updated_at=10 WHERE node_id='pending-a'")
            .unwrap();
        set_permission(&conn, "pending", "pending-a", 0o755).unwrap();
        assert_eq!(
            permission(&conn, "pending", "pending-a").unwrap(),
            Some(0o755)
        );
        let mut stmt = conn
            .prepare("SELECT updated_at FROM drive_permissions WHERE node_id='pending-a'")
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert!(stmt.read::<i64, _>(0).unwrap() > 10);
    }
}

#[cfg(test)]
mod symlink_load_tests {
    use super::*;

    fn fixture() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_symlinks
             (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
             VALUES('link-a',NULL,'shortcut','relative/target',10,20,NULL)",
        )
        .unwrap();
        conn
    }

    #[test]
    fn native_symlink_reads_fail_closed_on_corrupt_metadata() {
        let conn = fixture();
        let expected = symlink_by_id(&conn, "link-a").unwrap().unwrap();
        assert_eq!(expected.created_at, 10);
        assert_eq!(expected.updated_at, 20);
        assert_eq!(load_symlinks(&conn, false).unwrap(), vec![expected.clone()]);
        assert_eq!(load_symlinks(&conn, true).unwrap(), vec![expected]);

        // SQL column affinity does not guarantee storage class: all of these
        // invalid values must be rejected by both the direct lookup used by
        // mounted readlink and the bulk manifest publication path.
        for (column, value) in [
            ("created_at", "-1"),
            ("created_at", "1.5"),
            ("created_at", "'ten'"),
            ("created_at", "x'01'"),
            ("updated_at", "-1"),
            ("updated_at", "9"),
            ("updated_at", "20.5"),
            ("updated_at", "'later'"),
            ("updated_at", "x'01'"),
            ("display_name", "''"),
            ("display_name", "'bad/name'"),
            ("display_name", "'short' || char(0) || 'cut'"),
            ("display_name", "x'616263'"),
            ("target", "''"),
            ("target", "'bad' || char(0)"),
            ("target", "x'616263'"),
            ("target", "printf('%04096d', 0)"),
            ("directory_id", "x'616263'"),
            ("directory_id", "''"),
            ("directory_id", "'   '"),
            ("directory_id", "char(9) || char(10)"),
            ("directory_id", "'home' || char(0) || 'bad'"),
        ] {
            conn.execute(format!(
                "UPDATE drive_symlinks SET created_at=10,updated_at=20,
                 directory_id=NULL,display_name='shortcut',target='relative/target',
                 {column}={value} WHERE id='link-a'"
            ))
            .unwrap();
            assert!(
                symlink_by_id(&conn, "link-a").is_err(),
                "direct symlink lookup accepted {column}={value}"
            );
            assert!(
                load_symlinks(&conn, false).is_err(),
                "symlink manifest loader accepted {column}={value}"
            );
        }

        for (id, label) in [("''", "empty"), ("'   '", "whitespace-only")] {
            conn.execute(format!(
                "UPDATE drive_symlinks SET created_at=10,updated_at=20,
                 directory_id=NULL,display_name='shortcut',target='relative/target',id={id}
                 WHERE id='link-a'"
            ))
            .unwrap();
            assert!(
                load_symlinks(&conn, false).is_err(),
                "{label} link ID accepted"
            );
            conn.execute(format!(
                "UPDATE drive_symlinks SET id='link-a' WHERE id={id}"
            ))
            .unwrap();
        }
    }

    #[test]
    fn native_symlink_upsert_rejects_invalid_rows_without_poisoning_metadata() {
        let conn = fixture();
        let original = symlink_by_id(&conn, "link-a").unwrap().unwrap();
        for case in 0..15 {
            let mut invalid = original.clone();
            match case {
                0 => invalid.id.clear(),
                1 => invalid.id = "bad\0id".to_string(),
                2 => invalid.directory_id = Some(String::new()),
                3 => invalid.directory_id = Some("bad\0parent".to_string()),
                4 => invalid.display_name.clear(),
                5 => invalid.display_name = "bad/name".to_string(),
                6 => invalid.display_name = "CON".to_string(),
                7 => invalid.target.clear(),
                8 => invalid.target = "bad\0target".to_string(),
                9 => invalid.created_at = -1,
                10 => invalid.updated_at = original.created_at - 1,
                11 => invalid.trashed_at = Some(-1),
                12 => invalid.id = "   ".to_string(),
                13 => invalid.directory_id = Some("   ".to_string()),
                14 => invalid.directory_id = Some("\t\n".to_string()),
                _ => unreachable!(),
            }
            assert!(
                upsert_symlink(&conn, &invalid).is_err(),
                "invalid symlink write case {case} was accepted"
            );
            assert_eq!(
                load_symlinks(&conn, true).unwrap(),
                vec![original.clone()],
                "invalid write case {case} changed persisted symlink metadata"
            );
        }

        let mut updated = original;
        updated.display_name = "renamed-shortcut".to_string();
        updated.updated_at += 1;
        updated.trashed_at = Some(30);
        upsert_symlink(&conn, &updated).unwrap();
        assert_eq!(load_symlinks(&conn, true).unwrap(), vec![updated]);
        assert!(load_symlinks(&conn, false).unwrap().is_empty());
    }

    #[test]
    fn native_symlink_tombstones_require_nonnegative_integer_time() {
        let conn = fixture();
        for value in ["-1", "1.5", "'broken'", "x'01'"] {
            conn.execute(format!(
                "UPDATE drive_symlinks SET trashed_at={value} WHERE id='link-a'"
            ))
            .unwrap();
            assert!(
                load_symlinks(&conn, true).is_err(),
                "corrupt symlink tombstone {value} was published"
            );
        }

        conn.execute("UPDATE drive_symlinks SET trashed_at=30 WHERE id='link-a'")
            .unwrap();
        assert_eq!(load_symlinks(&conn, true).unwrap()[0].trashed_at, Some(30));
        assert!(load_symlinks(&conn, false).unwrap().is_empty());
        assert!(symlink_by_id(&conn, "link-a").unwrap().is_none());

        conn.execute("UPDATE drive_symlinks SET trashed_at=NULL WHERE id='link-a'")
            .unwrap();
        assert_eq!(
            symlink_by_id(&conn, "link-a").unwrap().unwrap().trashed_at,
            None
        );
    }
}
