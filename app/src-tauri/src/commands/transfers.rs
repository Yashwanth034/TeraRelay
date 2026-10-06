use crate::commands::logical_files::ManifestChunkV1;
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const SOURCE_CHANGED: &str =
    "Upload source changed since this transfer was saved. Select the file as a new upload.";

fn queue_kind(kind: &str) -> Result<(), String> {
    if matches!(kind, "upload" | "download") {
        Ok(())
    } else {
        Err("Invalid transfer queue kind".to_string())
    }
}

fn queue_table(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS transfer_queues (kind TEXT PRIMARY KEY, items TEXT NOT NULL)",
    )
    .map_err(|e| format!("Cannot open transfer queue: {e}"))
}

fn load_queue(conn: &sqlite::Connection, kind: &str) -> Result<Option<Value>, String> {
    queue_kind(kind)?;
    queue_table(conn)?;
    let mut statement = conn
        .prepare("SELECT items FROM transfer_queues WHERE kind = ?")
        .map_err(|e| e.to_string())?;
    statement.bind((1, kind)).map_err(|e| e.to_string())?;
    if statement.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let text = statement.read::<String, _>(0).map_err(|e| e.to_string())?;
    let items: Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid saved transfer queue: {e}"))?;
    if !items.is_array() {
        return Err("Invalid saved transfer queue: expected an array".to_string());
    }
    Ok(Some(items))
}

fn save_queue(conn: &sqlite::Connection, kind: &str, items: &Value) -> Result<(), String> {
    queue_kind(kind)?;
    if !items.is_array() {
        return Err("Transfer queue must be an array".to_string());
    }
    queue_table(conn)?;
    let text = serde_json::to_string(items).map_err(|e| e.to_string())?;
    let mut statement = conn.prepare(
        "INSERT INTO transfer_queues(kind, items) VALUES (?, ?) ON CONFLICT(kind) DO UPDATE SET items = excluded.items"
    ).map_err(|e| e.to_string())?;
    statement.bind((1, kind)).map_err(|e| e.to_string())?;
    statement
        .bind((2, text.as_str()))
        .map_err(|e| e.to_string())?;
    statement
        .next()
        .map_err(|e| format!("Cannot save transfer queue: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn cmd_load_transfer_queue(
    kind: String,
    db_pool: tauri::State<'_, DbConnection>,
) -> Result<Option<Value>, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    load_queue(&conn, &kind)
}

#[tauri::command]
pub fn cmd_save_transfer_queue(
    kind: String,
    items: Value,
    db_pool: tauri::State<'_, DbConnection>,
) -> Result<(), String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    save_queue(&conn, &kind, &items)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceMetadata {
    pub size: u64,
    modified: Option<(u64, u32)>,
    created: Option<(u64, u32)>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl SourceMetadata {
    fn from_metadata(meta: &std::fs::Metadata) -> Result<Self, String> {
        if !meta.is_file() {
            return Err("Upload source must be a regular file".to_string());
        }
        let stamp = |time: std::io::Result<std::time::SystemTime>| {
            time.ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()))
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: meta.len(),
            modified: stamp(meta.modified()),
            created: stamp(meta.created()),
            #[cfg(unix)]
            device: meta.dev(),
            #[cfg(unix)]
            inode: meta.ino(),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceIdentity {
    canonical_path: String,
    pub metadata: SourceMetadata,
    pub size: u64,
    sample_sha256: String,
}

pub async fn source_identity(path: &str) -> Result<SourceIdentity, String> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("Cannot read upload source: {e}"))?;
    let before = SourceMetadata::from_metadata(
        &tokio::fs::metadata(&canonical)
            .await
            .map_err(|e| e.to_string())?,
    )?;
    let mut file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|e| e.to_string())?;
    let mut buffer = vec![0; before.size.min(16 * 1024) as usize];
    let mut hasher = Sha256::new();
    file.read_exact(&mut buffer)
        .await
        .map_err(|e| format!("Cannot verify upload source: {e}"))?;
    hasher.update(&buffer);
    if before.size > buffer.len() as u64 {
        file.seek(std::io::SeekFrom::Start(before.size - buffer.len() as u64))
            .await
            .map_err(|e| e.to_string())?;
        file.read_exact(&mut buffer)
            .await
            .map_err(|e| format!("Cannot verify upload source: {e}"))?;
        hasher.update(&buffer);
    }
    let after = SourceMetadata::from_metadata(
        &tokio::fs::metadata(&canonical)
            .await
            .map_err(|e| e.to_string())?,
    )?;
    if before != after {
        return Err(SOURCE_CHANGED.to_string());
    }
    Ok(SourceIdentity {
        canonical_path: canonical.to_string_lossy().into_owned(),
        size: before.size,
        metadata: before,
        sample_sha256: format!("{:x}", hasher.finalize()),
    })
}

