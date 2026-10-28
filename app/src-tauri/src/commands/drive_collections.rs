//! Device-local Drive Virtual Collections. Membership bookmarks reference stable
//! entry IDs; they never move, delete, copy, upload, or mutate Drive objects.
//! These tables deliberately do NOT participate in Telegram manifest sync.
#[cfg(test)]
use crate::commands::drive_metadata::drive_snapshot;
use crate::db::DbConnection;
use serde::Serialize;
use std::collections::HashSet;
use tauri::State;

const MAX_COLLECTIONS: usize = 100;
const MAX_MEMBERS: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct DriveCollection {
    pub id: String,
    pub name: String,
    pub revision: i64,
    pub entry_ids: Vec<String>,
}

fn schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_local_collections (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL COLLATE NOCASE UNIQUE,
            revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1),
            created_at INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS drive_local_collection_members (
            collection_id TEXT NOT NULL,
            entry_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            PRIMARY KEY(collection_id, entry_id));",
    )
    .map_err(|e| e.to_string())
}

fn check_id(id: &str) -> Result<(), String> {
    if id.len() != "collection-".len() + 32
        || !id.starts_with("collection-")
        || !id["collection-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid collection identity; refresh and select a saved collection.".into());
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 100
        || name.trim() != name
        || name.chars().count() > 80
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(
            "Collection name must be 1–80 clean characters, without path separators.".into(),
        );
    }
    Ok(())
}

fn is_pending_replacement(conn: &sqlite::Connection, id: &str) -> Result<bool, String> {
    // An unfinalized copy-on-write edit shadows its base entry in the
    // authoritative Drive snapshot, even if a direct file-ID lookup finds it.
    let mut query = conn
        .prepare("SELECT 1 FROM drive_pending_files WHERE base_entry_id=? LIMIT 1")
        .map_err(|e| e.to_string())?;
    query.bind((1, id)).map_err(|e| e.to_string())?;
    Ok(query.next().map_err(|e| e.to_string())? == sqlite::State::Row)
}

fn check_members(conn: &sqlite::Connection, ids: &[String]) -> Result<(), String> {
    if ids.len() > MAX_MEMBERS {
        return Err(format!(
            "A collection can contain at most {MAX_MEMBERS} Drive files."
        ));
    }
    // Resolve only the selected stable file identities instead of enumerating
    // the entire Drive snapshot for an edit of at most MAX_MEMBERS references.
    // The lookup excludes incomplete/trashed files and validates their schema.
    let mut seen = HashSet::new();
    for id in ids {
        if id.is_empty()
            || id.len() > 256
            || !seen.insert(id.as_str())
            || crate::drive_lookup::file(conn, id)?.is_none()
            || is_pending_replacement(conn, id)?
        {
            return Err(
                "Collection contains an unavailable, unfinished, or duplicate Drive entry.".into(),
            );
        }
    }
    Ok(())
}

