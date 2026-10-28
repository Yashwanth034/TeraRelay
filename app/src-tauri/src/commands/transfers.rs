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

pub async fn hash_file_with_identity(path: &str) -> Result<(String, String, u64), String> {
    let before = source_identity(path).await?;
    let hash = hash_file_range(path, 0, before.size).await?;
    let after = source_identity(path).await?;
    if before != after {
        return Err(SOURCE_CHANGED.to_string());
    }
    let identity = serde_json::to_string(&before)
        .map_err(|e| format!("Cannot serialize upload source identity: {e}"))?;
    Ok((hash, identity, before.size))
}

pub async fn verify_serialized_source_identity(path: &str, serialized: &str) -> Result<(), String> {
    let expected: SourceIdentity = serde_json::from_str(serialized)
        .map_err(|e| format!("Invalid upload source identity: {e}"))?;
    verify_source(path, &expected).await
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
    /// Bound to the transfer ID on first upload. Older checkpoints omit this;
    /// their verified chunks can still resume with the name chosen at reopen.
    #[serde(default)]
    pub display_name: Option<String>,
    /// First upload's manifest creation timestamp, retained across retries.
    #[serde(default)]
    pub created_at: Option<i64>,
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
        display_name: &str,
    ) -> Result<Self, String> {
        if display_name.is_empty() {
            return Err("Upload display name cannot be empty".into());
        }
        let source = source_identity(path).await?;
        // Without a stable transfer ID there is no safe way to tell a new
        // upload from a retry. Never reuse a completed file's logical ID just
        // because its local path and destination happen to be unchanged.
        let key = if transfer_id.trim().is_empty() {
            format!("upload-once-{:032x}", rand::random::<u128>())
        } else {
            let identity =
                serde_json::to_vec(&(transfer_id, path, folder_id)).map_err(|e| e.to_string())?;
            format!("{:x}", Sha256::digest(&identity))
        };
        let mut resumed = false;
        let mut snapshot = UploadCheckpoint {
            schema_version: 1,
            source: source.clone(),
            part_size,
            logical_file_id: file_id,
            display_name: Some(display_name.to_owned()),
            created_at: Some(chrono::Utc::now().timestamp()),
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
                if snapshot.created_at.is_some_and(|created| created <= 0) {
                    return Err("Saved upload has an invalid creation timestamp".to_string());
                }
                if snapshot
                    .display_name
                    .as_deref()
                    .is_some_and(|saved| saved != display_name)
                {
                    return Err("Upload filename changed since this transfer was saved. Restore its original filename or add it as a new upload.".to_string());
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
                drop(parts);
                drop(statement);
                // Upgrade older checkpoints before more network activity.
                // Legacy records omitted the chosen name and first upload time.
                if snapshot.display_name.is_none() || snapshot.created_at.is_none() {
                    snapshot.display_name = Some(display_name.to_owned());
                    snapshot
                        .created_at
                        .get_or_insert_with(|| chrono::Utc::now().timestamp());
                    save_checkpoint(&conn, &key, &snapshot)?;
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

    /// Remember the accepted Telegram manifest *before* local Drive indexing.
    ///
    /// Publishing the kernel-visible file can fail after Telegram accepted its
    /// manifest. Without this durable checkpoint the next retry has to scan
    /// Telegram to rediscover the manifest, and an isolated QA cleanup may
    /// miss the last remote document. This does not mark the upload complete.
    pub fn record_manifest_message_id(&mut self, manifest_message_id: i64) -> Result<(), String> {
        if manifest_message_id <= 0 || manifest_message_id > i64::from(i32::MAX) {
            return Err("Invalid accepted Telegram manifest message ID".into());
        }
        let mut next = self.snapshot.clone();
        next.manifest_message_id = Some(manifest_message_id);
        let conn = self.db.lock().map_err(|_| "DB poisoned".to_string())?;
        save_checkpoint(&conn, &self.key, &next)?;
        self.snapshot = next;
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
    async fn full_file_fingerprint_hashes_every_byte_and_binds_to_source_identity() {
        let dir = scratch();
        let path = dir.join("fingerprint.bin");
        std::fs::write(&path, b"abc").unwrap();
        let path_str = path.to_str().unwrap();

        let (hash, identity, size) = hash_file_with_identity(path_str).await.unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        verify_serialized_source_identity(path_str, &identity)
            .await
            .unwrap();

        std::fs::write(&path, b"abd").unwrap();
        assert!(verify_serialized_source_identity(path_str, &identity)
            .await
            .unwrap_err()
            .contains("Upload source changed"));

        std::fs::remove_dir_all(dir).unwrap();
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
            let mut journal = UploadJournal::open(
                db,
                path,
                "transfer-1",
                Some(42),
                10,
                Some("a".repeat(32)),
                "movie.bin",
            )
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
                "movie.bin",
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
            assert!(UploadJournal::open(
                db.clone(),
                path,
                "transfer-1",
                Some(42),
                10,
                None,
                "movie.bin"
            )
            .await
            .err()
            .unwrap()
            .contains("Upload source changed"));
            assert!(
                UploadJournal::open(db, path, "transfer-2", Some(42), 10, None, "movie.bin")
                    .await
                    .is_ok()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn accepted_manifest_checkpoint_survives_local_publish_failure_and_retry() {
        let directory = scratch();
        let path = directory.join("accepted-manifest.bin");
        std::fs::write(&path, b"one small private upload").unwrap();
        let path = path.to_str().unwrap();
        let db_file = directory.join("upload.db");
        {
            let db = Arc::new(Mutex::new(sqlite::open(&db_file).unwrap()));
            let mut journal = UploadJournal::open(
                db,
                path,
                "accepted-manifest",
                Some(42),
                1024,
                Some("a".repeat(32)),
                "accepted-manifest.bin",
            )
            .await
            .unwrap();
            journal
                .record(ManifestChunkV1 {
                    index: 1,
                    message_id: 1801,
                    size: 24,
                    sha256: None,
                })
                .unwrap();
            assert!(journal.record_manifest_message_id(0).is_err());
            assert!(journal
                .record_manifest_message_id(i64::from(i32::MAX) + 1)
                .is_err());
            assert_eq!(journal.snapshot.manifest_message_id, None);
            journal.record_manifest_message_id(1802).unwrap();
            assert!(
                !journal.snapshot.completed,
                "Remote acceptance is not local completion"
            );
        }
        {
            let db = Arc::new(Mutex::new(sqlite::open(&db_file).unwrap()));
            let resumed = UploadJournal::open(
                db,
                path,
                "accepted-manifest",
                Some(42),
                1024,
                Some("b".repeat(32)),
                "accepted-manifest.bin",
            )
            .await
            .unwrap();
            assert!(resumed.resumed);
            assert_eq!(
                resumed.snapshot.logical_file_id.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
            assert_eq!(resumed.snapshot.manifest_message_id, Some(1802));
            assert_eq!(resumed.snapshot.chunks[&1].message_id, 1801);
            assert!(!resumed.snapshot.completed);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn upload_resume_binds_name_before_reusing_saved_remote_chunks() {
        let dir = scratch();
        let file = dir.join("original.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        let path = file.to_str().unwrap();
        let db = Arc::new(Mutex::new(sqlite::open(":memory:").unwrap()));
        let mut initial = UploadJournal::open(
            db.clone(),
            path,
            "keep-name",
            Some(91),
            10,
            Some("f".repeat(32)),
            "Original.bin",
        )
        .await
        .unwrap();
        initial
            .record(ManifestChunkV1 {
                index: 1,
                message_id: 8123,
                size: 10,
                sha256: None,
            })
            .unwrap();
        let key = initial.key.clone();
        let created_at = initial.snapshot.created_at;
        assert!(created_at.unwrap() > 0);
        drop(initial);

        let same = UploadJournal::open(
            db.clone(),
            path,
            "keep-name",
            Some(91),
            10,
            Some("e".repeat(32)),
            "Original.bin",
        )
        .await
        .unwrap();
        assert!(same.resumed);
        assert_eq!(
            same.snapshot.logical_file_id.as_deref(),
            Some("ffffffffffffffffffffffffffffffff")
        );
        assert_eq!(same.snapshot.display_name.as_deref(), Some("Original.bin"));
        assert_eq!(same.snapshot.created_at, created_at);
        assert_eq!(same.snapshot.chunks[&1].message_id, 8123);
        drop(same);

        let error = UploadJournal::open(
            db.clone(),
            path,
            "keep-name",
            Some(91),
            10,
            Some("a".repeat(32)),
            "Changed.bin",
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("filename changed"), "{error}");
        let conn = db.lock().unwrap();
        let mut statement = conn
            .prepare("SELECT state FROM upload_checkpoints WHERE id=?1")
            .unwrap();
        statement.bind((1, key.as_str())).unwrap();
        assert_eq!(statement.next().unwrap(), sqlite::State::Row);
        let state: UploadCheckpoint =
            serde_json::from_str(&statement.read::<String, _>(0).unwrap()).unwrap();
        assert_eq!(state.display_name.as_deref(), Some("Original.bin"));
        drop(statement);
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn old_checkpoint_adds_name_without_losing_saved_parts() {
        let dir = scratch();
        let file = dir.join("legacy.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        let path = file.to_str().unwrap();
        let db = Arc::new(Mutex::new(sqlite::open(":memory:").unwrap()));
        let mut journal = UploadJournal::open(
            db.clone(),
            path,
            "old-checkpoint",
            Some(91),
            10,
            None,
            "Legacy.bin",
        )
        .await
        .unwrap();
        journal
            .record(ManifestChunkV1 {
                index: 1,
                message_id: 444,
                size: 10,
                sha256: None,
            })
            .unwrap();
        let key = journal.key.clone();
        drop(journal);
        {
            let conn = db.lock().unwrap();
            let mut statement = conn
                .prepare("SELECT state FROM upload_checkpoints WHERE id=?1")
                .unwrap();
            statement.bind((1, key.as_str())).unwrap();
            assert_eq!(statement.next().unwrap(), sqlite::State::Row);
            let mut original: Value =
                serde_json::from_str(&statement.read::<String, _>(0).unwrap()).unwrap();
            drop(statement);
            original.as_object_mut().unwrap().remove("display_name");
            original.as_object_mut().unwrap().remove("created_at");
            // Store literal legacy JSON with both fields ABSENT, not null:
            // serde defaults must accept an actual pre-upgrade checkpoint.
            let text = serde_json::to_string(&original).unwrap();
            let mut update = conn
                .prepare("UPDATE upload_checkpoints SET state=?1 WHERE id=?2")
                .unwrap();
            update.bind((1, text.as_str())).unwrap();
            update.bind((2, key.as_str())).unwrap();
            update.next().unwrap();
        }
        let recovered = UploadJournal::open(
            db.clone(),
            path,
            "old-checkpoint",
            Some(91),
            10,
            None,
            "Recovered.bin",
        )
        .await
        .unwrap();
        assert!(recovered.resumed);
        assert_eq!(recovered.snapshot.chunks[&1].message_id, 444);
        assert_eq!(
            recovered.snapshot.display_name.as_deref(),
            Some("Recovered.bin")
        );
        drop(recovered);
        let error = UploadJournal::open(
            db.clone(),
            path,
            "old-checkpoint",
            Some(91),
            10,
            None,
            "Other.bin",
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("filename changed"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn missing_transfer_id_cannot_reuse_previous_completed_upload_identity() {
        let dir = scratch();
        let file = dir.join("same.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        let path = file.to_str().unwrap();
        let db = Arc::new(Mutex::new(sqlite::open(":memory:").unwrap()));
        let mut first = UploadJournal::open(
            db.clone(),
            path,
            "",
            Some(91),
            10,
            Some("a".repeat(32)),
            "same.bin",
        )
        .await
        .unwrap();
        first
            .record(ManifestChunkV1 {
                index: 1,
                message_id: 2345,
                size: 10,
                sha256: None,
            })
            .unwrap();
        first.complete(Some(3333)).unwrap();
        let first_key = first.key.clone();
        drop(first);

        let second = UploadJournal::open(
            db.clone(),
            path,
            "",
            Some(91),
            10,
            Some("b".repeat(32)),
            "same.bin",
        )
        .await
        .unwrap();
        assert!(!second.resumed);
        assert_ne!(second.key, first_key);
        assert_eq!(second.snapshot.logical_file_id, Some("b".repeat(32)));
        assert!(second.snapshot.chunks.is_empty());
        assert_eq!(second.snapshot.manifest_message_id, None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
