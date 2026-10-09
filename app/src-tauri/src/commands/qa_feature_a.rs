use crate::commands::file_stacks::{collapse_files, validate_stack_manifest, FileStackManifestV1};
use crate::db::DbConnection;
use crate::models::FileMetadata;
use std::path::Path;
use tauri::State;

const QA_MOVIES_FOLDER_ID: i64 = 9_100_001;
const QA_ARCHIVE_FOLDER_ID: i64 = 9_100_002;
const QA_MOVIES_LOGICAL_ID: &str = "qa-feature-a-movies";
const QA_ARCHIVE_LOGICAL_ID: &str = "qa-feature-a-archive";

pub fn enabled() -> bool {
    cfg!(debug_assertions)
        && std::env::var("TERARELAY_QA_FEATURE_A")
            .map(|value| value == "1")
            .unwrap_or(false)
}

fn require_enabled() -> Result<(), String> {
    if enabled() {
        Ok(())
    } else {
        Err("Feature A QA mode is available only in an explicitly enabled debug build".to_string())
    }
}

fn ensure_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS qa_feature_a_stacks (
            stack_id TEXT PRIMARY KEY,
            logical_channel_id TEXT NOT NULL,
            manifest_json TEXT NOT NULL
        );",
    )
    .map_err(|e| e.to_string())
}