pub async fn verify_source(path: &str, expected: &SourceIdentity) -> Result<(), String> {
    if &source_identity(path).await? == expected {
        Ok(())
    } else {
        Err(SOURCE_CHANGED.to_string())
    }
}

/// Bounded-memory verification for legacy parts without a local checkpoint.
pub async fn hash_file_range(path: &str, offset: u64, len: u64) -> Result<String, String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| e.to_string())?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut remaining = len;
    let mut hasher = Sha256::new();
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..take])
            .await
            .map_err(|e| format!("Cannot verify saved upload part: {e}"))?;
        hasher.update(&buffer[..take]);
        remaining -= take as u64;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadCheckpoint {
    schema_version: u32,
    pub source: SourceIdentity,
    pub part_size: u64,
    pub logical_file_id: Option<String>,
    #[serde(skip)]
    pub chunks: BTreeMap<u32, ManifestChunkV1>,
    pub manifest_message_id: Option<i64>,
    pub completed: bool,
}

pub struct UploadJournal {
    db: DbConnection,
    key: String,
    pub snapshot: UploadCheckpoint,
    pub resumed: bool,
}

fn journal_tables(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS upload_checkpoints (id TEXT PRIMARY KEY, state TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS upload_checkpoint_chunks (checkpoint_id TEXT NOT NULL, part_index INTEGER NOT NULL, chunk TEXT NOT NULL, PRIMARY KEY(checkpoint_id, part_index));"
    ).map_err(|e| format!("Cannot open upload checkpoints: {e}"))
}

fn save_checkpoint(
    conn: &sqlite::Connection,
    key: &str,
    state: &UploadCheckpoint,
) -> Result<(), String> {
    let text = serde_json::to_string(state).map_err(|e| e.to_string())?;
    let mut statement = conn.prepare("INSERT INTO upload_checkpoints(id,state) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET state=excluded.state").map_err(|e| e.to_string())?;
    statement.bind((1, key)).map_err(|e| e.to_string())?;
    statement
        .bind((2, text.as_str()))
        .map_err(|e| e.to_string())?;
    statement
        .next()
        .map_err(|e| format!("Cannot save upload checkpoint: {e}"))?;
    Ok(())
}

fn save_chunk(conn: &sqlite::Connection, key: &str, chunk: &ManifestChunkV1) -> Result<(), String> {
    let text = serde_json::to_string(chunk).map_err(|e| e.to_string())?;
    let mut statement = conn.prepare("INSERT INTO upload_checkpoint_chunks(checkpoint_id,part_index,chunk) VALUES (?,?,?) ON CONFLICT(checkpoint_id,part_index) DO UPDATE SET chunk=excluded.chunk").map_err(|e| e.to_string())?;
    statement.bind((1, key)).map_err(|e| e.to_string())?;
    statement
        .bind((2, i64::from(chunk.index)))
        .map_err(|e| e.to_string())?;
    statement
        .bind((3, text.as_str()))
        .map_err(|e| e.to_string())?;
    statement
        .next()
        .map_err(|e| format!("Cannot save confirmed upload part: {e}"))?;
    Ok(())
}

