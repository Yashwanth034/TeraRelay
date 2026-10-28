//! Opt-in portable receipt for a recursively trashed folder.
//! Metadata only; SHA-256 is an integrity check, not an authenticity signature.
use crate::commands::drive_folder_tree_trash::{
    ensure_portable_tree_journal_schema, validate_portable_tree_journal,
};
use crate::commands::drive_metadata::{drive_snapshot, portable_name_key, validate_drive_name};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, State as SqlState, Value};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};
use tauri::State;
const FORMAT: &str = "terarelay-drive-tree-trash-recovery";
const LIMIT: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    root_id: String,
    name: String,
    parent_id: Option<String>,
    created_at: i64,
    folders: usize,
    files: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    kind: String,
    id: String,
    parent: Option<String>,
    name: String,
    deleted_at: i64,
    storage: i64,
    content_id: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: String,
    version: u32,
    exported_at: i64,
    header: Header,
    members: Vec<Member>,
    sha256: String,
}
#[derive(Debug, Serialize)]
pub struct ReceiptResult {
    pub root_id: String,
    pub path: String,
    pub folders: usize,
    pub files: usize,
    pub revision: i64,
    pub checksum: String,
    pub imported: bool,
}
fn sql_error(e: sqlite::Error) -> String {
    e.to_string()
}
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn nullable(q: &sqlite::Statement, n: usize) -> Result<Option<String>, String> {
    match q.read::<Value, _>(n).map_err(sql_error)? {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err("Invalid nullable receipt field".into()),
    }
}
fn digest(r: &Receipt) -> Result<String, String> {
    let payload = serde_json::to_vec(&(&r.format, r.version, r.exported_at, &r.header, &r.members))
        .map_err(|_| "Could not hash receipt")?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}