fn list(conn: &sqlite::Connection) -> Result<Vec<DriveCollection>, String> {
    schema(conn)?;
    let mut query = conn
        .prepare(
            "SELECT id, name, revision FROM drive_local_collections
         ORDER BY name COLLATE NOCASE, id LIMIT ?",
        )
        .map_err(|e| e.to_string())?;
    query
        .bind((1, (MAX_COLLECTIONS + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    while query.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if results.len() == MAX_COLLECTIONS {
            return Err("Too many saved Drive collections; inspection is bounded.".into());
        }
        let id = query.read::<String, _>("id").map_err(|e| e.to_string())?;
        let name = query.read::<String, _>("name").map_err(|e| e.to_string())?;
        let revision = query
            .read::<i64, _>("revision")
            .map_err(|e| e.to_string())?;
        check_name(&name)?;
        if check_id(&id).is_err() || revision < 1 {
            return Err("Invalid saved Drive collection metadata.".into());
        }
        let mut members = conn
            .prepare(
                "SELECT entry_id FROM drive_local_collection_members
             WHERE collection_id=? ORDER BY position LIMIT ?",
            )
            .map_err(|e| e.to_string())?;
        members.bind((1, id.as_str())).map_err(|e| e.to_string())?;
        members
            .bind((2, (MAX_MEMBERS + 1) as i64))
            .map_err(|e| e.to_string())?;
        let mut entry_ids = Vec::new();
        while members.next().map_err(|e| e.to_string())? == sqlite::State::Row {
            if entry_ids.len() == MAX_MEMBERS {
                return Err("Saved Drive collection exceeds safety cap.".into());
            }
            let item = members.read::<String, _>(0).map_err(|e| e.to_string())?;
            if item.is_empty() || item.len() > 256 {
                return Err("Invalid saved Drive collection entry identity.".into());
            }
            entry_ids.push(item);
        }
        results.push(DriveCollection {
            id,
            name,
            revision,
            entry_ids,
        });
    }
    Ok(results)
}

fn in_savepoint<T>(
    conn: &sqlite::Connection,
    run: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    conn.execute("SAVEPOINT drive_local_collection_edit")
        .map_err(|e| e.to_string())?;
    match run() {
        Ok(value) => {
            if let Err(e) = conn.execute("RELEASE drive_local_collection_edit") {
                let _ = conn.execute(
                    "ROLLBACK TO drive_local_collection_edit; RELEASE drive_local_collection_edit",
                );
                return Err(e.to_string());
            }
            Ok(value)
        }
        Err(problem) => {
            conn.execute(
                "ROLLBACK TO drive_local_collection_edit; RELEASE drive_local_collection_edit",
            )
            .map_err(|e| format!("{problem}; rollback failed: {e}"))?;
            Err(problem)
        }
    }
}

fn insert_members(
    conn: &sqlite::Connection,
    collection_id: &str,
    ids: &[String],
) -> Result<(), String> {
    for (position, id) in ids.iter().enumerate() {
        let mut insert = conn.prepare(
            "INSERT INTO drive_local_collection_members (collection_id,entry_id,position) VALUES (?,?,?)",
        ).map_err(|e| e.to_string())?;
        insert.bind((1, collection_id)).map_err(|e| e.to_string())?;
        insert.bind((2, id.as_str())).map_err(|e| e.to_string())?;
        insert
            .bind((3, position as i64))
            .map_err(|e| e.to_string())?;
        insert.next().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn save(
    conn: &sqlite::Connection,
    collection_id: Option<&str>,
    expected_revision: Option<i64>,
    name: &str,
    entry_ids: &[String],
) -> Result<DriveCollection, String> {
    schema(conn)?;
    check_name(name)?;
    if collection_id.is_none() && expected_revision.is_some() {
        return Err("Creating a new collection must not contain an old revision.".into());
    }
    check_members(conn, entry_ids)?;
    if let Some(id) = collection_id {
        check_id(id)?;
    }
    let id = collection_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("collection-{:032x}", rand::random::<u128>()));
    let revision = if let Some(id) = collection_id {
        let expected = expected_revision.ok_or("Missing saved collection revision")?;
        if expected < 1 {
            return Err("Invalid saved collection revision".into());
        }
        let mut stmt = conn
            .prepare("SELECT revision FROM drive_local_collections WHERE id=?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, id)).map_err(|e| e.to_string())?;
        if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
            return Err("Collection was removed; refresh before editing.".into());
        }
        let actual = stmt.read::<i64, _>(0).map_err(|e| e.to_string())?;
        if actual != expected {
            return Err("Collection changed since review. Refresh before saving.".into());
        }
        expected
            .checked_add(1)
            .ok_or("Collection revision overflow")?
    } else {
        let mut stmt = conn
            .prepare("SELECT count(*) FROM drive_local_collections")
            .map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        if stmt.read::<i64, _>(0).map_err(|e| e.to_string())? >= MAX_COLLECTIONS as i64 {
            return Err("Drive already has the maximum number of collections.".into());
        }
        1
    };
    in_savepoint(conn, || {
        if collection_id.is_some() {
            let mut stmt = conn
                .prepare("UPDATE drive_local_collections SET name=?, revision=? WHERE id=?")
                .map_err(|e| e.to_string())?;
            stmt.bind((1, name)).map_err(|e| e.to_string())?;
            stmt.bind((2, revision)).map_err(|e| e.to_string())?;
            stmt.bind((3, id.as_str())).map_err(|e| e.to_string())?;
            stmt.next().map_err(|e| e.to_string())?;
            let mut clear = conn
                .prepare("DELETE FROM drive_local_collection_members WHERE collection_id=?")
                .map_err(|e| e.to_string())?;
            clear.bind((1, id.as_str())).map_err(|e| e.to_string())?;
            clear.next().map_err(|e| e.to_string())?;
        } else {
            let mut stmt = conn
                .prepare("INSERT INTO drive_local_collections(id,name,revision) VALUES (?,?,1)")
                .map_err(|e| e.to_string())?;
            stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
            stmt.bind((2, name)).map_err(|e| e.to_string())?;
            stmt.next().map_err(|e| e.to_string())?;
        }
        insert_members(conn, &id, entry_ids)?;
        Ok(())
    })?;
    Ok(DriveCollection {
        id,
        name: name.into(),
        revision,
        entry_ids: entry_ids.to_vec(),
    })
}

fn remove(conn: &sqlite::Connection, id: &str, expected_revision: i64) -> Result<(), String> {
    check_id(id)?;
    schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT revision FROM drive_local_collections WHERE id=?")
        .map_err(|e| e.to_string())?;
    stmt.bind((1, id)).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row
        || stmt.read::<i64, _>(0).map_err(|e| e.to_string())? != expected_revision
    {
        return Err("Collection changed or was removed. Refresh before deleting it.".into());
    }
    // Finalize the reader before opening a write transaction.
    drop(stmt);
    in_savepoint(conn, || {
        let mut entries = conn
            .prepare("DELETE FROM drive_local_collection_members WHERE collection_id=?")
            .map_err(|e| e.to_string())?;
        entries.bind((1, id)).map_err(|e| e.to_string())?;
        entries.next().map_err(|e| e.to_string())?;
        let mut entry = conn
            .prepare("DELETE FROM drive_local_collections WHERE id=?")
            .map_err(|e| e.to_string())?;
        entry.bind((1, id)).map_err(|e| e.to_string())?;
        entry.next().map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn cmd_drive_collections_list(
    db: State<'_, DbConnection>,
) -> Result<Vec<DriveCollection>, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    list(&conn)
}

#[tauri::command]
pub fn cmd_drive_collections_save(
    db: State<'_, DbConnection>,
    collection_id: Option<String>,
    expected_revision: Option<i64>,
    name: String,
    entry_ids: Vec<String>,
) -> Result<DriveCollection, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    save(
        &conn,
        collection_id.as_deref(),
        expected_revision,
        &name,
        &entry_ids,
    )
}

#[tauri::command]
pub fn cmd_drive_collections_delete(
    db: State<'_, DbConnection>,
    collection_id: String,
    expected_revision: i64,
) -> Result<(), String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    remove(&conn, &collection_id, expected_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        fixture_on(sqlite::open(":memory:").unwrap())
    }

    fn fixture_on(conn: sqlite::Connection) -> sqlite::Connection {
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
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,created_at,joined_at)
                VALUES('col-channel',971,'QA',1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,created_at)
                VALUES('file-one','col-channel',11,'one.txt',5,1),
                      ('file-two','col-channel',12,'two.txt',6,1);",
        ).unwrap();
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_file_entries(entry_id,file_id,display_name,created_at,updated_at)
              VALUES('entry-one','file-one','one.txt',1,1),
                    ('entry-two','file-two','two.txt',1,1);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn collections_survive_sqlite_close_and_reopen_without_drive_mutation() {
        let path = std::env::temp_dir().join(format!(
            "terarelay-collections-{:032x}.sqlite",
            rand::random::<u128>()
        ));
        let id = {
            let conn = fixture_on(sqlite::open(&path).unwrap());
            let saved = save(
                &conn,
                None,
                None,
                "Persistent collection",
                &["entry-one".into()],
            )
            .unwrap();
            assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 2);
            saved.id
        };
        {
            let conn = sqlite::open(&path).unwrap();
            let saved = list(&conn).unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].id, id);
            assert_eq!(saved[0].entry_ids, ["entry-one"]);
            assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 2);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn saved_memberships_persist_without_renaming_or_moving_file_entries() {
        let conn = fixture();
        let items = vec!["entry-one".into(), "entry-two".into()];
        let created = save(&conn, None, None, "Project Alpha", &items).unwrap();
        assert_eq!((created.revision, created.entry_ids.len()), (1, 2));
        let reloaded = list(&conn).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].entry_ids, items);
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 2);
    }

    #[test]
    fn malformed_create_revision_and_untrusted_collection_id_cannot_mutate() {
        let conn = fixture();
        assert!(save(&conn, None, Some(42), "Bad create", &[]).is_err());
        assert!(save(&conn, Some("../other"), Some(1), "Bad edit", &[]).is_err());
        assert!(remove(&conn, "../other", 1).is_err());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn membership_lookup_rejects_trashed_and_incomplete_files_without_scan() {
        let conn = fixture();
        conn.execute("UPDATE drive_file_entries SET trashed_at=99 WHERE entry_id='entry-one'")
            .unwrap();
        conn.execute("UPDATE logical_files SET status='uploading' WHERE file_id='file-two'")
            .unwrap();
        assert!(save(
            &conn,
            None,
            None,
            "Cannot save trash",
            &["entry-one".into()]
        )
        .is_err());
        assert!(save(
            &conn,
            None,
            None,
            "Cannot save pending",
            &["entry-two".into()]
        )
        .is_err());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn pending_copy_on_write_shadows_base_file_from_collection_save() {
        let conn = fixture();
        assert!(crate::drive_lookup::file(&conn, "entry-one")
            .unwrap()
            .is_some());
        conn.execute(
            "INSERT INTO drive_pending_files
             (id,display_name,staging_path,size,backing_channel_id,base_entry_id,created_at,updated_at)
             VALUES ('qa-collection-cow','editing.txt','private-fixture',5,971,'entry-one',1,1);",
        ).unwrap();
        assert!(save(&conn, None, None, "Unsafe COW", &["entry-one".into()]).is_err());
        assert!(save(&conn, None, None, "Safe other", &["entry-two".into()]).is_ok());
        assert_eq!(list(&conn).unwrap().len(), 1);
    }

    #[test]
    fn stale_revision_and_missing_identity_rejected_without_mutation() {
        let conn = fixture();
        let created = save(&conn, None, None, "Alpha", &["entry-one".into()]).unwrap();
        assert!(save(&conn, Some(&created.id), Some(999), "Corrupt", &[]).is_err());
        assert!(save(
            &conn,
            Some(&created.id),
            Some(1),
            "Renamed",
            &["missing".into()]
        )
        .is_err());
        assert_eq!(list(&conn).unwrap()[0].name, "Alpha");
        let updated = save(
            &conn,
            Some(&created.id),
            Some(1),
            "Renamed",
            &["entry-two".into()],
        )
        .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(list(&conn).unwrap()[0].entry_ids, ["entry-two"]);
    }

    #[test]
    fn cannot_overwrite_collection_name_and_duplicate_members() {
        let conn = fixture();
        save(&conn, None, None, "Alpha", &[]).unwrap();
        assert!(save(&conn, None, None, "alpha", &[]).is_err());
        assert!(save(
            &conn,
            None,
            None,
            "Other",
            &["entry-one".into(), "entry-one".into()]
        )
        .is_err());
        assert_eq!(list(&conn).unwrap().len(), 1);
    }

    #[test]
    fn deleting_collection_never_deletes_drive_file() {
        let conn = fixture();
        let created = save(&conn, None, None, "Alpha", &["entry-one".into()]).unwrap();
        assert!(remove(&conn, &created.id, 2).is_err());
        remove(&conn, &created.id, 1).unwrap();
        assert!(list(&conn).unwrap().is_empty());
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 2);
    }

    #[test]
    fn atomic_membership_rewrite_rolls_back_failed_second_insert() {
        let conn = fixture();
        let created = save(&conn, None, None, "Alpha", &["entry-one".into()]).unwrap();
        conn.execute(
            "CREATE TRIGGER block_second_member BEFORE INSERT ON drive_local_collection_members
             WHEN NEW.entry_id='entry-two' BEGIN SELECT RAISE(ABORT,'fixture second denied'); END;",
        )
        .unwrap();
        assert!(save(
            &conn,
            Some(&created.id),
            Some(1),
            "Renamed",
            &["entry-one".into(), "entry-two".into()]
        )
        .unwrap_err()
        .contains("fixture second denied"));
        let after = list(&conn).unwrap();
        assert_eq!(after[0].name, "Alpha");
        assert_eq!(after[0].revision, 1);
        assert_eq!(after[0].entry_ids, ["entry-one"]);
    }
}