impl UploadJournal {
    pub async fn open(
        db: DbConnection,
        path: &str,
        transfer_id: &str,
        folder_id: Option<i64>,
        part_size: u64,
        file_id: Option<String>,
    ) -> Result<Self, String> {
        let source = source_identity(path).await?;
        let identity =
            serde_json::to_vec(&(transfer_id, path, folder_id)).map_err(|e| e.to_string())?;
        let key = format!("{:x}", Sha256::digest(&identity));
        let mut resumed = false;
        let mut snapshot = UploadCheckpoint {
            schema_version: 1,
            source: source.clone(),
            part_size,
            logical_file_id: file_id,
            chunks: BTreeMap::new(),
            manifest_message_id: None,
            completed: false,
        };
        {
            let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
            journal_tables(&conn)?;
            let mut statement = conn
                .prepare("SELECT state FROM upload_checkpoints WHERE id = ?")
                .map_err(|e| e.to_string())?;
            statement
                .bind((1, key.as_str()))
                .map_err(|e| e.to_string())?;
            if statement.next().map_err(|e| e.to_string())? == sqlite::State::Row {
                resumed = true;
                let text = statement.read::<String, _>(0).map_err(|e| e.to_string())?;
                snapshot = serde_json::from_str(&text)
                    .map_err(|e| format!("Invalid saved upload checkpoint: {e}"))?;
                if snapshot.schema_version != 1 {
                    return Err("Unsupported saved upload checkpoint".to_string());
                }
                if snapshot.source != source {
                    return Err(SOURCE_CHANGED.to_string());
                }
                if snapshot.part_size != part_size {
                    return Err("Upload part layout changed. Restore the previous part-size setting or select the file as a new upload.".to_string());
                }
                let mut parts = conn.prepare("SELECT chunk FROM upload_checkpoint_chunks WHERE checkpoint_id = ? ORDER BY part_index").map_err(|e| e.to_string())?;
                parts.bind((1, key.as_str())).map_err(|e| e.to_string())?;
                while parts.next().map_err(|e| e.to_string())? == sqlite::State::Row {
                    let text = parts.read::<String, _>(0).map_err(|e| e.to_string())?;
                    let chunk: ManifestChunkV1 = serde_json::from_str(&text)
                        .map_err(|e| format!("Invalid saved upload part: {e}"))?;
                    validate_chunk(&snapshot, &chunk)?;
                    snapshot.chunks.insert(chunk.index, chunk);
                }
            } else {
                save_checkpoint(&conn, &key, &snapshot)?;
            }
        }
        Ok(Self {
            db,
            key,
            snapshot,
            resumed,
        })
    }

    pub async fn verify_source(&self, path: &str) -> Result<(), String> {
        verify_source(path, &self.snapshot.source).await
    }

    pub fn record(&mut self, chunk: ManifestChunkV1) -> Result<(), String> {
        validate_chunk(&self.snapshot, &chunk)?;
        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
        save_chunk(&conn, &self.key, &chunk)?;
        self.snapshot.chunks.insert(chunk.index, chunk);
        Ok(())
    }

    pub fn replace_chunks(&mut self, chunks: BTreeMap<u32, ManifestChunkV1>) -> Result<(), String> {
        for chunk in chunks.values() {
            validate_chunk(&self.snapshot, chunk)?;
        }
        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
        conn.execute("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
        let result = (|| {
            let mut statement = conn
                .prepare("DELETE FROM upload_checkpoint_chunks WHERE checkpoint_id = ?")
                .map_err(|e| e.to_string())?;
            statement
                .bind((1, self.key.as_str()))
                .map_err(|e| e.to_string())?;
            statement.next().map_err(|e| e.to_string())?;
            for chunk in chunks.values() {
                save_chunk(&conn, &self.key, chunk)?;
            }
            conn.execute("COMMIT").map_err(|e| e.to_string())
        })();
        if let Err(error) = result {
            let _ = conn.execute("ROLLBACK");
            return Err(error);
        }
        self.snapshot.chunks = chunks;
        Ok(())
    }

    pub fn complete(&mut self, manifest_message_id: Option<i64>) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        next.completed = true;
        next.manifest_message_id = manifest_message_id;
        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
        save_checkpoint(&conn, &self.key, &next)?;
        self.snapshot = next;
        Ok(())
    }
}