fn validate(r: &Receipt) -> Result<(), String> {
    let h = &r.header;
    if r.format != FORMAT
        || r.version != 1
        || r.exported_at <= 0
        || h.created_at <= 0
        || r.sha256.len() != 64
        || !r
            .sha256
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || !safe_id(&h.root_id)
        || h.parent_id.as_deref().is_some_and(|s| !safe_id(s))
        || validate_drive_name(&h.name).is_err()
        || h.folders == 0
        || h.folders > 512
        || h.files > 2000
        || h.folders + h.files != r.members.len()
    {
        return Err("Invalid recovery receipt schema, header or bounds".into());
    }
    let mut identities = HashSet::new();
    let mut names = HashSet::new();
    let (mut dirs, mut files, mut roots) = (0, 0, 0);
    for m in &r.members {
        if !safe_id(&m.id)
            || m.parent.as_deref().is_some_and(|s| !safe_id(s))
            || validate_drive_name(&m.name).is_err()
            || m.deleted_at <= 0
            || !identities.insert((m.kind.as_str(), m.id.as_str()))
            || !names.insert((m.parent.clone(), portable_name_key(&m.name)))
        {
            return Err("Recovery receipt has invalid, duplicate or colliding members".into());
        }
        match m.kind.as_str() {
            "folder" if m.storage == 0 && m.content_id.is_none() => {
                dirs += 1;
                if m.id == h.root_id {
                    roots += 1;
                    if m.name != h.name || m.parent != h.parent_id {
                        return Err("Recovery root identity changed".into());
                    }
                }
            }
            "file"
                if (m.storage == 1 || m.storage == 2)
                    && m.content_id.as_deref().is_some_and(safe_id) =>
            {
                files += 1
            }
            _ => return Err("Recovery member kind and content identity disagree".into()),
        }
    }
    if dirs != h.folders || files != h.files || roots != 1 {
        return Err("Recovery membership counts disagree with header".into());
    }
    Ok(())
}
fn encode(mut r: Receipt) -> Result<Vec<u8>, String> {
    r.sha256 = "0".repeat(64);
    validate(&r)?;
    r.sha256 = digest(&r)?;
    let raw = serde_json::to_vec_pretty(&r).map_err(|_| "Cannot serialize receipt")?;
    if raw.len() > LIMIT {
        return Err("Receipt is larger than 4 MiB".into());
    }
    Ok(raw)
}
fn decode(raw: &[u8]) -> Result<Receipt, String> {
    if raw.len() > LIMIT {
        return Err("Receipt is larger than 4 MiB".into());
    }
    let r: Receipt =
        serde_json::from_slice(raw).map_err(|_| "Invalid or truncated recovery receipt")?;
    validate(&r)?;
    if digest(&r)? != r.sha256 {
        return Err("Receipt SHA-256 integrity check failed".into());
    }
    Ok(r)
}
fn select(conn: &Connection, root: &str) -> Result<(Receipt, i64), String> {
    ensure_portable_tree_journal_schema(conn)?;
    if !safe_id(root) {
        return Err("Invalid selected folder".into());
    }
    validate_portable_tree_journal(conn, root)?;
    let rev = drive_snapshot(conn)?.revision;
    let mut q = conn
        .prepare(
            "SELECT original_name,original_parent_id,created_at,folder_count,file_count
   FROM drive_tree_trash_runs WHERE root_id=?",
        )
        .map_err(sql_error)?;
    q.bind((1, root)).map_err(sql_error)?;
    if q.next().map_err(sql_error)? != SqlState::Row {
        return Err("Recovery journal not found".into());
    }
    let header = Header {
        root_id: root.to_owned(),
        name: q.read::<String, _>(0).map_err(sql_error)?,
        parent_id: nullable(&q, 1)?,
        created_at: q.read::<i64, _>(2).map_err(sql_error)?,
        folders: usize::try_from(q.read::<i64, _>(3).map_err(sql_error)?)
            .map_err(|_| "Invalid saved folder count")?,
        files: usize::try_from(q.read::<i64, _>(4).map_err(sql_error)?)
            .map_err(|_| "Invalid saved file count")?,
    };
    drop(q);
    let mut q = conn
        .prepare(
            "SELECT kind,entry_id,parent_id,display_name,deleted_at,storage_version,content_id
   FROM drive_tree_trash_members WHERE root_id=? ORDER BY kind,entry_id",
        )
        .map_err(sql_error)?;
    q.bind((1, root)).map_err(sql_error)?;
    let mut members = Vec::new();
    while q.next().map_err(sql_error)? == SqlState::Row {
        members.push(Member {
            kind: q.read::<String, _>(0).map_err(sql_error)?,
            id: q.read::<String, _>(1).map_err(sql_error)?,
            parent: nullable(&q, 2)?,
            name: q.read::<String, _>(3).map_err(sql_error)?,
            deleted_at: q.read::<i64, _>(4).map_err(sql_error)?,
            storage: q.read::<i64, _>(5).map_err(sql_error)?,
            content_id: nullable(&q, 6)?,
        });
        if members.len() > 2512 {
            return Err("Journal membership limit exceeded".into());
        }
    }
    let r = Receipt {
        format: FORMAT.into(),
        version: 1,
        exported_at: chrono::Utc::now().timestamp_millis(),
        header,
        members,
        sha256: "0".repeat(64),
    };
    validate(&r)?;
    Ok((r, rev))
}
fn checked_path(s: &str) -> Result<&Path, String> {
    if s.len() > 4096 || !s.ends_with(".json") || s.chars().any(char::is_control) {
        return Err("Choose an absolute JSON receipt path".into());
    }
    let p = Path::new(s);
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Receipt path cannot use relative traversal".into());
    }
    Ok(p)
}
fn safe_parent(path: &Path) -> Result<(), String> {
    let mut prefix = std::path::PathBuf::new();
    for part in path
        .parent()
        .ok_or("Receipt has no parent directory")?
        .components()
    {
        prefix.push(part.as_os_str());
        if matches!(part, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let md =
            fs::symlink_metadata(&prefix).map_err(|_| "Receipt parent directory unavailable")?;
        if !md.is_dir() || md.file_type().is_symlink() {
            return Err("Receipt path cannot traverse symbolic links".into());
        }
    }
    Ok(())
}
fn write_new(path: &str, bytes: &[u8]) -> Result<(), String> {
    let dst = checked_path(path)?;
    safe_parent(dst)?;
    let stage = dst.with_file_name(format!(
        ".terarelay-tree-receipt-{}.tmp",
        rand::random::<u128>()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&stage).map_err(|_| "Cannot stage receipt")?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = result {
        let _ = fs::remove_file(&stage);
        return Err(format!("Staging recovery receipt failed: {e}"));
    }
    let result = fs::hard_link(&stage, dst);
    let _ = fs::remove_file(&stage);
    result.map_err(|_| "Receipt exists or no-clobber publication failed")?;
    #[cfg(unix)]
    {
        let dir = fs::File::open(dst.parent().ok_or("Missing receipt directory")?)
            .map_err(|_| "Receipt published; durability unconfirmed")?;
        dir.sync_all()
            .map_err(|_| "Receipt published; durability unconfirmed")?;
    }
    Ok(())
}
fn read_checked(path: &str) -> Result<Vec<u8>, String> {
    let p = checked_path(path)?;
    safe_parent(p)?;
    let md = fs::symlink_metadata(p).map_err(|_| "Receipt path unavailable")?;
    if !md.is_file() || md.len() > LIMIT as u64 {
        return Err("Receipt must be an ordinary JSON file <= 4 MiB".into());
    }
    let f = fs::File::open(p).map_err(|_| "Cannot open receipt")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = f.metadata().map_err(|_| "Cannot stat receipt")?;
        if opened.dev() != md.dev() || opened.ino() != md.ino() {
            return Err("Receipt changed while opening".into());
        }
    }
    let mut raw = Vec::new();
    f.take(LIMIT as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "Cannot read receipt")?;
    if raw.len() > LIMIT {
        return Err("Receipt too large".into());
    }
    Ok(raw)
}
fn import_receipt(conn: &Connection, r: &Receipt) -> Result<i64, String> {
    ensure_portable_tree_journal_schema(conn)?;
    validate(r)?;
    let before = drive_snapshot(conn)?;
    conn.execute("SAVEPOINT drive_portable_tree_receipt")
        .map_err(sql_error)?;
    let result = (|| {
        let h = &r.header;
        let mut q = conn
            .prepare(
                "INSERT INTO drive_tree_trash_runs
    (root_id,original_name,original_parent_id,created_at,folder_count,file_count)
    VALUES (?,?,?,?,?,?)",
            )
            .map_err(sql_error)?;
        q.bind((1, h.root_id.as_str())).map_err(sql_error)?;
        q.bind((2, h.name.as_str())).map_err(sql_error)?;
        q.bind((3, h.parent_id.as_deref())).map_err(sql_error)?;
        q.bind((4, h.created_at)).map_err(sql_error)?;
        q.bind((5, i64::try_from(h.folders).map_err(|_| "Invalid count")?))
            .map_err(sql_error)?;
        q.bind((6, i64::try_from(h.files).map_err(|_| "Invalid count")?))
            .map_err(sql_error)?;
        q.next().map_err(sql_error)?;
        drop(q);
        for m in &r.members {
            let mut q = conn
                .prepare(
                    "INSERT INTO drive_tree_trash_members
     (root_id,kind,entry_id,parent_id,display_name,deleted_at,storage_version,content_id)
     VALUES (?,?,?,?,?,?,?,?)",
                )
                .map_err(sql_error)?;
            q.bind((1, h.root_id.as_str())).map_err(sql_error)?;
            q.bind((2, m.kind.as_str())).map_err(sql_error)?;
            q.bind((3, m.id.as_str())).map_err(sql_error)?;
            q.bind((4, m.parent.as_deref())).map_err(sql_error)?;
            q.bind((5, m.name.as_str())).map_err(sql_error)?;
            q.bind((6, m.deleted_at)).map_err(sql_error)?;
            q.bind((7, m.storage)).map_err(sql_error)?;
            q.bind((8, m.content_id.as_deref())).map_err(sql_error)?;
            q.next().map_err(sql_error)?;
        }
        let (folders, files) = validate_portable_tree_journal(conn, &h.root_id)?;
        if folders != h.folders || files != h.files {
            return Err("Imported recovery receipt has incomplete membership".into());
        }
        let after = drive_snapshot(conn)?;
        if after.revision != before.revision || after.dirty != before.dirty {
            return Err("Import unexpectedly modified Drive content metadata".into());
        }
        Ok(after.revision)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_portable_tree_receipt", result)
}
#[tauri::command]
pub fn cmd_drive_tree_receipt_export(
    db: State<'_, DbConnection>,
    root_id: String,
    destination_path: String,
) -> Result<ReceiptResult, String> {
    let (receipt, revision) = {
        let conn = db.lock().map_err(|_| "Drive DB unavailable")?;
        select(&conn, &root_id)?
    };
    let bytes = encode(receipt)?;
    let verified = decode(&bytes)?;
    write_new(&destination_path, &bytes)?;
    Ok(ReceiptResult {
        root_id: verified.header.root_id,
        path: destination_path,
        folders: verified.header.folders,
        files: verified.header.files,
        revision,
        checksum: verified.sha256,
        imported: false,
    })
}
#[tauri::command]
pub fn cmd_drive_tree_receipt_import(
    db: State<'_, DbConnection>,
    source_path: String,
) -> Result<ReceiptResult, String> {
    let receipt = decode(&read_checked(&source_path)?)?;
    let revision = {
        let conn = db.lock().map_err(|_| "Drive DB unavailable")?;
        import_receipt(&conn, &receipt)?
    };
    Ok(ReceiptResult {
        root_id: receipt.header.root_id,
        path: source_path,
        folders: receipt.header.folders,
        files: receipt.header.files,
        revision,
        checksum: receipt.sha256,
        imported: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;
    fn fixture(with_journal: bool) -> Connection {
        let c = sqlite::open(":memory:").unwrap();
        c.execute(
            "CREATE TABLE logical_channels (
   logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER UNIQUE,
   name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
   CREATE TABLE logical_files (
   file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,
   first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,
   mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
   storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
   status TEXT NOT NULL DEFAULT 'complete');",
        )
        .unwrap();
        init_drive_schema(&c).unwrap();
        c.execute(
            "INSERT INTO drive_directories
     (id,parent_id,name,created_at,updated_at,trashed_at)
     VALUES ('root',NULL,'Projects',1,1,777),
            ('child','root','Drafts',1,1,777),
            ('sibling',NULL,'Outside',1,1,NULL);
    INSERT INTO logical_channels
     (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
     VALUES('qa',11,'qa','owner',1,1,1);
    INSERT INTO logical_files
     (file_id,logical_channel_id,first_message_id,display_name,total_size,
     mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
     VALUES('blob','qa',5,'notes.txt',14,'text/plain',1,'hash',1,1,'complete');
    INSERT INTO drive_file_entries
     (entry_id,file_id,directory_id,display_name,created_at,updated_at,trashed_at)
     VALUES('file','blob','child','notes.txt',1,1,777);",
        )
        .unwrap();
        if with_journal {
            ensure_portable_tree_journal_schema(&c).unwrap();
            c.execute(
                "INSERT INTO drive_tree_trash_runs
      (root_id,original_name,original_parent_id,created_at,folder_count,file_count)
      VALUES('root','Projects',NULL,1000,2,1);
     INSERT INTO drive_tree_trash_members
      (root_id,kind,entry_id,parent_id,display_name,deleted_at,storage_version,content_id)
      VALUES('root','folder','root',NULL,'Projects',777,0,NULL),
       ('root','folder','child','root','Drafts',777,0,NULL),
       ('root','file','file','child','notes.txt',777,1,'blob');",
            )
            .unwrap();
        }
        c
    }
    #[test]
    fn portable_receipt_roundtrip_imports_full_journal_on_other_matching_database() {
        let source = fixture(true);
        let (receipt, _) = select(&source, "root").unwrap();
        let raw = encode(receipt).unwrap();
        let verified = decode(&raw).unwrap();
        let dest = fixture(false);
        let original_revision = drive_snapshot(&dest).unwrap().revision;
        assert_eq!(import_receipt(&dest, &verified).unwrap(), original_revision);
        assert_eq!(
            validate_portable_tree_journal(&dest, "root").unwrap(),
            (2, 1)
        );
        assert_eq!(drive_snapshot(&dest).unwrap().revision, original_revision);
        assert!(import_receipt(&dest, &verified).is_err());
        assert_eq!(
            validate_portable_tree_journal(&dest, "root").unwrap(),
            (2, 1)
        );
    }
    #[test]
    fn checksum_tampering_truncation_schema_and_member_duplication_fail() {
        let (r, _) = select(&fixture(true), "root").unwrap();
        let raw = encode(r).unwrap();
        assert!(decode(&raw[..raw.len() - 2]).is_err());
        assert!(decode(&vec![b' '; LIMIT + 1]).is_err());
        let tampered = String::from_utf8(raw.clone())
            .unwrap()
            .replace("notes.txt", "evilx.txt");
        assert!(decode(tampered.as_bytes()).is_err());
        let mut same = decode(&raw).unwrap();
        same.members.push(same.members[0].clone());
        assert!(encode(same).is_err());
        let schema = String::from_utf8(raw)
            .unwrap()
            .replace("\"version\": 1", "\"version\": 2");
        assert!(decode(schema.as_bytes()).is_err());
    }
    #[test]
    fn changed_content_identity_and_tombstone_refuse_import_without_partial_journal() {
        let (r, _) = select(&fixture(true), "root").unwrap();
        let receipt = decode(&encode(r).unwrap()).unwrap();
        let dest = fixture(false);
        dest.execute(
            "INSERT INTO logical_files
   (file_id,logical_channel_id,first_message_id,display_name,total_size,
    mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
   VALUES('other','qa',6,'notes.txt',14,'text/plain',1,'new',1,1,'complete');
   UPDATE drive_file_entries SET file_id='other' WHERE entry_id='file'",
        )
        .unwrap();
        assert!(import_receipt(&dest, &receipt).is_err());
        assert!(validate_portable_tree_journal(&dest, "root").is_err());
        dest.execute(
            "UPDATE drive_file_entries SET file_id='blob';
   UPDATE drive_directories SET trashed_at=778 WHERE id='child'",
        )
        .unwrap();
        assert!(import_receipt(&dest, &receipt).is_err());
        assert!(validate_portable_tree_journal(&dest, "root").is_err());
    }
    #[test]
    fn ignored_member_insert_rolls_back_header_and_all_member_inserts() {
        let (receipt, _) = select(&fixture(true), "root").unwrap();
        let dest = fixture(false);
        ensure_portable_tree_journal_schema(&dest).unwrap();
        dest.execute(
            "CREATE TRIGGER ignore_one BEFORE INSERT ON drive_tree_trash_members
     WHEN NEW.entry_id='file' BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        assert!(import_receipt(&dest, &receipt).is_err());
        assert!(validate_portable_tree_journal(&dest, "root").is_err());
        let mut q = dest
            .prepare("SELECT COUNT(*) FROM drive_tree_trash_runs")
            .unwrap();
        assert_eq!(q.next().unwrap(), SqlState::Row);
        assert_eq!(q.read::<i64, _>(0).unwrap(), 0);
    }
    #[test]
    fn no_clobber_export_and_traversal_restrictions() {
        assert!(checked_path("relative.json").is_err());
        assert!(checked_path("/fake/../receipt.json").is_err());
        assert!(checked_path("/fake/other.txt").is_err());
        let directory = crate::test_temp_dir()
            .join(format!("terarelay-receipt-test-{}", rand::random::<u128>()));
        fs::create_dir(&directory).unwrap();
        let dst = directory.join("recovery.json");
        let name = dst.to_string_lossy().to_string();
        let raw = encode(select(&fixture(true), "root").unwrap().0).unwrap();
        write_new(&name, &raw).unwrap();
        assert!(write_new(&name, b"overwrite").is_err());
        assert_eq!(read_checked(&name).unwrap(), raw);
        fs::remove_file(dst).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