fn upsert_folder(
    conn: &sqlite::Connection,
    folder_id: i64,
    logical_id: &str,
    name: &str,
    display_order: i64,
) -> Result<(), String> {
    let mut folder = conn
        .prepare(
            "INSERT INTO folder_metadata
             (channel_id, name, username, is_public, display_order, group_id)
             VALUES (?, ?, NULL, 0, ?, NULL)
             ON CONFLICT(channel_id) DO UPDATE SET
               name = excluded.name,
               display_order = excluded.display_order",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    folder
        .bind((1, folder_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    folder
        .bind((2, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    folder
        .bind((3, display_order))
        .map_err(|e: sqlite::Error| e.to_string())?;
    folder.next().map_err(|e: sqlite::Error| e.to_string())?;

    let now = chrono::Utc::now().timestamp();
    let mut logical = conn
        .prepare(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES (?, ?, ?, 'owner', 1, ?, ?)
             ON CONFLICT(logical_id) DO UPDATE SET
               backing_channel_id = excluded.backing_channel_id,
               name = excluded.name,
               role = 'owner'",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((1, logical_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((2, folder_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((3, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((4, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((5, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

fn insert_seed_file(
    conn: &sqlite::Connection,
    file_id: &str,
    logical_channel_id: &str,
    message_id: i64,
    display_name: &str,
    total_size: i64,
    mime_type: &str,
    created_at: i64,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT OR IGNORE INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES (?, ?, ?, ?, ?, ?, 1, NULL, 1, ?, 'complete')",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, display_name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, total_size))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, mime_type))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, created_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cmd_qa_feature_a_seed(db_pool: State<'_, DbConnection>) -> Result<bool, String> {
    require_enabled()?;
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    ensure_schema(&conn)?;

    upsert_folder(
        &conn,
        QA_MOVIES_FOLDER_ID,
        QA_MOVIES_LOGICAL_ID,
        "Movies QA",
        0,
    )?;
    upsert_folder(
        &conn,
        QA_ARCHIVE_FOLDER_ID,
        QA_ARCHIVE_LOGICAL_ID,
        "Archive QA",
        1,
    )?;

    let now = chrono::Utc::now().timestamp();
    insert_seed_file(
        &conn,
        "11111111111111111111111111111111",
        QA_MOVIES_LOGICAL_ID,
        91_001,
        "Interstellar.1080p.BluRay.mkv",
        2_100_000_000,
        "video/x-matroska",
        now - 86_400 * 60,
    )?;
    insert_seed_file(
        &conn,
        "22222222222222222222222222222222",
        QA_MOVIES_LOGICAL_ID,
        91_002,
        "Interstellar.2160p.HDR.mkv",
        4_300_000_000,
        "video/x-matroska",
        now - 86_400 * 10,
    )?;
    insert_seed_file(
        &conn,
        "33333333333333333333333333333333",
        QA_MOVIES_LOGICAL_ID,
        91_003,
        "Dune.Part.Two.2160p.mkv",
        5_600_000_000,
        "video/x-matroska",
        now - 86_400 * 2,
    )?;
    insert_seed_file(
        &conn,
        "44444444444444444444444444444444",
        QA_MOVIES_LOGICAL_ID,
        91_004,
        "TeraRelay-Feature-A-notes.pdf",
        1_250_000,
        "application/pdf",
        now - 86_400,
    )?;

    Ok(true)
}

fn logical_channel_id_for_folder(
    conn: &sqlite::Connection,
    folder_id: i64,
) -> Result<String, String> {
    crate::commands::logical_channels::logical_channel_id_for_backing(conn, folder_id)?
        .ok_or_else(|| "QA folder is missing logical channel metadata".to_string())
}

fn metadata_from_row(stmt: &sqlite::Statement, folder_id: i64) -> Result<FileMetadata, String> {
    let file_id = stmt
        .read::<String, _>("file_id")
        .map_err(|e| e.to_string())?;
    let name = stmt
        .read::<String, _>("display_name")
        .map_err(|e| e.to_string())?;
    let created_at = stmt
        .read::<i64, _>("created_at")
        .map_err(|e| e.to_string())?;
    let created = chrono::DateTime::<chrono::Utc>::from_timestamp(created_at, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| created_at.to_string());
    let ext = Path::new(&name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_string);

    Ok(FileMetadata {
        id: stmt
            .read::<i64, _>("first_message_id")
            .map_err(|e| e.to_string())?,
        folder_id: Some(folder_id),
        name,
        size: stmt
            .read::<i64, _>("total_size")
            .map_err(|e| e.to_string())? as u64,
        mime_type: stmt.read::<Option<String>, _>("mime_type").ok().flatten(),
        file_ext: ext,
        created_at: created,
        icon_type: "file".to_string(),
        is_split: stmt
            .read::<i64, _>("chunk_count")
            .map_err(|e| e.to_string())?
            > 1,
        logical_file_id: Some(file_id),
        stack_id: None,
        stack_name: None,
        stack_version_count: 0,
        stack_label: None,
    })
}

pub fn list_files(conn: &sqlite::Connection, folder_id: i64) -> Result<Vec<FileMetadata>, String> {
    require_enabled()?;
    ensure_schema(conn)?;
    let logical_channel_id = logical_channel_id_for_folder(conn, folder_id)?;
    let mut stmt = conn
        .prepare(
            "SELECT file_id, first_message_id, display_name, total_size, mime_type,
                    chunk_count, created_at
             FROM logical_files
             WHERE logical_channel_id = ? AND status = 'complete'
             ORDER BY created_at DESC, first_message_id DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;

    let mut files = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        files.push(metadata_from_row(&stmt, folder_id)?);
    }
    let manifests = load_manifests(conn, &logical_channel_id)?;
    Ok(collapse_files(files, &manifests))
}

pub fn load_manifests(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
) -> Result<Vec<FileStackManifestV1>, String> {
    require_enabled()?;
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT manifest_json FROM qa_feature_a_stacks
             WHERE logical_channel_id = ?
             ORDER BY rowid DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;

    let mut manifests = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let raw = stmt.read::<String, _>(0).map_err(|e| e.to_string())?;
        let manifest: FileStackManifestV1 =
            serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        validate_stack_manifest(
            &manifest,
            Some(&manifest.stack_id),
            Some(logical_channel_id),
        )?;
        manifests.push(manifest);
    }
    Ok(manifests)
}

pub fn load_manifest(
    conn: &sqlite::Connection,
    stack_id: &str,
    logical_channel_id: &str,
) -> Result<FileStackManifestV1, String> {
    require_enabled()?;
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT manifest_json FROM qa_feature_a_stacks
             WHERE stack_id = ? AND logical_channel_id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, stack_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let raw = stmt.read::<String, _>(0).map_err(|e| e.to_string())?;
        let manifest: FileStackManifestV1 =
            serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        validate_stack_manifest(&manifest, Some(stack_id), Some(logical_channel_id))?;
        return Ok(manifest);
    }
    Err("QA version stack was not found".to_string())
}

pub fn publish_manifest(
    conn: &sqlite::Connection,
    manifest: &FileStackManifestV1,
) -> Result<(), String> {
    require_enabled()?;
    ensure_schema(conn)?;
    validate_stack_manifest(
        manifest,
        Some(&manifest.stack_id),
        Some(&manifest.logical_channel_id),
    )?;
    let raw = serde_json::to_string(manifest).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "INSERT INTO qa_feature_a_stacks(stack_id, logical_channel_id, manifest_json)
             VALUES (?, ?, ?)
             ON CONFLICT(stack_id) DO UPDATE SET
               logical_channel_id = excluded.logical_channel_id,
               manifest_json = excluded.manifest_json",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, manifest.stack_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, manifest.logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, raw.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn delete_stack(conn: &sqlite::Connection, stack_id: &str) -> Result<(), String> {
    require_enabled()?;
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare("DELETE FROM qa_feature_a_stacks WHERE stack_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, stack_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn record_upload(
    conn: &sqlite::Connection,
    folder_id: i64,
    file_id: &str,
    path: &str,
    size: u64,
    whole_sha256: Option<&str>,
) -> Result<i64, String> {
    require_enabled()?;
    let logical_channel_id = logical_channel_id_for_folder(conn, folder_id)?;
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "QA upload has no file name".to_string())?;
    let mut next_stmt = conn
        .prepare("SELECT COALESCE(MAX(first_message_id), 91000) + 1 FROM logical_files")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let message_id =
        if let sqlite::State::Row = next_stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            next_stmt.read::<i64, _>(0).map_err(|e| e.to_string())?
        } else {
            91_100
        };
    drop(next_stmt);

    let mime = match Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mkv" => "video/x-matroska",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };

    let mut stmt = conn
        .prepare(
            "INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?, 1, ?, 'complete')",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, mime))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, whole_sha256))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((8, chrono::Utc::now().timestamp()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(message_id)
}

pub fn rename_file(
    conn: &sqlite::Connection,
    folder_id: i64,
    message_id: i32,
    new_name: &str,
) -> Result<(), String> {
    require_enabled()?;
    let logical_channel_id = logical_channel_id_for_folder(conn, folder_id)?;
    let mut stmt = conn
        .prepare(
            "UPDATE logical_files SET display_name = ?
             WHERE logical_channel_id = ? AND first_message_id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, new_name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, message_id as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn delete_file(
    conn: &sqlite::Connection,
    folder_id: i64,
    message_id: i32,
) -> Result<(), String> {
    require_enabled()?;
    let logical_channel_id = logical_channel_id_for_folder(conn, folder_id)?;
    let mut lookup = conn
        .prepare(
            "SELECT file_id FROM logical_files
             WHERE logical_channel_id = ? AND first_message_id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    lookup
        .bind((1, logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    lookup
        .bind((2, message_id as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let file_id =
        if let sqlite::State::Row = lookup.next().map_err(|e: sqlite::Error| e.to_string())? {
            Some(lookup.read::<String, _>(0).map_err(|e| e.to_string())?)
        } else {
            None
        };
    drop(lookup);

    if let Some(file_id) = file_id {
        let mut chunks = conn
            .prepare("DELETE FROM logical_file_chunks WHERE file_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        chunks
            .bind((1, file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        chunks.next().map_err(|e: sqlite::Error| e.to_string())?;
        drop(chunks);

        let mut file = conn
            .prepare("DELETE FROM logical_files WHERE file_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        file.bind((1, file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        file.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(())
}

pub fn move_files(
    conn: &sqlite::Connection,
    source_folder_id: i64,
    target_folder_id: i64,
    message_ids: &[i32],
) -> Result<(), String> {
    require_enabled()?;
    let source = logical_channel_id_for_folder(conn, source_folder_id)?;
    let target = logical_channel_id_for_folder(conn, target_folder_id)?;
    for message_id in message_ids {
        let mut stmt = conn
            .prepare(
                "UPDATE logical_files SET logical_channel_id = ?
                 WHERE logical_channel_id = ? AND first_message_id = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, target.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, source.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((3, *message_id as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(())
}

pub fn move_stack_members(
    conn: &sqlite::Connection,
    source_folder_id: i64,
    target_folder_id: i64,
    manifest: &mut FileStackManifestV1,
) -> Result<(), String> {
    require_enabled()?;
    let source = logical_channel_id_for_folder(conn, source_folder_id)?;
    let target = logical_channel_id_for_folder(conn, target_folder_id)?;
    for member in &manifest.members {
        let mut stmt = conn
            .prepare(
                "UPDATE logical_files SET logical_channel_id = ?
                 WHERE logical_channel_id = ? AND file_id = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, target.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, source.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((3, member.file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    manifest.logical_channel_id = target;
    Ok(())
}

pub fn delete_manifest_files(
    conn: &sqlite::Connection,
    manifest: &FileStackManifestV1,
) -> Result<usize, String> {
    require_enabled()?;
    let mut deleted = 0usize;
    for member in &manifest.members {
        let mut stmt = conn
            .prepare("DELETE FROM logical_files WHERE file_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, member.file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        deleted += 1;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::file_stacks::{FileStackMemberV1, STACK_SCHEMA_VERSION};
    use std::sync::{Mutex, OnceLock};

    static QA_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct QaEnvGuard {
        previous: Option<String>,
    }

    impl QaEnvGuard {
        fn enable() -> Self {
            let previous = std::env::var("TERARELAY_QA_FEATURE_A").ok();
            std::env::set_var("TERARELAY_QA_FEATURE_A", "1");
            Self { previous }
        }
    }

    impl Drop for QaEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("TERARELAY_QA_FEATURE_A", value),
                None => std::env::remove_var("TERARELAY_QA_FEATURE_A"),
            }
        }
    }

    fn test_conn() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").expect("open in-memory sqlite");
        conn.execute(
            "CREATE TABLE folder_metadata (
                channel_id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                username TEXT,
                is_public INTEGER NOT NULL DEFAULT 0,
                display_order INTEGER NOT NULL DEFAULT 0,
                group_id INTEGER
            );
            CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY,
                backing_channel_id INTEGER NOT NULL UNIQUE,
                name TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'owner',
                storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                joined_at INTEGER NOT NULL
            );
            CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY,
                logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL,
                display_name TEXT NOT NULL,
                total_size INTEGER NOT NULL,
                mime_type TEXT,
                chunk_count INTEGER NOT NULL DEFAULT 1,
                whole_sha256 TEXT,
                storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'complete'
            );
            CREATE TABLE logical_file_chunks (
                file_id TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                chunk_size INTEGER NOT NULL,
                sha256 TEXT,
                PRIMARY KEY(file_id, chunk_index)
            );",
        )
        .expect("create QA schema");
        ensure_schema(&conn).expect("create stack schema");
        conn
    }

    fn seed_two_files(conn: &sqlite::Connection) {
        upsert_folder(
            conn,
            QA_MOVIES_FOLDER_ID,
            QA_MOVIES_LOGICAL_ID,
            "Movies QA",
            0,
        )
        .expect("seed movies folder");
        upsert_folder(
            conn,
            QA_ARCHIVE_FOLDER_ID,
            QA_ARCHIVE_LOGICAL_ID,
            "Archive QA",
            1,
        )
        .expect("seed archive folder");
        insert_seed_file(
            conn,
            "11111111111111111111111111111111",
            QA_MOVIES_LOGICAL_ID,
            91_001,
            "Interstellar.1080p.BluRay.mkv",
            2_100_000_000,
            "video/x-matroska",
            100,
        )
        .expect("seed 1080p file");
        insert_seed_file(
            conn,
            "22222222222222222222222222222222",
            QA_MOVIES_LOGICAL_ID,
            91_002,
            "Interstellar.2160p.HDR.mkv",
            4_300_000_000,
            "video/x-matroska",
            200,
        )
        .expect("seed 4K file");
    }

    fn test_manifest(logical_channel_id: &str) -> FileStackManifestV1 {
        FileStackManifestV1 {
            schema_version: STACK_SCHEMA_VERSION,
            stack_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            logical_channel_id: logical_channel_id.to_string(),
            display_name: "Interstellar".to_string(),
            primary_file_id: "11111111111111111111111111111111".to_string(),
            anchor_file_id: "11111111111111111111111111111111".to_string(),
            created_at: 300,
            updated_at: 300,
            members: vec![
                FileStackMemberV1 {
                    file_id: "11111111111111111111111111111111".to_string(),
                    label: Some("1080P · BluRay".to_string()),
                    added_at: 300,
                },
                FileStackMemberV1 {
                    file_id: "22222222222222222222222222222222".to_string(),
                    label: Some("4K · HDR".to_string()),
                    added_at: 301,
                },
            ],
        }
    }

    #[test]
    fn qa_stack_rename_and_unstack_preserve_member_files() {
        let _serial = QA_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _env = QaEnvGuard::enable();
        let conn = test_conn();
        seed_two_files(&conn);

        let mut manifest = test_manifest(QA_MOVIES_LOGICAL_ID);
        publish_manifest(&conn, &manifest).expect("publish stack");

        let collapsed = list_files(&conn, QA_MOVIES_FOLDER_ID).expect("list collapsed stack");
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].stack_version_count, 2);

        manifest.display_name = "Interstellar QA".to_string();
        manifest.updated_at += 1;
        publish_manifest(&conn, &manifest).expect("rename stack manifest");
        let renamed = load_manifest(&conn, &manifest.stack_id, QA_MOVIES_LOGICAL_ID)
            .expect("load renamed stack");
        assert_eq!(renamed.display_name, "Interstellar QA");

        delete_stack(&conn, &manifest.stack_id).expect("unstack");
        assert!(load_manifest(&conn, &manifest.stack_id, QA_MOVIES_LOGICAL_ID).is_err());

        let visible = list_files(&conn, QA_MOVIES_FOLDER_ID).expect("list unstacked files");
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|file| file.stack_id.is_none()));
    }

    #[test]
    fn qa_stack_move_and_delete_all_affect_every_member() {
        let _serial = QA_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _env = QaEnvGuard::enable();
        let conn = test_conn();
        seed_two_files(&conn);

        let mut manifest = test_manifest(QA_MOVIES_LOGICAL_ID);
        publish_manifest(&conn, &manifest).expect("publish stack");
        move_stack_members(
            &conn,
            QA_MOVIES_FOLDER_ID,
            QA_ARCHIVE_FOLDER_ID,
            &mut manifest,
        )
        .expect("move stack members");
        publish_manifest(&conn, &manifest).expect("publish moved stack");

        assert_eq!(
            list_files(&conn, QA_MOVIES_FOLDER_ID)
                .expect("list source")
                .len(),
            0
        );
        let target = list_files(&conn, QA_ARCHIVE_FOLDER_ID).expect("list target");
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].stack_version_count, 2);

        assert_eq!(
            delete_manifest_files(&conn, &manifest).expect("delete stack files"),
            2
        );
        delete_stack(&conn, &manifest.stack_id).expect("delete moved stack metadata");
        assert_eq!(
            list_files(&conn, QA_ARCHIVE_FOLDER_ID)
                .expect("list target after delete")
                .len(),
            0
        );
    }

    #[test]
    fn qa_single_file_rename_and_delete_are_scoped_to_that_file() {
        let _serial = QA_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _env = QaEnvGuard::enable();
        let conn = test_conn();
        seed_two_files(&conn);

        rename_file(
            &conn,
            QA_MOVIES_FOLDER_ID,
            91_002,
            "Interstellar.4K.HDR.REMUX.mkv",
        )
        .expect("rename one file");
        let renamed = list_files(&conn, QA_MOVIES_FOLDER_ID).expect("list renamed files");
        assert!(renamed
            .iter()
            .any(|file| file.name == "Interstellar.4K.HDR.REMUX.mkv"));
        assert!(renamed
            .iter()
            .any(|file| file.name == "Interstellar.1080p.BluRay.mkv"));

        delete_file(&conn, QA_MOVIES_FOLDER_ID, 91_002).expect("delete one file");
        let remaining = list_files(&conn, QA_MOVIES_FOLDER_ID).expect("list remaining files");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "Interstellar.1080p.BluRay.mkv");
    }
}