fn validate_chunk(snapshot: &UploadCheckpoint, chunk: &ManifestChunkV1) -> Result<(), String> {
    let offset = u64::from(chunk.index)
        .checked_sub(1)
        .and_then(|i| i.checked_mul(snapshot.part_size));
    let expected = offset
        .filter(|offset| *offset < snapshot.source.size)
        .map(|offset| snapshot.part_size.min(snapshot.source.size - offset));
    if expected != Some(chunk.size)
        || chunk.message_id <= 0
        || chunk.message_id > i64::from(i32::MAX)
        || chunk.sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64 || !hash.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
        })
    {
        return Err("Invalid confirmed upload part".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn scratch() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "terarelay-journal-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn transfer_queue_survives_reopen_without_changing_other_tables() {
        let dir = scratch();
        let db_path = dir.join("test.db");
        {
            let conn = sqlite::open(&db_path).unwrap();
            conn.execute(
                "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('keep');",
            )
            .unwrap();
            assert_eq!(load_queue(&conn, "upload").unwrap(), None);
            save_queue(
                &conn,
                "upload",
                &json!([{"id":"saved","status":"pending","path":"/test/file"}]),
            )
            .unwrap();
            save_queue(&conn, "download", &json!([])).unwrap();
        }
        {
            let conn = sqlite::open(&db_path).unwrap();
            assert_eq!(
                load_queue(&conn, "upload").unwrap().unwrap()[0]["id"],
                "saved"
            );
            assert_eq!(load_queue(&conn, "download").unwrap(), Some(json!([])));
            assert!(save_queue(&conn, "auth", &json!([])).is_err());
            assert!(save_queue(&conn, "upload", &json!({})).is_err());
            let mut statement = conn.prepare("SELECT value FROM unrelated").unwrap();
            assert_eq!(statement.next().unwrap(), sqlite::State::Row);
            assert_eq!(statement.read::<String, _>(0).unwrap(), "keep");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_transfer_queue_is_reported_and_preserved() {
        let conn = sqlite::open(":memory:").unwrap();
        save_queue(&conn, "upload", &json!([])).unwrap();
        conn.execute("UPDATE transfer_queues SET items = 'broken' WHERE kind = 'upload'")
            .unwrap();
        assert!(load_queue(&conn, "upload")
            .unwrap_err()
            .contains("Invalid saved transfer queue"));
        let mut statement = conn
            .prepare("SELECT items FROM transfer_queues WHERE kind = 'upload'")
            .unwrap();
        assert_eq!(statement.next().unwrap(), sqlite::State::Row);
        assert_eq!(statement.read::<String, _>(0).unwrap(), "broken");
    }

    #[tokio::test]
    async fn upload_checkpoint_survives_reopen_and_rejects_same_size_replacement() {
        let dir = scratch();
        let path = dir.join("movie.bin");
        let db_path = dir.join("test.db");
        std::fs::write(&path, b"original file bytes").unwrap();
        let path = path.to_str().unwrap();
        {
            let db = Arc::new(Mutex::new(sqlite::open(&db_path).unwrap()));
            let mut journal =
                UploadJournal::open(db, path, "transfer-1", Some(42), 10, Some("a".repeat(32)))
                    .await
                    .unwrap();
            journal
                .record(ManifestChunkV1 {
                    index: 1,
                    message_id: 7,
                    size: 10,
                    sha256: Some("b".repeat(64)),
                })
                .unwrap();
        }
        {
            let db = Arc::new(Mutex::new(sqlite::open(&db_path).unwrap()));
            let journal = UploadJournal::open(
                db.clone(),
                path,
                "transfer-1",
                Some(42),
                10,
                Some("c".repeat(32)),
            )
            .await
            .unwrap();
            assert_eq!(journal.snapshot.logical_file_id, Some("a".repeat(32)));
            assert_eq!(journal.snapshot.chunks[&1].message_id, 7);
            std::fs::write(path, b"replacement bytes!!").unwrap();
            assert_eq!(
                std::fs::metadata(path).unwrap().len(),
                journal.snapshot.source.size
            );
            assert!(journal
                .verify_source(path)
                .await
                .unwrap_err()
                .contains("Upload source changed"));
            assert!(
                UploadJournal::open(db.clone(), path, "transfer-1", Some(42), 10, None)
                    .await
                    .err()
                    .unwrap()
                    .contains("Upload source changed")
            );
            assert!(
                UploadJournal::open(db, path, "transfer-2", Some(42), 10, None)
                    .await
                    .is_ok()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
