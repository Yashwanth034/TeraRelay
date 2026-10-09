use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use grammers_client::types::{Media, Peer};
use grammers_client::{Client, InputMessage};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

pub const DRIVE_SCHEMA_VERSION: u32 = 3;
const LEGACY_DRIVE_SCHEMA_VERSION: u32 = 1;
const PRE_STORAGE_POOL_DRIVE_SCHEMA_VERSION: u32 = 2;
const DRIVE_MANIFEST_FILE: &str = ".terarelay-drive-index.json";
const DRIVE_MANIFEST_CAPTION: &str = "TRD1:index";
// V3 carries cross-device storage-pool part metadata. A 10 TB mounted file
// can contain tens of thousands of remote parts, so the old 4 MiB cap was too
// small even though the actual file bytes never live in this manifest.
const MAX_DRIVE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveDirectoryRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub backing_logical_channel_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Synced tombstone. Older V1 manifests omit this field, so absence means active.
    #[serde(default)]
    pub trashed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveEntryRecord {
    pub entry_id: String,
    pub file_id: String,
    pub directory_id: Option<String>,
    pub display_name: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub encryption_version: u32,
    #[serde(default)]
    pub plaintext_size: Option<u64>,
    #[serde(default)]
    pub crypto_id: Option<String>,
    #[serde(default)]
    pub content_fingerprint: Option<String>,
    /// Synced tombstone. Keeping deleted records in the manifest prevents an
    /// offline device from resurrecting them when it reconnects.
    #[serde(default)]
    pub trashed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveStorageManifestRecord {
    pub generation: i64,
    pub backing_channel_id: i64,
    pub state: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub retirement_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveObjectRecord {
    pub object_id: String,
    pub plaintext_size: u64,
    pub encryption_version: u32,
    #[serde(default)]
    pub crypto_id: Option<String>,
    pub content_fingerprint: String,
    pub block_count: u64,
    /// Compact concatenation of the 32-byte SHA-256 for each encoded block,
    /// base64-encoded for cross-device random-read integrity verification.
    #[serde(default)]
    pub block_hashes_b64: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveEntryV2Record {
    pub entry_id: String,
    pub object_id: String,
    #[serde(default)]
    pub directory_id: Option<String>,
    pub display_name: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub trashed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveManifestV1 {
    pub schema_version: u32,
    pub revision: i64,
    #[serde(default)]
    pub crypto: Option<crate::drive_crypto::DriveCryptoEnvelope>,
    pub directories: Vec<DriveDirectoryRecord>,
    pub entries: Vec<DriveEntryRecord>,
    #[serde(default)]
    pub storage_channels: Vec<DriveStorageManifestRecord>,
    #[serde(default)]
    pub objects: Vec<DriveObjectRecord>,
    #[serde(default)]
    pub parts: Vec<DriveObjectPartRecord>,
    #[serde(default)]
    pub v2_entries: Vec<DriveEntryV2Record>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveFileRecord {
    pub entry_id: String,
    pub file_id: String,
    pub directory_id: Option<String>,
    pub display_name: String,
    pub total_size: u64,
    pub mime_type: Option<String>,
    pub first_message_id: i64,
    pub backing_channel_id: i64,
    pub logical_channel_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub encryption_version: u32,
    pub plaintext_size: Option<u64>,
    pub crypto_id: Option<String>,
    pub content_fingerprint: Option<String>,
    /// 1 = legacy logical-file-backed Drive entry, 2 = Drive storage-pool object.
    pub storage_version: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrivePendingRecord {
    pub id: String,
    pub directory_id: Option<String>,
    pub display_name: String,
    pub staging_path: String,
    pub size: u64,
    /// 1 = legacy one-message-per-block pending write, 2 = storage-pool V2.
    pub format_version: u32,
    pub backing_channel_id: i64,
    /// When present this sparse pending file is a copy-on-write replacement of
    /// an existing Drive entry rather than a brand-new path.
    pub base_entry_id: Option<String>,
    pub encryption_version: u32,
    pub crypto_id: Option<String>,
    pub closed_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrivePendingChunkRecord {
    pub pending_id: String,
    pub chunk_index: u64,
    pub message_id: i64,
    pub chunk_size: u64,
    pub sha256: String,
    pub plaintext_sha256: String,
    pub uploaded_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DrivePendingPartRecord {
    pub pending_id: String,
    pub part_index: u64,
    pub first_chunk_index: u64,
    pub chunk_count: u64,
    /// Exact Drive storage channel holding this remote part. Part-level location
    /// keeps one logical file readable even if a future rollover changes channel.
    pub backing_channel_id: i64,
    pub message_id: i64,
    pub part_size: u64,
    pub sha256: String,
    pub uploaded_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveV2FinalizeResult {
    pub object_id: String,
    pub entry_id: String,
    pub staging_path: String,
    pub reused_existing_object: bool,
    /// Temporary pending parts that should be deleted remotely only when an
    /// already-complete object was reused. New objects own these same messages.
    pub cleanup_parts: Vec<DrivePendingPartRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveObjectPartRecord {
    pub object_id: String,
    pub part_index: u64,
    pub first_chunk_index: u64,
    pub chunk_count: u64,
    pub backing_channel_id: i64,
    pub message_id: i64,
    pub ciphertext_size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveObjectBlockRecord {
    pub object_id: String,
    pub chunk_index: u64,
    pub part_index: u64,
    pub part_offset: u64,
    pub ciphertext_size: u64,
    pub sha256: String,
    pub plaintext_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveSnapshot {
    pub revision: i64,
    pub dirty: bool,
    pub directories: Vec<DriveDirectoryRecord>,
    pub files: Vec<DriveFileRecord>,
    pub pending: Vec<DrivePendingRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveSyncResult {
    pub revision: i64,
    pub dirty: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveTrashItem {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub deleted_at: i64,
    pub original_directory_id: Option<String>,
    pub size: Option<u64>,
    pub restorable: bool,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn new_drive_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_drive_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Drive name cannot be empty".to_string());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Drive name is reserved".to_string());
    }
    if trimmed.contains('/') || trimmed.contains('\0') {
        return Err("Drive name contains an invalid character".to_string());
    }
    if trimmed.len() > 255 {
        return Err("Drive name is too long".to_string());
    }
    Ok(trimmed.to_string())
}

pub fn init_drive_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            revision INTEGER NOT NULL DEFAULT 0,
            dirty INTEGER NOT NULL DEFAULT 0,
            last_remote_message_id INTEGER
        );
        INSERT OR IGNORE INTO drive_state(singleton, revision, dirty)
            VALUES (1, 0, 0);

        CREATE TABLE IF NOT EXISTS drive_directories (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            name TEXT NOT NULL,
            backing_logical_channel_id TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            trashed_at INTEGER,
            FOREIGN KEY(parent_id) REFERENCES drive_directories(id) ON DELETE RESTRICT,
            FOREIGN KEY(backing_logical_channel_id) REFERENCES logical_channels(logical_id) ON DELETE SET NULL
        );
        CREATE INDEX IF NOT EXISTS idx_drive_directories_parent
            ON drive_directories(parent_id, trashed_at, name);

        CREATE TABLE IF NOT EXISTS drive_file_entries (
            entry_id TEXT PRIMARY KEY,
            file_id TEXT NOT NULL,
            directory_id TEXT,
            display_name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            trashed_at INTEGER,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            plaintext_size INTEGER,
            crypto_id TEXT,
            content_fingerprint TEXT,
            FOREIGN KEY(file_id) REFERENCES logical_files(file_id) ON DELETE CASCADE,
            FOREIGN KEY(directory_id) REFERENCES drive_directories(id) ON DELETE SET NULL
        );
        CREATE INDEX IF NOT EXISTS idx_drive_file_entries_directory
            ON drive_file_entries(directory_id, trashed_at, display_name);
        CREATE INDEX IF NOT EXISTS idx_drive_file_entries_file
            ON drive_file_entries(file_id, trashed_at);

        CREATE TABLE IF NOT EXISTS drive_pending_files (
            id TEXT PRIMARY KEY,
            directory_id TEXT,
            display_name TEXT NOT NULL,
            staging_path TEXT NOT NULL,
            size INTEGER NOT NULL DEFAULT 0,
            format_version INTEGER NOT NULL DEFAULT 1,
            backing_channel_id INTEGER NOT NULL,
            base_entry_id TEXT,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            crypto_id TEXT,
            closed_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY(directory_id) REFERENCES drive_directories(id) ON DELETE SET NULL
        );
        CREATE INDEX IF NOT EXISTS idx_drive_pending_directory
            ON drive_pending_files(directory_id, display_name);

        CREATE TABLE IF NOT EXISTS drive_pending_ranges (
            pending_id TEXT NOT NULL,
            range_start INTEGER NOT NULL,
            range_end INTEGER NOT NULL,
            PRIMARY KEY(pending_id, range_start, range_end)
        );
        CREATE INDEX IF NOT EXISTS idx_drive_pending_ranges
            ON drive_pending_ranges(pending_id, range_start, range_end);

        CREATE TABLE IF NOT EXISTS drive_pending_chunks (
            pending_id TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            message_id INTEGER NOT NULL,
            chunk_size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            plaintext_sha256 TEXT NOT NULL DEFAULT '',
            uploaded_at INTEGER NOT NULL,
            PRIMARY KEY(pending_id, chunk_index)
        );
        CREATE INDEX IF NOT EXISTS idx_drive_pending_chunks
            ON drive_pending_chunks(pending_id, chunk_index);

        CREATE TABLE IF NOT EXISTS drive_pending_parts (
            pending_id TEXT NOT NULL,
            part_index INTEGER NOT NULL,
            first_chunk_index INTEGER NOT NULL,
            chunk_count INTEGER NOT NULL,
            backing_channel_id INTEGER NOT NULL DEFAULT 0,
            message_id INTEGER NOT NULL,
            part_size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            uploaded_at INTEGER NOT NULL,
            PRIMARY KEY(pending_id, part_index)
        );
        CREATE INDEX IF NOT EXISTS idx_drive_pending_parts_chunks
            ON drive_pending_parts(pending_id, first_chunk_index);

        CREATE TABLE IF NOT EXISTS drive_objects (
            object_id TEXT PRIMARY KEY,
            plaintext_size INTEGER NOT NULL,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            crypto_id TEXT,
            content_fingerprint TEXT NOT NULL,
            block_count INTEGER NOT NULL,
            block_hashes_b64 TEXT,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_drive_objects_fingerprint
            ON drive_objects(content_fingerprint, plaintext_size, encryption_version);

        CREATE TABLE IF NOT EXISTS drive_object_parts (
            object_id TEXT NOT NULL,
            part_index INTEGER NOT NULL,
            first_chunk_index INTEGER NOT NULL,
            chunk_count INTEGER NOT NULL,
            backing_channel_id INTEGER NOT NULL,
            message_id INTEGER NOT NULL,
            ciphertext_size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            PRIMARY KEY(object_id, part_index),
            FOREIGN KEY(object_id) REFERENCES drive_objects(object_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_drive_object_parts_location
            ON drive_object_parts(backing_channel_id, message_id);

        CREATE TABLE IF NOT EXISTS drive_object_blocks (
            object_id TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            part_index INTEGER NOT NULL,
            part_offset INTEGER NOT NULL,
            ciphertext_size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            plaintext_sha256 TEXT NOT NULL,
            PRIMARY KEY(object_id, chunk_index),
            FOREIGN KEY(object_id) REFERENCES drive_objects(object_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_drive_object_blocks_part
            ON drive_object_blocks(object_id, part_index, chunk_index);

        CREATE TABLE IF NOT EXISTS drive_entries_v2 (
            entry_id TEXT PRIMARY KEY,
            object_id TEXT NOT NULL,
            directory_id TEXT,
            display_name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            trashed_at INTEGER,
            FOREIGN KEY(object_id) REFERENCES drive_objects(object_id),
            FOREIGN KEY(directory_id) REFERENCES drive_directories(id) ON DELETE SET NULL
        );
        CREATE INDEX IF NOT EXISTS idx_drive_entries_v2_directory
            ON drive_entries_v2(directory_id, trashed_at, display_name);",
    )
    .map_err(|e: sqlite::Error| e.to_string())?;

    // Early Drive prototypes keyed virtual entries directly by file_id. That
    // prevents two normal filesystem paths from referencing the same verified
    // remote content. Migrate that private, unreleased schema in-place so exact
    // duplicate content can be de-duplicated without turning a copy into a move.
    let mut columns = conn
        .prepare("PRAGMA table_info(drive_file_entries)")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut has_entry_id = false;
    while let sqlite::State::Row = columns.next().map_err(|e: sqlite::Error| e.to_string())? {
        let name = columns
            .read::<String, _>("name")
            .map_err(|e| e.to_string())?;
        if name == "entry_id" {
            has_entry_id = true;
            break;
        }
    }
    drop(columns);

    if !has_entry_id {
        conn.execute(
            "BEGIN IMMEDIATE;
             DROP INDEX IF EXISTS idx_drive_file_entries_directory;
             DROP INDEX IF EXISTS idx_drive_file_entries_file;
             ALTER TABLE drive_file_entries RENAME TO drive_file_entries_legacy;
             CREATE TABLE drive_file_entries (
                entry_id TEXT PRIMARY KEY,
                file_id TEXT NOT NULL,
                directory_id TEXT,
                display_name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                trashed_at INTEGER,
                encryption_version INTEGER NOT NULL DEFAULT 0,
                plaintext_size INTEGER,
                crypto_id TEXT,
                FOREIGN KEY(file_id) REFERENCES logical_files(file_id) ON DELETE CASCADE,
                FOREIGN KEY(directory_id) REFERENCES drive_directories(id) ON DELETE SET NULL
             );
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id)
             SELECT
                'legacy-' || file_id, file_id, directory_id, display_name, created_at, updated_at,
                trashed_at, encryption_version, plaintext_size, crypto_id
             FROM drive_file_entries_legacy;
             DROP TABLE drive_file_entries_legacy;
             CREATE INDEX idx_drive_file_entries_directory
                ON drive_file_entries(directory_id, trashed_at, display_name);
             CREATE INDEX idx_drive_file_entries_file
                ON drive_file_entries(file_id, trashed_at);
             COMMIT;",
        )
        .map_err(|e: sqlite::Error| format!("Failed to migrate TeraRelay Drive entries: {e}"))?;
    }

    // The streamed/sparse Drive writer adds an optional source entry for
    // copy-on-write edits. Existing unreleased QA databases predate this column.
    let mut pending_columns = conn
        .prepare("PRAGMA table_info(drive_pending_files)")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut has_base_entry_id = false;
    while let sqlite::State::Row = pending_columns
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
    {
        if pending_columns
            .read::<String, _>("name")
            .map_err(|e| e.to_string())?
            == "base_entry_id"
        {
            has_base_entry_id = true;
            break;
        }
    }
    drop(pending_columns);
    if !has_base_entry_id {
        conn.execute("ALTER TABLE drive_pending_files ADD COLUMN base_entry_id TEXT")
            .map_err(|e: sqlite::Error| {
                format!("Failed to migrate TeraRelay Drive pending writes: {e}")
            })?;
    }

    fn has_column(conn: &sqlite::Connection, table: &str, column: &str) -> Result<bool, String> {
        let sql = format!("PRAGMA table_info({table})");
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e: sqlite::Error| e.to_string())?;
        while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            if stmt.read::<String, _>("name").map_err(|e| e.to_string())? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    for (table, column, sql) in [
        (
            "drive_file_entries",
            "content_fingerprint",
            "ALTER TABLE drive_file_entries ADD COLUMN content_fingerprint TEXT",
        ),
        (
            "drive_pending_files",
            "format_version",
            "ALTER TABLE drive_pending_files ADD COLUMN format_version INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "drive_pending_files",
            "encryption_version",
            "ALTER TABLE drive_pending_files ADD COLUMN encryption_version INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "drive_pending_files",
            "crypto_id",
            "ALTER TABLE drive_pending_files ADD COLUMN crypto_id TEXT",
        ),
        (
            "drive_pending_files",
            "closed_at",
            "ALTER TABLE drive_pending_files ADD COLUMN closed_at INTEGER",
        ),
        (
            "drive_pending_chunks",
            "plaintext_sha256",
            "ALTER TABLE drive_pending_chunks ADD COLUMN plaintext_sha256 TEXT NOT NULL DEFAULT ''",
        ),
        (
            "drive_pending_parts",
            "backing_channel_id",
            "ALTER TABLE drive_pending_parts ADD COLUMN backing_channel_id INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "drive_objects",
            "block_hashes_b64",
            "ALTER TABLE drive_objects ADD COLUMN block_hashes_b64 TEXT",
        ),
    ] {
        if !has_column(conn, table, column)? {
            conn.execute(sql).map_err(|e: sqlite::Error| {
                format!("Failed to migrate TeraRelay Drive {table}.{column}: {e}")
            })?;
        }
    }

    crate::drive_crypto::init_crypto_schema(conn)?;
    Ok(())
}

fn state_row(conn: &sqlite::Connection) -> Result<(i64, bool), String> {
    let mut stmt = conn
        .prepare("SELECT revision, dirty FROM drive_state WHERE singleton = 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok((0, false));
    }
    let revision = stmt.read::<i64, _>(0).map_err(|e| e.to_string())?;
    let dirty = stmt.read::<i64, _>(1).map_err(|e| e.to_string())? != 0;
    Ok((revision, dirty))
}

pub fn mark_drive_dirty(conn: &sqlite::Connection) -> Result<i64, String> {
    let (current, _) = state_row(conn)?;
    let revision = std::cmp::max(current.saturating_add(1), now_ms());
    let mut stmt = conn
        .prepare("UPDATE drive_state SET revision = ?, dirty = 1 WHERE singleton = 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, revision))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(revision)
}

fn set_drive_clean(
    conn: &sqlite::Connection,
    revision: i64,
    remote_message_id: Option<i64>,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "UPDATE drive_state
             SET revision = ?, dirty = 0, last_remote_message_id = ?
             WHERE singleton = 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, revision))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, remote_message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

fn directory_name_exists(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    name: &str,
    ignore_dir_id: Option<&str>,
) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id FROM drive_directories
             WHERE ((parent_id IS NULL AND ? IS NULL) OR parent_id = ?)
               AND trashed_at IS NULL
               AND lower(name) = lower(?)
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(false);
    }
    let id = stmt.read::<String, _>(0).map_err(|e| e.to_string())?;
    Ok(ignore_dir_id != Some(id.as_str()))
}

fn file_name_exists(
    conn: &sqlite::Connection,
    directory_id: Option<&str>,
    name: &str,
    ignore_entry_id: Option<&str>,
    ignore_pending_id: Option<&str>,
) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entry_id FROM drive_file_entries
             WHERE ((directory_id IS NULL AND ? IS NULL) OR directory_id = ?)
               AND trashed_at IS NULL
               AND lower(display_name) = lower(?)
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        let id = stmt.read::<String, _>(0).map_err(|e| e.to_string())?;
        if ignore_entry_id != Some(id.as_str()) {
            return Ok(true);
        }
    }
    drop(stmt);

    let mut v2 = conn
        .prepare(
            "SELECT entry_id FROM drive_entries_v2
             WHERE ((directory_id IS NULL AND ? IS NULL) OR directory_id = ?)
               AND trashed_at IS NULL
               AND lower(display_name) = lower(?)
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    v2.bind((1, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    v2.bind((2, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    v2.bind((3, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        v2.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        let id = v2.read::<String, _>(0).map_err(|e| e.to_string())?;
        if ignore_entry_id != Some(id.as_str()) {
            return Ok(true);
        }
    }
    drop(v2);

    let mut pending = conn
        .prepare(
            "SELECT id FROM drive_pending_files
             WHERE ((directory_id IS NULL AND ? IS NULL) OR directory_id = ?)
               AND lower(display_name) = lower(?)
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((1, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((2, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((3, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        pending.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        let id = pending.read::<String, _>(0).map_err(|e| e.to_string())?;
        if ignore_pending_id != Some(id.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_name_available(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    name: &str,
    ignore_dir_id: Option<&str>,
    ignore_entry_id: Option<&str>,
    ignore_pending_id: Option<&str>,
) -> Result<(), String> {
    if directory_name_exists(conn, parent_id, name, ignore_dir_id)?
        || file_name_exists(conn, parent_id, name, ignore_entry_id, ignore_pending_id)?
    {
        return Err(format!("A drive item named \"{name}\" already exists"));
    }
    Ok(())
}

fn imported_directory_id(logical_channel_id: &str) -> String {
    format!("channel-{logical_channel_id}")
}

pub fn ensure_imported_layout(conn: &sqlite::Connection) -> Result<bool, String> {
    init_drive_schema(conn)?;
    let mut changed = false;
    let now = now_ms();

    let mut channels = conn
        .prepare(
            "SELECT logical_id, name
             FROM logical_channels
             WHERE role != 'drive_storage'
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;

    let mut channel_rows = Vec::new();
    while let sqlite::State::Row = channels.next().map_err(|e: sqlite::Error| e.to_string())? {
        channel_rows.push((
            channels
                .read::<String, _>("logical_id")
                .map_err(|e| e.to_string())?,
            channels
                .read::<String, _>("name")
                .map_err(|e| e.to_string())?,
        ));
    }
    drop(channels);

    for (logical_id, channel_name) in channel_rows {
        let dir_id = imported_directory_id(&logical_id);
        let mut check = conn
            .prepare("SELECT 1 FROM drive_directories WHERE id = ? LIMIT 1")
            .map_err(|e: sqlite::Error| e.to_string())?;
        check
            .bind((1, dir_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        let exists = matches!(
            check.next().map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        );
        drop(check);

        if !exists {
            let mut name = validate_drive_name(&channel_name)
                .unwrap_or_else(|_| "TeraRelay Storage".to_string());
            if directory_name_exists(conn, None, &name, None)? {
                name = format!("{name} {}", &logical_id[..8.min(logical_id.len())]);
            }
            let mut insert = conn
                .prepare(
                    "INSERT INTO drive_directories
                     (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
                     VALUES (?, NULL, ?, ?, ?, ?, NULL)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((1, dir_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((2, name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((3, logical_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((4, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((5, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert.next().map_err(|e: sqlite::Error| e.to_string())?;
            changed = true;
        }

        let mut files = conn
            .prepare(
                "SELECT file_id, display_name, created_at
                 FROM logical_files
                 WHERE logical_channel_id = ? AND status = 'complete'
                   AND display_name NOT LIKE '.terarelay-drive-file-%'",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        files
            .bind((1, logical_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        let mut unmapped = Vec::new();
        while let sqlite::State::Row = files.next().map_err(|e: sqlite::Error| e.to_string())? {
            unmapped.push((
                files
                    .read::<String, _>("file_id")
                    .map_err(|e| e.to_string())?,
                files
                    .read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?,
                files
                    .read::<i64, _>("created_at")
                    .map_err(|e| e.to_string())?,
            ));
        }
        drop(files);

        for (file_id, file_name, created_at) in unmapped {
            let mut check = conn
                .prepare("SELECT 1 FROM drive_file_entries WHERE file_id = ? LIMIT 1")
                .map_err(|e: sqlite::Error| e.to_string())?;
            check
                .bind((1, file_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            let mapped = matches!(
                check.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            );
            drop(check);
            if mapped {
                continue;
            }

            let mut name = validate_drive_name(&file_name).unwrap_or_else(|_| "file".to_string());
            if file_name_exists(conn, Some(&dir_id), &name, None, None)? {
                let ext = std::path::Path::new(&name)
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(str::to_string);
                let stem = std::path::Path::new(&name)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file");
                name = match ext {
                    Some(ext) => format!("{stem} ({}).{ext}", &file_id[..6.min(file_id.len())]),
                    None => format!("{stem} ({})", &file_id[..6.min(file_id.len())]),
                };
            }

            let entry_id = format!("import-{file_id}");
            let mut insert = conn
                .prepare(
                    "INSERT INTO drive_file_entries
                     (entry_id, file_id, directory_id, display_name, created_at, updated_at, trashed_at,
                      encryption_version, plaintext_size, crypto_id)
                     VALUES (?, ?, ?, ?, ?, ?, NULL, 0, NULL, NULL)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((1, entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((2, file_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((3, dir_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((4, name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((5, created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((6, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert.next().map_err(|e: sqlite::Error| e.to_string())?;
            changed = true;
        }
    }

    if changed {
        mark_drive_dirty(conn)?;
    }
    Ok(changed)
}

fn load_directories(conn: &sqlite::Connection) -> Result<Vec<DriveDirectoryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, parent_id, name, backing_logical_channel_id, created_at, updated_at
             FROM drive_directories
             WHERE trashed_at IS NULL
             ORDER BY name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveDirectoryRecord {
            id: stmt.read::<String, _>("id").map_err(|e| e.to_string())?,
            parent_id: stmt.read::<Option<String>, _>("parent_id").ok().flatten(),
            name: stmt.read::<String, _>("name").map_err(|e| e.to_string())?,
            backing_logical_channel_id: stmt
                .read::<Option<String>, _>("backing_logical_channel_id")
                .ok()
                .flatten(),
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            trashed_at: None,
        });
    }
    Ok(rows)
}

#[cfg(test)]
fn load_entries(conn: &sqlite::Connection) -> Result<Vec<DriveEntryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entry_id, file_id, directory_id, display_name, created_at, updated_at,
                    encryption_version, plaintext_size, crypto_id, content_fingerprint
             FROM drive_file_entries
             WHERE trashed_at IS NULL
             ORDER BY display_name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveEntryRecord {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            file_id: stmt
                .read::<String, _>("file_id")
                .map_err(|e| e.to_string())?,
            directory_id: stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
            plaintext_size: stmt
                .read::<Option<i64>, _>("plaintext_size")
                .ok()
                .flatten()
                .map(|value| value as u64),
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
            content_fingerprint: stmt
                .read::<Option<String>, _>("content_fingerprint")
                .ok()
                .flatten(),
            trashed_at: None,
        });
    }
    Ok(rows)
}

/// The sync manifest deliberately keeps tombstones. Snapshot/listing helpers
/// above expose only active items to the filesystem, while these helpers retain
/// deletions long enough for offline devices to converge safely.
fn load_manifest_directories(
    conn: &sqlite::Connection,
) -> Result<Vec<DriveDirectoryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, parent_id, name, backing_logical_channel_id,
                    created_at, updated_at, trashed_at
             FROM drive_directories
             ORDER BY id ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveDirectoryRecord {
            id: stmt.read::<String, _>("id").map_err(|e| e.to_string())?,
            parent_id: stmt.read::<Option<String>, _>("parent_id").ok().flatten(),
            name: stmt.read::<String, _>("name").map_err(|e| e.to_string())?,
            backing_logical_channel_id: stmt
                .read::<Option<String>, _>("backing_logical_channel_id")
                .ok()
                .flatten(),
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            trashed_at: stmt.read::<Option<i64>, _>("trashed_at").ok().flatten(),
        });
    }
    Ok(rows)
}

fn load_manifest_entries(conn: &sqlite::Connection) -> Result<Vec<DriveEntryRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entry_id, file_id, directory_id, display_name, created_at, updated_at,
                    encryption_version, plaintext_size, crypto_id, content_fingerprint, trashed_at
             FROM drive_file_entries
             ORDER BY entry_id ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveEntryRecord {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            file_id: stmt
                .read::<String, _>("file_id")
                .map_err(|e| e.to_string())?,
            directory_id: stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
            plaintext_size: stmt
                .read::<Option<i64>, _>("plaintext_size")
                .ok()
                .flatten()
                .map(|value| value as u64),
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
            content_fingerprint: stmt
                .read::<Option<String>, _>("content_fingerprint")
                .ok()
                .flatten(),
            trashed_at: stmt.read::<Option<i64>, _>("trashed_at").ok().flatten(),
        });
    }
    Ok(rows)
}

fn load_manifest_storage_channels(
    conn: &sqlite::Connection,
) -> Result<Vec<DriveStorageManifestRecord>, String> {
    crate::drive_storage::init_drive_storage_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT generation, backing_channel_id, state, title, created_at, updated_at,
                    retirement_reason
             FROM drive_storage_channels
             ORDER BY generation ASC, backing_channel_id ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveStorageManifestRecord {
            generation: stmt
                .read::<i64, _>("generation")
                .map_err(|e| e.to_string())?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            state: stmt.read::<String, _>("state").map_err(|e| e.to_string())?,
            title: stmt.read::<String, _>("title").map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            retirement_reason: stmt
                .read::<Option<String>, _>("retirement_reason")
                .ok()
                .flatten(),
        });
    }
    Ok(rows)
}

fn sha256_hex_bytes(value: &str) -> Result<[u8; 32], String> {
    if !valid_drive_hex(value, 64) {
        return Err("Drive block hash is invalid".to_string());
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| "Drive block hash is invalid".to_string())?;
    }
    Ok(bytes)
}

fn derive_block_hashes_b64(
    conn: &sqlite::Connection,
    object_id: &str,
    block_count: u64,
) -> Result<Option<String>, String> {
    let expected = usize::try_from(block_count)
        .map_err(|_| "Drive V2 block count is too large".to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT chunk_index, sha256
             FROM drive_object_blocks
             WHERE object_id = ?
             ORDER BY chunk_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, object_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut raw = Vec::with_capacity(expected.saturating_mul(32));
    let mut next_index = 0u64;
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let chunk_index = stmt
            .read::<i64, _>("chunk_index")
            .map_err(|e| e.to_string())? as u64;
        if chunk_index != next_index {
            return Err("Drive V2 block hash index is incomplete".to_string());
        }
        let hash = stmt
            .read::<String, _>("sha256")
            .map_err(|e| e.to_string())?;
        if hash.is_empty() {
            return Ok(None);
        }
        raw.extend_from_slice(&sha256_hex_bytes(&hash)?);
        next_index = next_index.saturating_add(1);
    }
    if next_index != block_count {
        return Err("Drive V2 object is missing block hashes".to_string());
    }
    Ok(Some(B64.encode(raw)))
}

fn decode_manifest_block_hashes(object: &DriveObjectRecord) -> Result<Vec<String>, String> {
    let encoded = object
        .block_hashes_b64
        .as_deref()
        .ok_or_else(|| "Drive V2 object is missing compact block hashes".to_string())?;
    let raw = B64
        .decode(encoded)
        .map_err(|_| "Drive V2 compact block hashes are invalid".to_string())?;
    let expected_len = object
        .block_count
        .checked_mul(32)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "Drive V2 compact block hash size overflow".to_string())?;
    if raw.len() != expected_len {
        return Err("Drive V2 compact block hash count is invalid".to_string());
    }
    Ok(raw
        .chunks_exact(32)
        .map(|hash| hash.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect())
}

fn load_manifest_objects(conn: &sqlite::Connection) -> Result<Vec<DriveObjectRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT object_id, plaintext_size, encryption_version, crypto_id,
                    content_fingerprint, block_count, block_hashes_b64, created_at
             FROM drive_objects
             ORDER BY object_id ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let object_id = stmt
            .read::<String, _>("object_id")
            .map_err(|e| e.to_string())?;
        let block_count = stmt
            .read::<i64, _>("block_count")
            .map_err(|e| e.to_string())? as u64;
        let persisted_hashes = stmt
            .read::<Option<String>, _>("block_hashes_b64")
            .ok()
            .flatten()
            .filter(|value| !value.is_empty());
        let block_hashes_b64 = match persisted_hashes {
            Some(value) => Some(value),
            None => derive_block_hashes_b64(conn, &object_id, block_count)?,
        };
        rows.push(DriveObjectRecord {
            object_id,
            plaintext_size: stmt
                .read::<i64, _>("plaintext_size")
                .map_err(|e| e.to_string())? as u64,
            encryption_version: stmt
                .read::<i64, _>("encryption_version")
                .map_err(|e| e.to_string())? as u32,
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
            content_fingerprint: stmt
                .read::<String, _>("content_fingerprint")
                .map_err(|e| e.to_string())?,
            block_count,
            block_hashes_b64,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(rows)
}

fn load_manifest_parts(conn: &sqlite::Connection) -> Result<Vec<DriveObjectPartRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT object_id, part_index, first_chunk_index, chunk_count,
                    backing_channel_id, message_id, ciphertext_size, sha256
             FROM drive_object_parts
             ORDER BY object_id ASC, part_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveObjectPartRecord {
            object_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            part_index: stmt
                .read::<i64, _>("part_index")
                .map_err(|e| e.to_string())? as u64,
            first_chunk_index: stmt
                .read::<i64, _>("first_chunk_index")
                .map_err(|e| e.to_string())? as u64,
            chunk_count: stmt
                .read::<i64, _>("chunk_count")
                .map_err(|e| e.to_string())? as u64,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            ciphertext_size: stmt
                .read::<i64, _>("ciphertext_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: stmt
                .read::<String, _>("sha256")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(rows)
}

fn load_manifest_v2_entries(conn: &sqlite::Connection) -> Result<Vec<DriveEntryV2Record>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entry_id, object_id, directory_id, display_name,
                    created_at, updated_at, trashed_at
             FROM drive_entries_v2
             ORDER BY entry_id ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut rows = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        rows.push(DriveEntryV2Record {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            object_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            directory_id: stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            trashed_at: stmt.read::<Option<i64>, _>("trashed_at").ok().flatten(),
        });
    }
    Ok(rows)
}

fn load_v2_files(conn: &sqlite::Connection) -> Result<Vec<DriveFileRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.object_id, e.directory_id, e.display_name,
                    e.created_at, e.updated_at, o.plaintext_size, o.encryption_version,
                    o.crypto_id, o.content_fingerprint,
                    COALESCE(p.backing_channel_id, 0) AS backing_channel_id,
                    COALESCE(p.message_id, 0) AS first_message_id
             FROM drive_entries_v2 e
             JOIN drive_objects o ON o.object_id = e.object_id
             LEFT JOIN drive_object_parts p
               ON p.object_id = e.object_id AND p.part_index = 0
             WHERE e.trashed_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM drive_pending_files pending
                   WHERE pending.base_entry_id = e.entry_id
               )
             ORDER BY e.display_name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut files = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let display_name = stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?;
        let plaintext_size = stmt
            .read::<i64, _>("plaintext_size")
            .map_err(|e| e.to_string())? as u64;
        files.push(DriveFileRecord {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            file_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            directory_id: stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            mime_type: mime_guess::from_path(&display_name)
                .first()
                .map(|mime| mime.essence_str().to_string()),
            display_name,
            total_size: plaintext_size,
            first_message_id: stmt
                .read::<i64, _>("first_message_id")
                .map_err(|e| e.to_string())?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            logical_channel_id: String::new(),
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
            plaintext_size: Some(plaintext_size),
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
            content_fingerprint: stmt
                .read::<Option<String>, _>("content_fingerprint")
                .ok()
                .flatten(),
            storage_version: 2,
        });
    }
    Ok(files)
}

fn drive_v2_file_by_entry(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<Option<DriveFileRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.object_id, e.directory_id, e.display_name,
                    e.created_at, e.updated_at, o.plaintext_size, o.encryption_version,
                    o.crypto_id, o.content_fingerprint,
                    COALESCE(p.backing_channel_id, 0) AS backing_channel_id,
                    COALESCE(p.message_id, 0) AS first_message_id
             FROM drive_entries_v2 e
             JOIN drive_objects o ON o.object_id = e.object_id
             LEFT JOIN drive_object_parts p
               ON p.object_id = e.object_id AND p.part_index = 0
             WHERE e.entry_id = ? AND e.trashed_at IS NULL
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    let display_name = stmt
        .read::<String, _>("display_name")
        .map_err(|e| e.to_string())?;
    let plaintext_size = stmt
        .read::<i64, _>("plaintext_size")
        .map_err(|e| e.to_string())? as u64;
    Ok(Some(DriveFileRecord {
        entry_id: stmt
            .read::<String, _>("entry_id")
            .map_err(|e| e.to_string())?,
        file_id: stmt
            .read::<String, _>("object_id")
            .map_err(|e| e.to_string())?,
        directory_id: stmt
            .read::<Option<String>, _>("directory_id")
            .ok()
            .flatten(),
        mime_type: mime_guess::from_path(&display_name)
            .first()
            .map(|mime| mime.essence_str().to_string()),
        display_name,
        total_size: plaintext_size,
        first_message_id: stmt
            .read::<i64, _>("first_message_id")
            .map_err(|e| e.to_string())?,
        backing_channel_id: stmt
            .read::<i64, _>("backing_channel_id")
            .map_err(|e| e.to_string())?,
        logical_channel_id: String::new(),
        created_at: stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?,
        updated_at: stmt
            .read::<i64, _>("updated_at")
            .map_err(|e| e.to_string())?,
        encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
        plaintext_size: Some(plaintext_size),
        crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
        content_fingerprint: stmt
            .read::<Option<String>, _>("content_fingerprint")
            .ok()
            .flatten(),
        storage_version: 2,
    }))
}

pub fn drive_snapshot(conn: &sqlite::Connection) -> Result<DriveSnapshot, String> {
    init_drive_schema(conn)?;
    let (revision, dirty) = state_row(conn)?;
    let directories = load_directories(conn)?;

    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.file_id, e.directory_id, e.display_name,
                    f.total_size, f.mime_type, f.first_message_id,
                    c.backing_channel_id, f.logical_channel_id, f.created_at,
                    e.updated_at, e.encryption_version, e.plaintext_size, e.crypto_id,
                    e.content_fingerprint
             FROM drive_file_entries e
             JOIN logical_files f ON f.file_id = e.file_id
             JOIN logical_channels c ON c.logical_id = f.logical_channel_id
             WHERE e.trashed_at IS NULL AND f.status = 'complete'
               AND NOT EXISTS (
                   SELECT 1 FROM drive_pending_files p
                   WHERE p.base_entry_id = e.entry_id
               )
             ORDER BY e.display_name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut files = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        files.push(DriveFileRecord {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            file_id: stmt
                .read::<String, _>("file_id")
                .map_err(|e| e.to_string())?,
            directory_id: stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            total_size: stmt
                .read::<i64, _>("total_size")
                .map_err(|e| e.to_string())? as u64,
            mime_type: stmt.read::<Option<String>, _>("mime_type").ok().flatten(),
            first_message_id: stmt
                .read::<i64, _>("first_message_id")
                .map_err(|e| e.to_string())?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            logical_channel_id: stmt
                .read::<String, _>("logical_channel_id")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
            plaintext_size: stmt
                .read::<Option<i64>, _>("plaintext_size")
                .ok()
                .flatten()
                .map(|value| value as u64),
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
            content_fingerprint: stmt
                .read::<Option<String>, _>("content_fingerprint")
                .ok()
                .flatten(),
            storage_version: 1,
        });
    }
    drop(stmt);

    files.extend(load_v2_files(conn)?);
    files.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });

    let mut pending_stmt = conn
        .prepare(
            "SELECT id, directory_id, display_name, staging_path, size, format_version,
                    backing_channel_id, base_entry_id, encryption_version, crypto_id,
                    closed_at, created_at, updated_at
             FROM drive_pending_files
             ORDER BY display_name COLLATE NOCASE ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut pending = Vec::new();
    while let sqlite::State::Row = pending_stmt
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
    {
        pending.push(DrivePendingRecord {
            id: pending_stmt
                .read::<String, _>("id")
                .map_err(|e| e.to_string())?,
            directory_id: pending_stmt
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            display_name: pending_stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            staging_path: pending_stmt
                .read::<String, _>("staging_path")
                .map_err(|e| e.to_string())?,
            size: pending_stmt
                .read::<i64, _>("size")
                .map_err(|e| e.to_string())? as u64,
            format_version: pending_stmt.read::<i64, _>("format_version").unwrap_or(1) as u32,
            backing_channel_id: pending_stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            base_entry_id: pending_stmt
                .read::<Option<String>, _>("base_entry_id")
                .ok()
                .flatten(),
            encryption_version: pending_stmt
                .read::<i64, _>("encryption_version")
                .unwrap_or(0) as u32,
            crypto_id: pending_stmt
                .read::<Option<String>, _>("crypto_id")
                .ok()
                .flatten(),
            closed_at: pending_stmt
                .read::<Option<i64>, _>("closed_at")
                .ok()
                .flatten(),
            created_at: pending_stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: pending_stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
        });
    }

    Ok(DriveSnapshot {
        revision,
        dirty,
        directories,
        files,
        pending,
    })
}

pub fn drive_file_by_entry(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<Option<DriveFileRecord>, String> {
    init_drive_schema(conn)?;
    if let Some(file) = drive_v2_file_by_entry(conn, entry_id)? {
        return Ok(Some(file));
    }
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.file_id, e.directory_id, e.display_name,
                    f.total_size, f.mime_type, f.first_message_id,
                    c.backing_channel_id, f.logical_channel_id, f.created_at,
                    e.updated_at, e.encryption_version, e.plaintext_size, e.crypto_id,
                    e.content_fingerprint
             FROM drive_file_entries e
             JOIN logical_files f ON f.file_id = e.file_id
             JOIN logical_channels c ON c.logical_id = f.logical_channel_id
             WHERE e.entry_id = ? AND e.trashed_at IS NULL AND f.status = 'complete'
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    Ok(Some(DriveFileRecord {
        entry_id: stmt
            .read::<String, _>("entry_id")
            .map_err(|e| e.to_string())?,
        file_id: stmt
            .read::<String, _>("file_id")
            .map_err(|e| e.to_string())?,
        directory_id: stmt
            .read::<Option<String>, _>("directory_id")
            .ok()
            .flatten(),
        display_name: stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?,
        total_size: stmt
            .read::<i64, _>("total_size")
            .map_err(|e| e.to_string())? as u64,
        mime_type: stmt.read::<Option<String>, _>("mime_type").ok().flatten(),
        first_message_id: stmt
            .read::<i64, _>("first_message_id")
            .map_err(|e| e.to_string())?,
        backing_channel_id: stmt
            .read::<i64, _>("backing_channel_id")
            .map_err(|e| e.to_string())?,
        logical_channel_id: stmt
            .read::<String, _>("logical_channel_id")
            .map_err(|e| e.to_string())?,
        created_at: stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?,
        updated_at: stmt
            .read::<i64, _>("updated_at")
            .map_err(|e| e.to_string())?,
        encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
        plaintext_size: stmt
            .read::<Option<i64>, _>("plaintext_size")
            .ok()
            .flatten()
            .map(|value| value as u64),
        crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
        content_fingerprint: stmt
            .read::<Option<String>, _>("content_fingerprint")
            .ok()
            .flatten(),
        storage_version: 1,
    }))
}

#[tauri::command]
pub fn cmd_drive_get_snapshot(db_pool: State<'_, DbConnection>) -> Result<DriveSnapshot, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    drive_snapshot(&conn)
}

pub fn drive_file_by_entry_id(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<Option<DriveFileRecord>, String> {
    init_drive_schema(conn)?;
    if let Some(file) = drive_v2_file_by_entry(conn, entry_id)? {
        return Ok(Some(file));
    }
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.file_id, e.directory_id, e.display_name,
                    f.total_size, f.mime_type, f.first_message_id,
                    c.backing_channel_id, f.logical_channel_id, f.created_at,
                    e.updated_at, e.encryption_version, e.plaintext_size, e.crypto_id,
                    e.content_fingerprint
             FROM drive_file_entries e
             JOIN logical_files f ON f.file_id = e.file_id
             JOIN logical_channels c ON c.logical_id = f.logical_channel_id
             WHERE e.entry_id = ? AND e.trashed_at IS NULL AND f.status = 'complete'
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    Ok(Some(DriveFileRecord {
        entry_id: stmt
            .read::<String, _>("entry_id")
            .map_err(|e| e.to_string())?,
        file_id: stmt
            .read::<String, _>("file_id")
            .map_err(|e| e.to_string())?,
        directory_id: stmt
            .read::<Option<String>, _>("directory_id")
            .ok()
            .flatten(),
        display_name: stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?,
        total_size: stmt
            .read::<i64, _>("total_size")
            .map_err(|e| e.to_string())? as u64,
        mime_type: stmt.read::<Option<String>, _>("mime_type").ok().flatten(),
        first_message_id: stmt
            .read::<i64, _>("first_message_id")
            .map_err(|e| e.to_string())?,
        backing_channel_id: stmt
            .read::<i64, _>("backing_channel_id")
            .map_err(|e| e.to_string())?,
        logical_channel_id: stmt
            .read::<String, _>("logical_channel_id")
            .map_err(|e| e.to_string())?,
        created_at: stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?,
        updated_at: stmt
            .read::<i64, _>("updated_at")
            .map_err(|e| e.to_string())?,
        encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
        plaintext_size: stmt
            .read::<Option<i64>, _>("plaintext_size")
            .ok()
            .flatten()
            .map(|value| value as u64),
        crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
        content_fingerprint: stmt
            .read::<Option<String>, _>("content_fingerprint")
            .ok()
            .flatten(),
        storage_version: 1,
    }))
}

pub fn create_drive_directory(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    name: &str,
) -> Result<DriveDirectoryRecord, String> {
    init_drive_schema(conn)?;
    let name = validate_drive_name(name)?;
    ensure_name_available(conn, parent_id, &name, None, None, None)?;
    // Drive folders are purely virtual metadata. New folders never inherit or
    // create a Telegram backing channel; storage is assigned only when bytes
    // are first uploaded through the dedicated Drive storage pool.
    let backing: Option<String> = None;
    let id = new_drive_id();
    let now = now_ms();
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_directories
             (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES (?, ?, ?, ?, ?, ?, NULL)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, backing.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(DriveDirectoryRecord {
        id,
        parent_id: parent_id.map(str::to_string),
        name,
        backing_logical_channel_id: backing,
        created_at: now,
        updated_at: now,
        trashed_at: None,
    })
}

pub fn create_drive_pending_file(
    conn: &sqlite::Connection,
    pending_id: &str,
    directory_id: Option<&str>,
    name: &str,
    staging_path: &str,
) -> Result<DrivePendingRecord, String> {
    init_drive_schema(conn)?;
    let name = validate_drive_name(name)?;
    ensure_name_available(conn, directory_id, &name, None, None, None)?;
    // Zero means "not assigned yet". The async upload path provisions/reuses
    // the dedicated Drive storage channel and atomically assigns it before the
    // first remote write. This keeps user-created TeraRelay channels isolated.
    let backing_channel_id = 0i64;
    if pending_id.is_empty() {
        return Err("Drive pending file ID cannot be empty".to_string());
    }
    let id = pending_id.to_string();
    let encryption_version = if crate::drive_crypto::load_crypto_envelope(conn)?.is_some() {
        crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
    } else {
        0
    };
    let crypto_id = if encryption_version != 0 {
        Some(crate::drive_crypto::new_crypto_id())
    } else {
        None
    };
    let now = now_ms();
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_pending_files
             (id, directory_id, display_name, staging_path, size, format_version,
              backing_channel_id, base_entry_id, encryption_version, crypto_id,
              closed_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, 0, 2, ?, NULL, ?, ?, NULL, ?, ?)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, staging_path))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, encryption_version as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, crypto_id.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((8, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((9, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(DrivePendingRecord {
        id,
        directory_id: directory_id.map(str::to_string),
        display_name: name,
        staging_path: staging_path.to_string(),
        size: 0,
        format_version: 2,
        backing_channel_id,
        base_entry_id: None,
        encryption_version,
        crypto_id,
        closed_at: None,
        created_at: now,
        updated_at: now,
    })
}

pub fn drive_pending_by_id(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Option<DrivePendingRecord>, String> {
    init_drive_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, directory_id, display_name, staging_path, size, format_version,
                    backing_channel_id, base_entry_id, encryption_version, crypto_id,
                    closed_at, created_at, updated_at
             FROM drive_pending_files
             WHERE id = ?
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    Ok(Some(DrivePendingRecord {
        id: stmt.read::<String, _>("id").map_err(|e| e.to_string())?,
        directory_id: stmt
            .read::<Option<String>, _>("directory_id")
            .ok()
            .flatten(),
        display_name: stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?,
        staging_path: stmt
            .read::<String, _>("staging_path")
            .map_err(|e| e.to_string())?,
        size: stmt.read::<i64, _>("size").map_err(|e| e.to_string())? as u64,
        format_version: stmt.read::<i64, _>("format_version").unwrap_or(1) as u32,
        backing_channel_id: stmt
            .read::<i64, _>("backing_channel_id")
            .map_err(|e| e.to_string())?,
        base_entry_id: stmt
            .read::<Option<String>, _>("base_entry_id")
            .ok()
            .flatten(),
        encryption_version: stmt.read::<i64, _>("encryption_version").unwrap_or(0) as u32,
        crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
        closed_at: stmt.read::<Option<i64>, _>("closed_at").ok().flatten(),
        created_at: stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?,
        updated_at: stmt
            .read::<i64, _>("updated_at")
            .map_err(|e| e.to_string())?,
    }))
}

pub fn assign_drive_pending_backing_channel(
    conn: &sqlite::Connection,
    pending_id: &str,
    backing_channel_id: i64,
) -> Result<DrivePendingRecord, String> {
    if backing_channel_id <= 0 {
        return Err("Drive storage channel ID is invalid".to_string());
    }
    let current = drive_pending_by_id(conn, pending_id)?
        .ok_or_else(|| "Drive pending write no longer exists".to_string())?;
    if current.backing_channel_id == backing_channel_id {
        return Ok(current);
    }
    if current.backing_channel_id != 0 {
        return Err("Drive pending write is already bound to another storage channel".to_string());
    }

    let mut stmt = conn
        .prepare(
            "UPDATE drive_pending_files
             SET backing_channel_id = ?, updated_at = ?
             WHERE id = ? AND backing_channel_id = 0",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;

    let assigned = drive_pending_by_id(conn, pending_id)?
        .ok_or_else(|| "Drive pending write disappeared while assigning storage".to_string())?;
    if assigned.backing_channel_id != backing_channel_id {
        return Err("Drive pending storage assignment did not persist".to_string());
    }
    Ok(assigned)
}

pub fn mark_drive_pending_closed(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<i64, String> {
    let closed_at = now_ms();
    let mut stmt = conn
        .prepare(
            "UPDATE drive_pending_files
             SET closed_at = COALESCE(closed_at, ?), updated_at = ?
             WHERE id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, closed_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, closed_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(closed_at)
}

pub fn update_drive_pending_size(
    conn: &sqlite::Connection,
    pending_id: &str,
    size: u64,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "UPDATE drive_pending_files
             SET size = ?, updated_at = ?
             WHERE id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn create_drive_pending_replacement(
    conn: &sqlite::Connection,
    pending_id: &str,
    base_entry_id: &str,
    staging_path: &str,
) -> Result<DrivePendingRecord, String> {
    init_drive_schema(conn)?;
    let (directory_id, display_name, size) =
        if let Some(file) = drive_v2_file_by_entry(conn, base_entry_id)? {
            (
                file.directory_id,
                file.display_name,
                file.plaintext_size.unwrap_or(file.total_size),
            )
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT e.directory_id, e.display_name,
                        COALESCE(e.plaintext_size, f.total_size) AS visible_size
                 FROM drive_file_entries e
                 JOIN logical_files f ON f.file_id = e.file_id
                 JOIN logical_channels c ON c.logical_id = f.logical_channel_id
                 WHERE e.entry_id = ? AND e.trashed_at IS NULL AND f.status = 'complete'
                 LIMIT 1",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, base_entry_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if !matches!(
                stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            ) {
                return Err("Drive source file is no longer available".to_string());
            }
            (
                stmt.read::<Option<String>, _>("directory_id")
                    .ok()
                    .flatten(),
                stmt.read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?,
                stmt.read::<i64, _>("visible_size")
                    .map_err(|e| e.to_string())? as u64,
            )
        };
    // Copy-on-write edits are finalized into the dedicated Drive storage pool,
    // never back into the immutable source object's/user channel.
    let backing_channel_id = 0i64;

    // Only one active copy-on-write editor per Drive entry. Multiple ordinary
    // readers remain fine and continue to stream the immutable remote version.
    let mut existing = conn
        .prepare(
            "SELECT id FROM drive_pending_files
             WHERE base_entry_id = ?
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    existing
        .bind((1, base_entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        existing.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("This Drive file is already open for editing".to_string());
    }
    drop(existing);

    let encryption_version = if crate::drive_crypto::load_crypto_envelope(conn)?.is_some() {
        crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
    } else {
        0
    };
    let crypto_id = if encryption_version != 0 {
        Some(crate::drive_crypto::new_crypto_id())
    } else {
        None
    };
    let now = now_ms();
    let mut insert = conn
        .prepare(
            "INSERT INTO drive_pending_files
             (id, directory_id, display_name, staging_path, size, format_version,
              backing_channel_id, base_entry_id, encryption_version, crypto_id,
              closed_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 2, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((2, directory_id.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((3, display_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((4, staging_path))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((5, size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((6, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((7, base_entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((8, encryption_version as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((9, crypto_id.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((10, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((11, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert.next().map_err(|e: sqlite::Error| e.to_string())?;

    Ok(DrivePendingRecord {
        id: pending_id.to_string(),
        directory_id,
        display_name,
        staging_path: staging_path.to_string(),
        size,
        format_version: 2,
        backing_channel_id,
        base_entry_id: Some(base_entry_id.to_string()),
        encryption_version,
        crypto_id,
        closed_at: None,
        created_at: now,
        updated_at: now,
    })
}

pub fn record_drive_pending_range(
    conn: &sqlite::Connection,
    pending_id: &str,
    start: u64,
    end: u64,
) -> Result<(), String> {
    if end <= start {
        return Ok(());
    }
    if end > i64::MAX as u64 {
        return Err("Drive write range is too large".to_string());
    }
    let mut stmt = conn
        .prepare(
            "SELECT range_start, range_end
             FROM drive_pending_ranges
             WHERE pending_id = ?
             ORDER BY range_start ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut ranges = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        ranges.push((
            stmt.read::<i64, _>("range_start")
                .map_err(|e| e.to_string())? as u64,
            stmt.read::<i64, _>("range_end")
                .map_err(|e| e.to_string())? as u64,
        ));
    }
    drop(stmt);
    ranges.push((start, end));
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (range_start, range_end) in ranges {
        if let Some(last) = merged.last_mut() {
            if range_start <= last.1 {
                last.1 = last.1.max(range_end);
                continue;
            }
        }
        merged.push((range_start, range_end));
    }

    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        let mut delete = conn
            .prepare("DELETE FROM drive_pending_ranges WHERE pending_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
        drop(delete);
        for (range_start, range_end) in &merged {
            let mut insert = conn
                .prepare(
                    "INSERT INTO drive_pending_ranges
                     (pending_id, range_start, range_end)
                     VALUES (?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((1, pending_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((2, *range_start as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((3, *range_end as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert.next().map_err(|e: sqlite::Error| e.to_string())?;
        }
        Ok::<(), String>(())
    })();
    match result {
        Ok(()) => conn
            .execute("COMMIT")
            .map_err(|e: sqlite::Error| e.to_string()),
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            Err(error)
        }
    }
}

pub fn drive_pending_ranges(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Vec<(u64, u64)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT range_start, range_end
             FROM drive_pending_ranges
             WHERE pending_id = ?
             ORDER BY range_start ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut ranges = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        ranges.push((
            stmt.read::<i64, _>("range_start")
                .map_err(|e| e.to_string())? as u64,
            stmt.read::<i64, _>("range_end")
                .map_err(|e| e.to_string())? as u64,
        ));
    }
    Ok(ranges)
}

pub fn clear_drive_pending_ranges_for_span(
    conn: &sqlite::Connection,
    pending_id: &str,
    span_start: u64,
    span_end: u64,
) -> Result<(), String> {
    let ranges = drive_pending_ranges(conn, pending_id)?;
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        let mut delete = conn
            .prepare("DELETE FROM drive_pending_ranges WHERE pending_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
        drop(delete);
        for (start, end) in ranges {
            let pieces = [(start, end.min(span_start)), (start.max(span_end), end)];
            for (piece_start, piece_end) in pieces {
                if piece_end <= piece_start {
                    continue;
                }
                let mut insert = conn
                    .prepare(
                        "INSERT INTO drive_pending_ranges
                         (pending_id, range_start, range_end)
                         VALUES (?, ?, ?)",
                    )
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((1, pending_id))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((2, piece_start as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((3, piece_end as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert.next().map_err(|e: sqlite::Error| e.to_string())?;
            }
        }
        Ok::<(), String>(())
    })();
    match result {
        Ok(()) => conn
            .execute("COMMIT")
            .map_err(|e: sqlite::Error| e.to_string()),
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            Err(error)
        }
    }
}

pub fn upsert_drive_pending_chunk(
    conn: &sqlite::Connection,
    pending_id: &str,
    chunk_index: u64,
    message_id: i64,
    chunk_size: u64,
    sha256: &str,
    plaintext_sha256: &str,
) -> Result<(), String> {
    if chunk_index > i64::MAX as u64 || chunk_size > i64::MAX as u64 {
        return Err("Drive chunk metadata is too large".to_string());
    }
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_pending_chunks
             (pending_id, chunk_index, message_id, chunk_size, sha256, plaintext_sha256, uploaded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(pending_id, chunk_index) DO UPDATE SET
                message_id = excluded.message_id,
                chunk_size = excluded.chunk_size,
                sha256 = excluded.sha256,
                plaintext_sha256 = excluded.plaintext_sha256,
                uploaded_at = excluded.uploaded_at",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, chunk_index as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, chunk_size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, sha256))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, plaintext_sha256))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn drive_pending_chunks(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Vec<DrivePendingChunkRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT pending_id, chunk_index, message_id, chunk_size, sha256, plaintext_sha256, uploaded_at
             FROM drive_pending_chunks
             WHERE pending_id = ?
             ORDER BY chunk_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut chunks = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        chunks.push(DrivePendingChunkRecord {
            pending_id: stmt
                .read::<String, _>("pending_id")
                .map_err(|e| e.to_string())?,
            chunk_index: stmt
                .read::<i64, _>("chunk_index")
                .map_err(|e| e.to_string())? as u64,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            chunk_size: stmt
                .read::<i64, _>("chunk_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: stmt
                .read::<String, _>("sha256")
                .map_err(|e| e.to_string())?,
            plaintext_sha256: stmt
                .read::<String, _>("plaintext_sha256")
                .map_err(|e| e.to_string())?,
            uploaded_at: stmt
                .read::<i64, _>("uploaded_at")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(chunks)
}

pub fn upsert_drive_pending_part(
    conn: &sqlite::Connection,
    pending_id: &str,
    part_index: u64,
    first_chunk_index: u64,
    chunk_count: u64,
    backing_channel_id: i64,
    message_id: i64,
    part_size: u64,
    sha256: &str,
) -> Result<(), String> {
    if part_index > i64::MAX as u64
        || first_chunk_index > i64::MAX as u64
        || chunk_count == 0
        || chunk_count > i64::MAX as u64
        || backing_channel_id <= 0
        || part_size == 0
        || part_size > i64::MAX as u64
        || message_id <= 0
    {
        return Err("Drive remote part metadata is invalid".to_string());
    }
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_pending_parts
             (pending_id, part_index, first_chunk_index, chunk_count, backing_channel_id,
              message_id, part_size, sha256, uploaded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(pending_id, part_index) DO UPDATE SET
                first_chunk_index = excluded.first_chunk_index,
                chunk_count = excluded.chunk_count,
                backing_channel_id = excluded.backing_channel_id,
                message_id = excluded.message_id,
                part_size = excluded.part_size,
                sha256 = excluded.sha256,
                uploaded_at = excluded.uploaded_at",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, part_index as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, first_chunk_index as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, chunk_count as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, part_size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((8, sha256))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((9, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn drive_pending_parts(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Vec<DrivePendingPartRecord>, String> {
    init_drive_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT pending_id, part_index, first_chunk_index, chunk_count, backing_channel_id,
                    message_id, part_size, sha256, uploaded_at
             FROM drive_pending_parts
             WHERE pending_id = ?
             ORDER BY part_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut parts = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        parts.push(DrivePendingPartRecord {
            pending_id: stmt
                .read::<String, _>("pending_id")
                .map_err(|e| e.to_string())?,
            part_index: stmt
                .read::<i64, _>("part_index")
                .map_err(|e| e.to_string())? as u64,
            first_chunk_index: stmt
                .read::<i64, _>("first_chunk_index")
                .map_err(|e| e.to_string())? as u64,
            chunk_count: stmt
                .read::<i64, _>("chunk_count")
                .map_err(|e| e.to_string())? as u64,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            part_size: stmt
                .read::<i64, _>("part_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: stmt
                .read::<String, _>("sha256")
                .map_err(|e| e.to_string())?,
            uploaded_at: stmt
                .read::<i64, _>("uploaded_at")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(parts)
}

pub fn drive_object_parts(
    conn: &sqlite::Connection,
    object_id: &str,
) -> Result<Vec<DriveObjectPartRecord>, String> {
    init_drive_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT object_id, part_index, first_chunk_index, chunk_count,
                    backing_channel_id, message_id, ciphertext_size, sha256
             FROM drive_object_parts
             WHERE object_id = ?
             ORDER BY part_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, object_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut parts = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        parts.push(DriveObjectPartRecord {
            object_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            part_index: stmt
                .read::<i64, _>("part_index")
                .map_err(|e| e.to_string())? as u64,
            first_chunk_index: stmt
                .read::<i64, _>("first_chunk_index")
                .map_err(|e| e.to_string())? as u64,
            chunk_count: stmt
                .read::<i64, _>("chunk_count")
                .map_err(|e| e.to_string())? as u64,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            ciphertext_size: stmt
                .read::<i64, _>("ciphertext_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: stmt
                .read::<String, _>("sha256")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(parts)
}

pub fn drive_object_blocks(
    conn: &sqlite::Connection,
    object_id: &str,
) -> Result<Vec<DriveObjectBlockRecord>, String> {
    init_drive_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT object_id, chunk_index, part_index, part_offset,
                    ciphertext_size, sha256, plaintext_sha256
             FROM drive_object_blocks
             WHERE object_id = ?
             ORDER BY chunk_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, object_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut blocks = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        blocks.push(DriveObjectBlockRecord {
            object_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            chunk_index: stmt
                .read::<i64, _>("chunk_index")
                .map_err(|e| e.to_string())? as u64,
            part_index: stmt
                .read::<i64, _>("part_index")
                .map_err(|e| e.to_string())? as u64,
            part_offset: stmt
                .read::<i64, _>("part_offset")
                .map_err(|e| e.to_string())? as u64,
            ciphertext_size: stmt
                .read::<i64, _>("ciphertext_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: stmt
                .read::<String, _>("sha256")
                .map_err(|e| e.to_string())?,
            plaintext_sha256: stmt
                .read::<String, _>("plaintext_sha256")
                .map_err(|e| e.to_string())?,
        });
    }
    Ok(blocks)
}

pub fn drive_object_block_location(
    conn: &sqlite::Connection,
    object_id: &str,
    chunk_index: u64,
) -> Result<Option<(DriveObjectBlockRecord, DriveObjectPartRecord)>, String> {
    let block = drive_object_blocks(conn, object_id)?
        .into_iter()
        .find(|block| block.chunk_index == chunk_index);
    let Some(block) = block else {
        return Ok(None);
    };
    let part = drive_object_parts(conn, object_id)?
        .into_iter()
        .find(|part| part.part_index == block.part_index)
        .ok_or_else(|| "Drive V2 block references a missing remote part".to_string())?;
    Ok(Some((block, part)))
}

pub fn take_drive_pending_parts_from_chunk(
    conn: &sqlite::Connection,
    pending_id: &str,
    from_chunk_index: u64,
) -> Result<Vec<DrivePendingPartRecord>, String> {
    let removed: Vec<_> = drive_pending_parts(conn, pending_id)?
        .into_iter()
        .filter(|part| part.first_chunk_index.saturating_add(part.chunk_count) > from_chunk_index)
        .collect();
    for part in &removed {
        let mut delete = conn
            .prepare(
                "DELETE FROM drive_pending_parts
                 WHERE pending_id = ? AND part_index = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((2, part.part_index as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(removed)
}

pub fn take_drive_pending_parts_starting_at_chunk(
    conn: &sqlite::Connection,
    pending_id: &str,
    from_chunk_index: u64,
) -> Result<Vec<DrivePendingPartRecord>, String> {
    let removed: Vec<_> = drive_pending_parts(conn, pending_id)?
        .into_iter()
        .filter(|part| part.first_chunk_index >= from_chunk_index)
        .collect();
    for part in &removed {
        let mut delete = conn
            .prepare(
                "DELETE FROM drive_pending_parts
                 WHERE pending_id = ? AND part_index = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((2, part.part_index as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(removed)
}

pub fn take_drive_pending_chunks_from(
    conn: &sqlite::Connection,
    pending_id: &str,
    from_chunk_index: u64,
) -> Result<Vec<DrivePendingChunkRecord>, String> {
    let removed: Vec<_> = drive_pending_chunks(conn, pending_id)?
        .into_iter()
        .filter(|chunk| chunk.chunk_index >= from_chunk_index)
        .collect();
    if removed.is_empty() {
        return Ok(removed);
    }
    let mut delete = conn
        .prepare(
            "DELETE FROM drive_pending_chunks
             WHERE pending_id = ? AND chunk_index >= ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((2, from_chunk_index as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(removed)
}

pub fn delete_drive_pending_state(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<(), String> {
    for sql in [
        "DELETE FROM drive_pending_ranges WHERE pending_id = ?",
        "DELETE FROM drive_pending_chunks WHERE pending_id = ?",
        "DELETE FROM drive_pending_parts WHERE pending_id = ?",
    ] {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(())
}

pub fn rename_drive_directory(
    conn: &sqlite::Connection,
    id: &str,
    new_parent_id: Option<&str>,
    new_name: &str,
) -> Result<(), String> {
    let new_name = validate_drive_name(new_name)?;
    ensure_name_available(conn, new_parent_id, &new_name, Some(id), None, None)?;
    if new_parent_id == Some(id) {
        return Err("A folder cannot be moved into itself".to_string());
    }
    if let Some(mut parent) = new_parent_id.map(str::to_string) {
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(parent.clone()) || parent == id {
                return Err("A folder cannot be moved inside one of its descendants".to_string());
            }
            let mut stmt = conn
                .prepare(
                    "SELECT parent_id FROM drive_directories WHERE id = ? AND trashed_at IS NULL",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, parent.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if !matches!(
                stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            ) {
                return Err("Destination folder does not exist".to_string());
            }
            match stmt.read::<Option<String>, _>("parent_id").ok().flatten() {
                Some(next) => parent = next,
                None => break,
            }
        }
    }
    let mut stmt = conn
        .prepare(
            "UPDATE drive_directories
             SET parent_id = ?, name = ?, updated_at = ?
             WHERE id = ? AND trashed_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, new_parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, new_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(())
}

pub fn rename_drive_file(
    conn: &sqlite::Connection,
    entry_id: &str,
    new_directory_id: Option<&str>,
    new_name: &str,
) -> Result<(), String> {
    let new_name = validate_drive_name(new_name)?;
    ensure_name_available(
        conn,
        new_directory_id,
        &new_name,
        None,
        Some(entry_id),
        None,
    )?;
    let is_v2 = {
        let mut check = conn
            .prepare(
                "SELECT 1 FROM drive_entries_v2
                 WHERE entry_id = ? AND trashed_at IS NULL
                 LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        check
            .bind((1, entry_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        matches!(
            check.next().map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        )
    };
    let sql = if is_v2 {
        "UPDATE drive_entries_v2
         SET directory_id = ?, display_name = ?, updated_at = ?
         WHERE entry_id = ? AND trashed_at IS NULL"
    } else {
        "UPDATE drive_file_entries
         SET directory_id = ?, display_name = ?, updated_at = ?
         WHERE entry_id = ? AND trashed_at IS NULL"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, new_directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, new_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(())
}

pub fn rename_drive_pending(
    conn: &sqlite::Connection,
    pending_id: &str,
    new_directory_id: Option<&str>,
    new_name: &str,
) -> Result<(), String> {
    let new_name = validate_drive_name(new_name)?;
    ensure_name_available(
        conn,
        new_directory_id,
        &new_name,
        None,
        None,
        Some(pending_id),
    )?;
    let mut stmt = conn
        .prepare(
            "UPDATE drive_pending_files
             SET directory_id = ?, display_name = ?, updated_at = ?
             WHERE id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, new_directory_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, new_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn trash_drive_file(conn: &sqlite::Connection, entry_id: &str) -> Result<(), String> {
    let now = now_ms();
    let is_v2 = {
        let mut check = conn
            .prepare(
                "SELECT 1 FROM drive_entries_v2 WHERE entry_id = ? AND trashed_at IS NULL LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        check
            .bind((1, entry_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        matches!(
            check.next().map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        )
    };
    let sql = if is_v2 {
        "UPDATE drive_entries_v2
         SET trashed_at = ?, updated_at = ?
         WHERE entry_id = ? AND trashed_at IS NULL"
    } else {
        "UPDATE drive_file_entries
         SET trashed_at = ?, updated_at = ?
         WHERE entry_id = ? AND trashed_at IS NULL"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(())
}

fn restore_parent_if_active(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(parent_id) = parent_id else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM drive_directories
             WHERE id = ? AND trashed_at IS NULL
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        Ok(Some(parent_id.to_string()))
    } else {
        Ok(None)
    }
}

fn available_restored_name(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    preferred: &str,
    directory_id: Option<&str>,
    entry_id: Option<&str>,
) -> Result<String, String> {
    if ensure_name_available(conn, parent_id, preferred, directory_id, entry_id, None).is_ok() {
        return Ok(preferred.to_string());
    }

    let path = std::path::Path::new(preferred);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Restored");
    let ext = path.extension().and_then(|value| value.to_str());
    for suffix in 1..=10_000u32 {
        let candidate = if suffix == 1 {
            match ext {
                Some(ext) => format!("{stem} (restored).{ext}"),
                None => format!("{stem} (restored)"),
            }
        } else {
            match ext {
                Some(ext) => format!("{stem} (restored {suffix}).{ext}"),
                None => format!("{stem} (restored {suffix})"),
            }
        };
        if ensure_name_available(conn, parent_id, &candidate, directory_id, entry_id, None).is_ok()
        {
            return Ok(candidate);
        }
    }
    Err("Could not choose a safe restored Drive name".to_string())
}

pub fn list_drive_trash(conn: &sqlite::Connection) -> Result<Vec<DriveTrashItem>, String> {
    init_drive_schema(conn)?;
    let mut items = Vec::new();

    let mut files = conn
        .prepare(
            "SELECT e.entry_id, e.display_name, e.directory_id, e.trashed_at,
                    f.total_size, f.status AS file_status
             FROM drive_file_entries e
             LEFT JOIN logical_files f ON f.file_id = e.file_id
             WHERE e.trashed_at IS NOT NULL
             ORDER BY e.trashed_at DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = files.next().map_err(|e: sqlite::Error| e.to_string())? {
        items.push(DriveTrashItem {
            kind: "file".to_string(),
            id: files
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            name: files
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            deleted_at: files
                .read::<i64, _>("trashed_at")
                .map_err(|e| e.to_string())?,
            original_directory_id: files
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            size: files
                .read::<Option<i64>, _>("total_size")
                .ok()
                .flatten()
                .map(|value| value as u64),
            restorable: files
                .read::<Option<String>, _>("file_status")
                .ok()
                .flatten()
                .as_deref()
                == Some("complete"),
        });
    }
    drop(files);

    let mut v2_files = conn
        .prepare(
            "SELECT e.entry_id, e.display_name, e.directory_id, e.trashed_at,
                    o.plaintext_size
             FROM drive_entries_v2 e
             JOIN drive_objects o ON o.object_id = e.object_id
             WHERE e.trashed_at IS NOT NULL
             ORDER BY e.trashed_at DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = v2_files.next().map_err(|e: sqlite::Error| e.to_string())? {
        items.push(DriveTrashItem {
            kind: "file".to_string(),
            id: v2_files
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            name: v2_files
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            deleted_at: v2_files
                .read::<i64, _>("trashed_at")
                .map_err(|e| e.to_string())?,
            original_directory_id: v2_files
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten(),
            size: Some(
                v2_files
                    .read::<i64, _>("plaintext_size")
                    .map_err(|e| e.to_string())? as u64,
            ),
            restorable: true,
        });
    }
    drop(v2_files);

    let mut directories = conn
        .prepare(
            "SELECT id, name, parent_id, trashed_at
             FROM drive_directories
             WHERE trashed_at IS NOT NULL
             ORDER BY trashed_at DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = directories
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
    {
        items.push(DriveTrashItem {
            kind: "folder".to_string(),
            id: directories
                .read::<String, _>("id")
                .map_err(|e| e.to_string())?,
            name: directories
                .read::<String, _>("name")
                .map_err(|e| e.to_string())?,
            deleted_at: directories
                .read::<i64, _>("trashed_at")
                .map_err(|e| e.to_string())?,
            original_directory_id: directories
                .read::<Option<String>, _>("parent_id")
                .ok()
                .flatten(),
            size: None,
            restorable: true,
        });
    }
    items.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    Ok(items)
}

#[tauri::command]
pub fn cmd_drive_list_trash(
    db_pool: State<'_, DbConnection>,
) -> Result<Vec<DriveTrashItem>, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    list_drive_trash(&conn)
}

pub fn restore_drive_trash_item(
    conn: &sqlite::Connection,
    kind: &str,
    id: &str,
) -> Result<String, String> {
    init_drive_schema(conn)?;
    let now = now_ms();

    match kind {
        "file" => {
            let mut v2_read = conn
                .prepare(
                    "SELECT directory_id, display_name
                     FROM drive_entries_v2
                     WHERE entry_id = ? AND trashed_at IS NOT NULL
                     LIMIT 1",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            v2_read
                .bind((1, id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if matches!(
                v2_read.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            ) {
                let original_parent = v2_read
                    .read::<Option<String>, _>("directory_id")
                    .ok()
                    .flatten();
                let original_name = v2_read
                    .read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?;
                drop(v2_read);
                let target_parent = restore_parent_if_active(&conn, original_parent.as_deref())?;
                let restored_name = available_restored_name(
                    &conn,
                    target_parent.as_deref(),
                    &original_name,
                    None,
                    Some(id),
                )?;
                let mut update = conn
                    .prepare(
                        "UPDATE drive_entries_v2
                         SET directory_id = ?, display_name = ?, trashed_at = NULL, updated_at = ?
                         WHERE entry_id = ? AND trashed_at IS NOT NULL",
                    )
                    .map_err(|e: sqlite::Error| e.to_string())?;
                update
                    .bind((1, target_parent.as_deref()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                update
                    .bind((2, restored_name.as_str()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                update
                    .bind((3, now))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                update
                    .bind((4, id))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                update.next().map_err(|e: sqlite::Error| e.to_string())?;
                mark_drive_dirty(&conn)?;
                return Ok(restored_name);
            }
            drop(v2_read);

            let mut read = conn
                .prepare(
                    "SELECT e.directory_id, e.display_name, f.status
                     FROM drive_file_entries e
                     LEFT JOIN logical_files f ON f.file_id = e.file_id
                     WHERE e.entry_id = ? AND e.trashed_at IS NOT NULL
                     LIMIT 1",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            read.bind((1, id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if !matches!(
                read.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            ) {
                return Err("Drive trash item no longer exists".to_string());
            }
            let original_parent = read
                .read::<Option<String>, _>("directory_id")
                .ok()
                .flatten();
            let original_name = read
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?;
            let status = read.read::<Option<String>, _>("status").ok().flatten();
            drop(read);
            if status.as_deref() != Some("complete") {
                return Err("The underlying TeraRelay file is no longer available".to_string());
            }
            let target_parent = restore_parent_if_active(&conn, original_parent.as_deref())?;
            let restored_name = available_restored_name(
                &conn,
                target_parent.as_deref(),
                &original_name,
                None,
                Some(&id),
            )?;
            let mut update = conn
                .prepare(
                    "UPDATE drive_file_entries
                     SET directory_id = ?, display_name = ?, trashed_at = NULL, updated_at = ?
                     WHERE entry_id = ? AND trashed_at IS NOT NULL",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((1, target_parent.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((2, restored_name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((3, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((4, id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update.next().map_err(|e: sqlite::Error| e.to_string())?;
            mark_drive_dirty(&conn)?;
            Ok(restored_name)
        }
        "folder" => {
            let mut read = conn
                .prepare(
                    "SELECT parent_id, name
                     FROM drive_directories
                     WHERE id = ? AND trashed_at IS NOT NULL
                     LIMIT 1",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            read.bind((1, id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if !matches!(
                read.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            ) {
                return Err("Drive trash folder no longer exists".to_string());
            }
            let original_parent = read.read::<Option<String>, _>("parent_id").ok().flatten();
            let original_name = read.read::<String, _>("name").map_err(|e| e.to_string())?;
            drop(read);
            let target_parent = restore_parent_if_active(&conn, original_parent.as_deref())?;
            let restored_name = available_restored_name(
                &conn,
                target_parent.as_deref(),
                &original_name,
                Some(&id),
                None,
            )?;
            let mut update = conn
                .prepare(
                    "UPDATE drive_directories
                     SET parent_id = ?, name = ?, trashed_at = NULL, updated_at = ?
                     WHERE id = ? AND trashed_at IS NOT NULL",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((1, target_parent.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((2, restored_name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((3, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update
                .bind((4, id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            update.next().map_err(|e: sqlite::Error| e.to_string())?;
            mark_drive_dirty(&conn)?;
            Ok(restored_name)
        }
        _ => Err("Unknown TeraRelay Drive trash item type".to_string()),
    }
}

#[tauri::command]
pub fn cmd_drive_restore_trash_item(
    kind: String,
    id: String,
    db_pool: State<'_, DbConnection>,
) -> Result<String, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    restore_drive_trash_item(&conn, &kind, &id)
}

pub fn delete_drive_pending(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Option<String>, String> {
    delete_drive_pending_state(conn, pending_id)?;
    let mut stmt = conn
        .prepare("SELECT staging_path FROM drive_pending_files WHERE id = ? LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    let path = if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        Some(stmt.read::<String, _>(0).map_err(|e| e.to_string())?)
    } else {
        None
    };
    drop(stmt);
    let mut delete = conn
        .prepare("DELETE FROM drive_pending_files WHERE id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(path)
}

pub fn trash_drive_directory(conn: &sqlite::Connection, id: &str) -> Result<(), String> {
    let mut child = conn
        .prepare(
            "SELECT 1 FROM drive_directories
             WHERE parent_id = ? AND trashed_at IS NULL
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    child
        .bind((1, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        child.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Folder is not empty".to_string());
    }
    drop(child);

    let mut files = conn
        .prepare(
            "SELECT 1
             FROM drive_file_entries e
             JOIN logical_files f ON f.file_id = e.file_id AND f.status = 'complete'
             JOIN logical_channels c ON c.logical_id = f.logical_channel_id
             WHERE e.directory_id = ? AND e.trashed_at IS NULL
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    files
        .bind((1, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        files.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Folder is not empty".to_string());
    }
    drop(files);

    let mut v2_files = conn
        .prepare(
            "SELECT 1 FROM drive_entries_v2
             WHERE directory_id = ? AND trashed_at IS NULL
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    v2_files
        .bind((1, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        v2_files.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Folder is not empty".to_string());
    }
    drop(v2_files);

    let mut pending = conn
        .prepare("SELECT 1 FROM drive_pending_files WHERE directory_id = ? LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((1, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        pending.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Folder is not empty".to_string());
    }
    drop(pending);

    let now = now_ms();

    // A source file can be deleted outside the Drive view. Such an entry is
    // intentionally absent from drive_snapshot(), so it must not make a
    // visually empty folder impossible to remove. Retire only entries whose
    // backing logical file is no longer renderable; visible files still block
    // rmdir above.
    let mut stale = conn
        .prepare(
            "UPDATE drive_file_entries
             SET trashed_at = ?, updated_at = ?
             WHERE directory_id = ? AND trashed_at IS NULL
               AND NOT EXISTS (
                   SELECT 1
                   FROM logical_files f
                   JOIN logical_channels c ON c.logical_id = f.logical_channel_id
                   WHERE f.file_id = drive_file_entries.file_id
                     AND f.status = 'complete'
               )",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stale
        .bind((1, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stale
        .bind((2, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stale
        .bind((3, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stale.next().map_err(|e: sqlite::Error| e.to_string())?;
    drop(stale);

    let mut stmt = conn
        .prepare(
            "UPDATE drive_directories
             SET trashed_at = ?, updated_at = ?
             WHERE id = ? AND trashed_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(())
}

#[tauri::command]
pub fn cmd_drive_finalize_upload(
    pending_id: String,
    logical_file_id: String,
    db_pool: State<'_, DbConnection>,
) -> Result<(), String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    init_drive_schema(&conn)?;

    let mut pending = conn
        .prepare(
            "SELECT directory_id, display_name, staging_path, created_at
             FROM drive_pending_files
             WHERE id = ?
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((1, pending_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        pending.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Drive upload staging record no longer exists".to_string());
    }
    let directory_id = pending
        .read::<Option<String>, _>("directory_id")
        .ok()
        .flatten();
    let display_name = pending
        .read::<String, _>("display_name")
        .map_err(|e| e.to_string())?;
    let staging_path = pending
        .read::<String, _>("staging_path")
        .map_err(|e| e.to_string())?;
    let created_at = pending
        .read::<i64, _>("created_at")
        .map_err(|e| e.to_string())?;
    drop(pending);

    let mut file_check = conn
        .prepare("SELECT 1 FROM logical_files WHERE file_id = ? AND status = 'complete' LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    file_check
        .bind((1, logical_file_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        file_check
            .next()
            .map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Uploaded logical file is not indexed yet".to_string());
    }
    drop(file_check);

    let now = now_ms();
    let mut insert = conn
        .prepare(
            "INSERT INTO drive_file_entries
             (entry_id, file_id, directory_id, display_name, created_at, updated_at, trashed_at,
              encryption_version, plaintext_size, crypto_id)
             VALUES (?, ?, ?, ?, ?, ?, NULL, 0, NULL, NULL)
             ON CONFLICT(entry_id) DO UPDATE SET
               file_id = excluded.file_id,
               directory_id = excluded.directory_id,
               display_name = excluded.display_name,
               updated_at = excluded.updated_at,
               trashed_at = NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((1, pending_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((2, logical_file_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((3, directory_id.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((4, display_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((5, created_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert
        .bind((6, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert.next().map_err(|e: sqlite::Error| e.to_string())?;
    drop(insert);

    let mut delete = conn
        .prepare("DELETE FROM drive_pending_files WHERE id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((1, pending_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(&conn)?;
    drop(delete);
    drop(conn);

    // The durable upload and Drive metadata now own the file. The staging
    // source is no longer needed; failure to clean it must never roll back a
    // successfully published remote file.
    if let Err(error) = std::fs::remove_file(&staging_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "TeraRelay Drive finalized {} but could not remove staging file {}: {}",
                logical_file_id,
                staging_path,
                error
            );
        }
    }
    if let Some(parent) = std::path::Path::new(&staging_path).parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DriveContentMatch {
    pub file_id: String,
    pub crypto_id: Option<String>,
}

pub fn find_drive_content_match(
    conn: &sqlite::Connection,
    content_fingerprint: &str,
    plaintext_size: u64,
    encryption_version: u32,
) -> Result<Option<DriveContentMatch>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.file_id, e.crypto_id
             FROM drive_file_entries e
             JOIN logical_files f ON f.file_id = e.file_id
             WHERE e.trashed_at IS NULL
               AND f.status = 'complete'
               AND e.content_fingerprint = ?
               AND e.plaintext_size = ?
               AND e.encryption_version = ?
             ORDER BY e.updated_at DESC
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, content_fingerprint))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, plaintext_size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, encryption_version as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        Ok(Some(DriveContentMatch {
            file_id: stmt
                .read::<String, _>("file_id")
                .map_err(|e| e.to_string())?,
            crypto_id: stmt.read::<Option<String>, _>("crypto_id").ok().flatten(),
        }))
    } else {
        Ok(None)
    }
}

fn valid_drive_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn finalize_drive_v2_metadata(
    conn: &sqlite::Connection,
    pending_id: &str,
    proposed_object_id: &str,
    content_fingerprint: &str,
) -> Result<DriveV2FinalizeResult, String> {
    init_drive_schema(conn)?;
    if !valid_drive_hex(proposed_object_id, 32) {
        return Err("Drive V2 object ID is invalid".to_string());
    }
    if !valid_drive_hex(content_fingerprint, 64) {
        return Err("Drive V2 content fingerprint is invalid".to_string());
    }
    let pending = drive_pending_by_id(conn, pending_id)?
        .ok_or_else(|| "Drive pending write no longer exists".to_string())?;
    if pending.closed_at.is_none() {
        return Err("Drive pending write is still open".to_string());
    }
    if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
        && pending.crypto_id.is_none()
    {
        return Err("Encrypted Drive V2 object is missing its crypto identity".to_string());
    }
    if pending.encryption_version != 0
        && pending.encryption_version != crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
    {
        return Err(format!(
            "Unsupported TeraRelay Drive encryption version {}",
            pending.encryption_version
        ));
    }

    let chunks = drive_pending_chunks(conn, pending_id)?;
    let parts = drive_pending_parts(conn, pending_id)?;
    let expected_chunks = if pending.size == 0 {
        1
    } else {
        pending
            .size
            .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
    };
    if chunks.len() as u64 != expected_chunks {
        return Err(format!(
            "Drive V2 write is incomplete: expected {expected_chunks} blocks, found {}",
            chunks.len()
        ));
    }
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.chunk_index != index as u64
            || chunk.message_id <= 0
            || chunk.chunk_size == 0
            || !valid_drive_hex(&chunk.sha256, 64)
            || !valid_drive_hex(&chunk.plaintext_sha256, 64)
        {
            return Err("Drive V2 block metadata is incomplete".to_string());
        }
    }

    let expected_parts = crate::drive_stream::remote_part_count(expected_chunks);
    if parts.len() as u64 != expected_parts {
        return Err(format!(
            "Drive V2 write is incomplete: expected {expected_parts} remote parts, found {}",
            parts.len()
        ));
    }
    for (index, part) in parts.iter().enumerate() {
        let part_index = index as u64;
        let (expected_first, expected_end) =
            crate::drive_stream::remote_part_chunk_range(part_index, expected_chunks)
                .ok_or_else(|| "Drive V2 part layout is invalid".to_string())?;
        if part.part_index != part_index
            || part.first_chunk_index != expected_first
            || part.chunk_count != expected_end - expected_first
            || part.backing_channel_id <= 0
            || part.message_id <= 0
            || part.part_size == 0
            || !valid_drive_hex(&part.sha256, 64)
        {
            return Err("Drive V2 remote part metadata is incomplete".to_string());
        }
        let mut size = 0u64;
        for chunk in &chunks[expected_first as usize..expected_end as usize] {
            if chunk.message_id != part.message_id {
                return Err("Drive V2 block points at the wrong remote part".to_string());
            }
            size = size
                .checked_add(chunk.chunk_size)
                .ok_or_else(|| "Drive V2 remote part size overflow".to_string())?;
        }
        if size != part.part_size {
            return Err("Drive V2 remote part size does not match its blocks".to_string());
        }
    }

    let duplicate_object_id = {
        let mut stmt = conn
            .prepare(
                "SELECT object_id FROM drive_objects
                 WHERE content_fingerprint = ? AND plaintext_size = ? AND encryption_version = ?
                 ORDER BY created_at ASC LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, content_fingerprint))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, pending.size as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((3, pending.encryption_version as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if matches!(
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        ) {
            Some(stmt.read::<String, _>(0).map_err(|e| e.to_string())?)
        } else {
            None
        }
    };
    let reused_existing_object = duplicate_object_id.is_some();
    let object_id = duplicate_object_id.unwrap_or_else(|| proposed_object_id.to_string());
    let entry_id = pending
        .base_entry_id
        .clone()
        .unwrap_or_else(|| pending.id.clone());
    let cleanup_parts = if reused_existing_object {
        parts.clone()
    } else {
        Vec::new()
    };

    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| -> Result<(), String> {
        if !reused_existing_object {
            let mut object = conn
                .prepare(
                    "INSERT INTO drive_objects
                     (object_id, plaintext_size, encryption_version, crypto_id,
                      content_fingerprint, block_count, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((1, object_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((2, pending.size as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((3, pending.encryption_version as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((4, pending.crypto_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((5, content_fingerprint))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((6, expected_chunks as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object
                .bind((7, now_ms()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            object.next().map_err(|e: sqlite::Error| e.to_string())?;

            for part in &parts {
                let mut insert = conn
                    .prepare(
                        "INSERT INTO drive_object_parts
                         (object_id, part_index, first_chunk_index, chunk_count,
                          backing_channel_id, message_id, ciphertext_size, sha256)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((1, object_id.as_str()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((2, part.part_index as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((3, part.first_chunk_index as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((4, part.chunk_count as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((5, part.backing_channel_id))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((6, part.message_id))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((7, part.part_size as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert
                    .bind((8, part.sha256.as_str()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                insert.next().map_err(|e: sqlite::Error| e.to_string())?;

                let mut part_offset = 0u64;
                let end = part.first_chunk_index + part.chunk_count;
                for chunk in &chunks[part.first_chunk_index as usize..end as usize] {
                    let mut block = conn
                        .prepare(
                            "INSERT INTO drive_object_blocks
                             (object_id, chunk_index, part_index, part_offset,
                              ciphertext_size, sha256, plaintext_sha256)
                             VALUES (?, ?, ?, ?, ?, ?, ?)",
                        )
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((1, object_id.as_str()))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((2, chunk.chunk_index as i64))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((3, part.part_index as i64))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((4, part_offset as i64))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((5, chunk.chunk_size as i64))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((6, chunk.sha256.as_str()))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block
                        .bind((7, chunk.plaintext_sha256.as_str()))
                        .map_err(|e: sqlite::Error| e.to_string())?;
                    block.next().map_err(|e: sqlite::Error| e.to_string())?;
                    part_offset = part_offset
                        .checked_add(chunk.chunk_size)
                        .ok_or_else(|| "Drive V2 block offset overflow".to_string())?;
                }
            }
        }

        let now = now_ms();
        let mut entry = conn
            .prepare(
                "INSERT INTO drive_entries_v2
                 (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
                 VALUES (?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(entry_id) DO UPDATE SET
                    object_id = excluded.object_id,
                    directory_id = excluded.directory_id,
                    display_name = excluded.display_name,
                    updated_at = excluded.updated_at,
                    trashed_at = NULL",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((1, entry_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((2, object_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((3, pending.directory_id.as_deref()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((4, pending.display_name.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((5, pending.created_at))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((6, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry.next().map_err(|e: sqlite::Error| e.to_string())?;

        if pending.base_entry_id.is_some() {
            let mut legacy = conn
                .prepare("DELETE FROM drive_file_entries WHERE entry_id = ?")
                .map_err(|e: sqlite::Error| e.to_string())?;
            legacy
                .bind((1, entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            legacy.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        delete_drive_pending_state(conn, pending_id)?;
        let mut delete = conn
            .prepare("DELETE FROM drive_pending_files WHERE id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
        mark_drive_dirty(conn)?;
        Ok(())
    })();
    match result {
        Ok(()) => conn
            .execute("COMMIT")
            .map_err(|e: sqlite::Error| e.to_string())?,
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            return Err(error);
        }
    }

    Ok(DriveV2FinalizeResult {
        object_id,
        entry_id,
        staging_path: pending.staging_path,
        reused_existing_object,
        cleanup_parts,
    })
}

pub fn finalize_drive_streamed_pending(
    conn: &sqlite::Connection,
    pending_id: &str,
    logical_file_id: &str,
    plaintext_size: u64,
    content_fingerprint: Option<&str>,
    crypto_id_override: Option<&str>,
) -> Result<Option<String>, String> {
    init_drive_schema(conn)?;
    let pending = drive_pending_by_id(conn, pending_id)?
        .ok_or_else(|| "Drive pending write no longer exists".to_string())?;
    if pending.closed_at.is_none() {
        return Err("Drive pending write is still open".to_string());
    }

    let effective_crypto_id = crypto_id_override.or(pending.crypto_id.as_deref());
    if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
        && effective_crypto_id.is_none()
    {
        return Err("Encrypted Drive entry is missing its crypto identity".to_string());
    }

    let now = now_ms();
    if let Some(base_entry_id) = pending.base_entry_id.as_deref() {
        let mut update = conn
            .prepare(
                "UPDATE drive_file_entries
                 SET file_id = ?, directory_id = ?, display_name = ?, updated_at = ?,
                     trashed_at = NULL, encryption_version = ?, plaintext_size = ?,
                     crypto_id = ?, content_fingerprint = ?
                 WHERE entry_id = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((1, logical_file_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((2, pending.directory_id.as_deref()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((3, pending.display_name.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((4, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((5, pending.encryption_version as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((6, plaintext_size as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((7, effective_crypto_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((8, content_fingerprint))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update
            .bind((9, base_entry_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        update.next().map_err(|e: sqlite::Error| e.to_string())?;
    } else {
        let mut insert = conn
            .prepare(
                "INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                  trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
                 VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)
                 ON CONFLICT(entry_id) DO UPDATE SET
                    file_id = excluded.file_id,
                    directory_id = excluded.directory_id,
                    display_name = excluded.display_name,
                    updated_at = excluded.updated_at,
                    trashed_at = NULL,
                    encryption_version = excluded.encryption_version,
                    plaintext_size = excluded.plaintext_size,
                    crypto_id = excluded.crypto_id,
                    content_fingerprint = excluded.content_fingerprint",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((1, pending.id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((2, logical_file_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((3, pending.directory_id.as_deref()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((4, pending.display_name.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((5, pending.created_at))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((6, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((7, pending.encryption_version as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((8, plaintext_size as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((9, effective_crypto_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((10, content_fingerprint))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert.next().map_err(|e: sqlite::Error| e.to_string())?;
    }

    delete_drive_pending_state(conn, pending_id)?;
    let mut delete = conn
        .prepare("DELETE FROM drive_pending_files WHERE id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    mark_drive_dirty(conn)?;
    Ok(Some(pending.staging_path))
}

fn validate_manifest(manifest: &DriveManifestV1) -> Result<(), String> {
    if manifest.schema_version != LEGACY_DRIVE_SCHEMA_VERSION
        && manifest.schema_version != PRE_STORAGE_POOL_DRIVE_SCHEMA_VERSION
        && manifest.schema_version != DRIVE_SCHEMA_VERSION
    {
        return Err(format!(
            "Unsupported TeraRelay Drive metadata version {}",
            manifest.schema_version
        ));
    }
    if let Some(crypto) = manifest.crypto.as_ref() {
        if crypto.version != crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
            return Err(format!(
                "Unsupported TeraRelay Drive encryption metadata version {}",
                crypto.version
            ));
        }
    }
    let mut directory_ids = HashSet::new();
    for directory in &manifest.directories {
        if directory.id.is_empty() || !directory_ids.insert(directory.id.clone()) {
            return Err("Drive metadata contains an invalid or duplicate directory ID".to_string());
        }
        validate_drive_name(&directory.name)?;
    }
    for directory in &manifest.directories {
        if let Some(parent) = directory.parent_id.as_deref() {
            if !directory_ids.contains(parent) {
                return Err("Drive metadata references a missing parent folder".to_string());
            }
        }
    }
    let valid_hex_64 = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };

    let mut entry_ids = HashSet::new();
    for entry in &manifest.entries {
        if entry.entry_id.is_empty() || !entry_ids.insert(entry.entry_id.clone()) {
            return Err("Drive metadata contains an invalid or duplicate entry ID".to_string());
        }
        if entry.file_id.is_empty() {
            return Err("Drive metadata contains an invalid file ID".to_string());
        }
        validate_drive_name(&entry.display_name)?;
        if let Some(directory_id) = entry.directory_id.as_deref() {
            if !directory_ids.contains(directory_id) {
                return Err("Drive metadata references a missing file folder".to_string());
            }
        }

        match entry.encryption_version {
            0 => {
                if let Some(fingerprint) = entry.content_fingerprint.as_deref() {
                    if !valid_hex_64(fingerprint) {
                        return Err(
                            "Drive metadata contains an invalid content fingerprint".to_string()
                        );
                    }
                }
            }
            crate::drive_crypto::DRIVE_ENCRYPTION_VERSION => {
                if manifest.crypto.is_none() {
                    return Err(
                        "Encrypted Drive entries are missing Drive encryption metadata".to_string(),
                    );
                }
                if entry
                    .crypto_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    return Err("Encrypted Drive entry is missing its crypto identity".to_string());
                }
                if entry.plaintext_size.is_none() {
                    return Err("Encrypted Drive entry is missing its plaintext size".to_string());
                }
                let fingerprint = entry.content_fingerprint.as_deref().ok_or_else(|| {
                    "Encrypted Drive entry is missing its private content fingerprint".to_string()
                })?;
                if !valid_hex_64(fingerprint) {
                    return Err(
                        "Encrypted Drive entry has an invalid content fingerprint".to_string()
                    );
                }
            }
            version => {
                return Err(format!(
                    "Unsupported TeraRelay Drive entry encryption version {version}"
                ));
            }
        }
    }

    if manifest.schema_version < DRIVE_SCHEMA_VERSION
        && (!manifest.storage_channels.is_empty()
            || !manifest.objects.is_empty()
            || !manifest.parts.is_empty()
            || !manifest.v2_entries.is_empty())
    {
        return Err("Legacy Drive metadata cannot contain storage-pool records".to_string());
    }

    let mut storage_generations = HashSet::new();
    let mut storage_backing_ids = HashSet::new();
    let mut active_storage_channels = 0usize;
    for channel in &manifest.storage_channels {
        if channel.generation <= 0
            || channel.backing_channel_id <= 0
            || !storage_generations.insert(channel.generation)
            || !storage_backing_ids.insert(channel.backing_channel_id)
            || channel.title.trim().is_empty()
            || !matches!(channel.state.as_str(), "active" | "read_only" | "retired")
        {
            return Err("Drive metadata contains an invalid storage channel".to_string());
        }
        if channel.state == "active" {
            active_storage_channels += 1;
        }
    }
    if active_storage_channels > 1 {
        return Err("Drive metadata contains multiple active storage channels".to_string());
    }

    let mut objects_by_id = BTreeMap::new();
    for object in &manifest.objects {
        let expected_blocks = if object.plaintext_size == 0 {
            1
        } else {
            object
                .plaintext_size
                .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
        };
        if !valid_drive_hex(&object.object_id, 32)
            || !valid_hex_64(&object.content_fingerprint)
            || object.block_count != expected_blocks
            || objects_by_id
                .insert(object.object_id.clone(), object)
                .is_some()
        {
            return Err("Drive metadata contains an invalid or duplicate V2 object".to_string());
        }
        decode_manifest_block_hashes(object)?;
        match object.encryption_version {
            0 => {}
            crate::drive_crypto::DRIVE_ENCRYPTION_VERSION => {
                if manifest.crypto.is_none()
                    || object
                        .crypto_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .is_none()
                {
                    return Err(
                        "Encrypted Drive V2 object is missing encryption metadata".to_string()
                    );
                }
            }
            version => {
                return Err(format!(
                    "Unsupported TeraRelay Drive V2 encryption version {version}"
                ));
            }
        }
    }

    let mut part_keys = HashSet::new();
    let mut parts_by_object: BTreeMap<String, Vec<&DriveObjectPartRecord>> = BTreeMap::new();
    for part in &manifest.parts {
        if !objects_by_id.contains_key(&part.object_id)
            || !part_keys.insert((part.object_id.clone(), part.part_index))
            || part.chunk_count == 0
            || part.backing_channel_id <= 0
            || part.message_id <= 0
            || !valid_hex_64(&part.sha256)
            || !storage_backing_ids.contains(&part.backing_channel_id)
        {
            return Err("Drive metadata contains an invalid V2 remote part".to_string());
        }
        parts_by_object
            .entry(part.object_id.clone())
            .or_default()
            .push(part);
    }
    for (object_id, object) in &objects_by_id {
        let parts = parts_by_object
            .get_mut(object_id)
            .ok_or_else(|| "Drive V2 object is missing all remote parts".to_string())?;
        parts.sort_by_key(|part| part.part_index);
        let mut expected_part = 0u64;
        let mut expected_chunk = 0u64;
        for part in parts.iter() {
            if part.part_index != expected_part || part.first_chunk_index != expected_chunk {
                return Err("Drive V2 remote part ordering is incomplete".to_string());
            }
            expected_part = expected_part.saturating_add(1);
            expected_chunk = expected_chunk
                .checked_add(part.chunk_count)
                .ok_or_else(|| "Drive V2 block count overflow".to_string())?;
        }
        if expected_chunk != object.block_count {
            return Err("Drive V2 remote parts do not cover the complete object".to_string());
        }
        // V3 manifests carry one compact encoded-block checksum per block so a
        // second device can verify random-range reads without fetching a whole
        // remote part first.
        decode_manifest_block_hashes(object)?;
    }

    let mut v2_entry_ids = HashSet::new();
    for entry in &manifest.v2_entries {
        if entry.entry_id.is_empty()
            || entry_ids.contains(&entry.entry_id)
            || !v2_entry_ids.insert(entry.entry_id.clone())
            || !objects_by_id.contains_key(&entry.object_id)
        {
            return Err("Drive metadata contains an invalid or duplicate V2 entry".to_string());
        }
        validate_drive_name(&entry.display_name)?;
        if let Some(directory_id) = entry.directory_id.as_deref() {
            if !directory_ids.contains(directory_id) {
                return Err("Drive V2 metadata references a missing file folder".to_string());
            }
        }
    }
    Ok(())
}

fn build_manifest(conn: &sqlite::Connection) -> Result<DriveManifestV1, String> {
    let (revision, _) = state_row(conn)?;
    let manifest = DriveManifestV1 {
        schema_version: DRIVE_SCHEMA_VERSION,
        revision,
        crypto: crate::drive_crypto::load_crypto_envelope(conn)?,
        directories: load_manifest_directories(conn)?,
        entries: load_manifest_entries(conn)?,
        storage_channels: load_manifest_storage_channels(conn)?,
        objects: load_manifest_objects(conn)?,
        parts: load_manifest_parts(conn)?,
        v2_entries: load_manifest_v2_entries(conn)?,
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn choose_newer_directory(
    current: DriveDirectoryRecord,
    candidate: DriveDirectoryRecord,
) -> DriveDirectoryRecord {
    if candidate.updated_at > current.updated_at {
        return candidate;
    }
    if candidate.updated_at < current.updated_at {
        return current;
    }
    // Equal millisecond timestamps can happen on two computers. Make the
    // winner deterministic so every device converges to the same manifest.
    let current_key = serde_json::to_vec(&current).unwrap_or_default();
    let candidate_key = serde_json::to_vec(&candidate).unwrap_or_default();
    if candidate_key > current_key {
        candidate
    } else {
        current
    }
}

fn choose_newer_entry(current: DriveEntryRecord, candidate: DriveEntryRecord) -> DriveEntryRecord {
    if candidate.updated_at > current.updated_at {
        return candidate;
    }
    if candidate.updated_at < current.updated_at {
        return current;
    }
    let current_key = serde_json::to_vec(&current).unwrap_or_default();
    let candidate_key = serde_json::to_vec(&candidate).unwrap_or_default();
    if candidate_key > current_key {
        candidate
    } else {
        current
    }
}

fn choose_newer_storage_channel(
    current: DriveStorageManifestRecord,
    candidate: DriveStorageManifestRecord,
) -> DriveStorageManifestRecord {
    if candidate.updated_at > current.updated_at {
        return candidate;
    }
    if candidate.updated_at < current.updated_at {
        return current;
    }
    let current_key = serde_json::to_vec(&current).unwrap_or_default();
    let candidate_key = serde_json::to_vec(&candidate).unwrap_or_default();
    if candidate_key > current_key {
        candidate
    } else {
        current
    }
}

fn choose_newer_v2_entry(
    current: DriveEntryV2Record,
    candidate: DriveEntryV2Record,
) -> DriveEntryV2Record {
    if candidate.updated_at > current.updated_at {
        return candidate;
    }
    if candidate.updated_at < current.updated_at {
        return current;
    }
    let current_key = serde_json::to_vec(&current).unwrap_or_default();
    let candidate_key = serde_json::to_vec(&candidate).unwrap_or_default();
    if candidate_key > current_key {
        candidate
    } else {
        current
    }
}

fn same_drive_master_key(
    left: &crate::drive_crypto::DriveCryptoEnvelope,
    right: &crate::drive_crypto::DriveCryptoEnvelope,
) -> bool {
    match (
        left.key_check_b64.as_deref(),
        right.key_check_b64.as_deref(),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => left.wrapped_master_key_b64 == right.wrapped_master_key_b64,
    }
}

fn merge_drive_manifests(
    left: &DriveManifestV1,
    right: &DriveManifestV1,
) -> Result<DriveManifestV1, String> {
    validate_manifest(left)?;
    validate_manifest(right)?;

    let mut directories: BTreeMap<String, DriveDirectoryRecord> = BTreeMap::new();
    for directory in left
        .directories
        .iter()
        .cloned()
        .chain(right.directories.iter().cloned())
    {
        match directories.remove(&directory.id) {
            Some(existing) => {
                directories.insert(
                    directory.id.clone(),
                    choose_newer_directory(existing, directory),
                );
            }
            None => {
                directories.insert(directory.id.clone(), directory);
            }
        }
    }

    let mut entries: BTreeMap<String, DriveEntryRecord> = BTreeMap::new();
    for entry in left
        .entries
        .iter()
        .cloned()
        .chain(right.entries.iter().cloned())
    {
        match entries.remove(&entry.entry_id) {
            Some(existing) => {
                entries.insert(entry.entry_id.clone(), choose_newer_entry(existing, entry));
            }
            None => {
                entries.insert(entry.entry_id.clone(), entry);
            }
        }
    }

    let mut storage_channels: BTreeMap<i64, DriveStorageManifestRecord> = BTreeMap::new();
    for channel in left
        .storage_channels
        .iter()
        .cloned()
        .chain(right.storage_channels.iter().cloned())
    {
        match storage_channels.remove(&channel.backing_channel_id) {
            Some(existing) => {
                storage_channels.insert(
                    channel.backing_channel_id,
                    choose_newer_storage_channel(existing, channel),
                );
            }
            None => {
                storage_channels.insert(channel.backing_channel_id, channel);
            }
        }
    }

    let mut objects: BTreeMap<String, DriveObjectRecord> = BTreeMap::new();
    for object in left
        .objects
        .iter()
        .cloned()
        .chain(right.objects.iter().cloned())
    {
        match objects.get(&object.object_id) {
            Some(existing) if existing != &object => {
                return Err(format!(
                    "Conflicting immutable TeraRelay Drive object metadata for {}",
                    object.object_id
                ));
            }
            Some(_) => {}
            None => {
                objects.insert(object.object_id.clone(), object);
            }
        }
    }

    let mut parts: BTreeMap<(String, u64), DriveObjectPartRecord> = BTreeMap::new();
    for part in left
        .parts
        .iter()
        .cloned()
        .chain(right.parts.iter().cloned())
    {
        let key = (part.object_id.clone(), part.part_index);
        match parts.get(&key) {
            Some(existing) if existing != &part => {
                return Err(format!(
                    "Conflicting immutable TeraRelay Drive part metadata for {} part {}",
                    part.object_id, part.part_index
                ));
            }
            Some(_) => {}
            None => {
                parts.insert(key, part);
            }
        }
    }

    let mut v2_entries: BTreeMap<String, DriveEntryV2Record> = BTreeMap::new();
    for entry in left
        .v2_entries
        .iter()
        .cloned()
        .chain(right.v2_entries.iter().cloned())
    {
        match v2_entries.remove(&entry.entry_id) {
            Some(existing) => {
                v2_entries.insert(
                    entry.entry_id.clone(),
                    choose_newer_v2_entry(existing, entry),
                );
            }
            None => {
                v2_entries.insert(entry.entry_id.clone(), entry);
            }
        }
    }

    let crypto = match (left.crypto.clone(), right.crypto.clone()) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(left_crypto), Some(right_crypto)) => {
            // The passphrase wrapper can rotate, but the underlying master
            // key must never fork. A concurrent first-time setup on two
            // computers is a conflict, not an update that can be resolved by
            // a timestamp: silently picking one would strand the other
            // device's encrypted content.
            if !same_drive_master_key(&left_crypto, &right_crypto) {
                return Err(
                    "Conflicting TeraRelay Drive encryption keys found on different devices.                      Stop syncing and recover the correct Drive key before continuing."
                        .to_string(),
                );
            }
            if right_crypto.updated_at > left_crypto.updated_at {
                Some(right_crypto)
            } else if right_crypto.updated_at < left_crypto.updated_at {
                Some(left_crypto)
            } else {
                let left_key = serde_json::to_vec(&left_crypto).unwrap_or_default();
                let right_key = serde_json::to_vec(&right_crypto).unwrap_or_default();
                if right_key > left_key {
                    Some(right_crypto)
                } else {
                    Some(left_crypto)
                }
            }
        }
    };

    let merged = DriveManifestV1 {
        schema_version: DRIVE_SCHEMA_VERSION,
        revision: left.revision.max(right.revision),
        crypto,
        directories: directories.into_values().collect(),
        entries: entries.into_values().collect(),
        storage_channels: storage_channels.into_values().collect(),
        objects: objects.into_values().collect(),
        parts: parts.into_values().collect(),
        v2_entries: v2_entries.into_values().collect(),
    };
    validate_manifest(&merged)?;
    Ok(merged)
}

fn apply_manifest(conn: &sqlite::Connection, manifest: &DriveManifestV1) -> Result<(), String> {
    validate_manifest(manifest)?;
    crate::drive_storage::init_drive_storage_schema(conn)?;
    let objects_by_id: BTreeMap<&str, &DriveObjectRecord> = manifest
        .objects
        .iter()
        .map(|object| (object.object_id.as_str(), object))
        .collect();
    let mut block_hashes_by_id: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for object in &manifest.objects {
        block_hashes_by_id.insert(
            object.object_id.as_str(),
            decode_manifest_block_hashes(object)?,
        );
    }
    match (
        crate::drive_crypto::load_crypto_envelope(conn)?,
        manifest.crypto.as_ref(),
    ) {
        (Some(local), Some(remote)) if !same_drive_master_key(&local, remote) => {
            return Err(
                "Remote TeraRelay Drive encryption key does not match this device.                  Refusing to replace the local key metadata."
                    .to_string(),
            );
        }
        (Some(_), None) => {
            return Err(
                "Remote Drive metadata lacks this device's encryption key.                  Refusing to erase the local Drive encryption configuration."
                    .to_string(),
            );
        }
        _ => {}
    }
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        conn.execute(
            "DELETE FROM drive_file_entries;
             DELETE FROM drive_entries_v2;
             DELETE FROM drive_object_blocks;
             DELETE FROM drive_object_parts;
             DELETE FROM drive_objects;
             DELETE FROM drive_storage_channels;
             DELETE FROM drive_directories;",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;

        for directory in &manifest.directories {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_directories
                     (id, parent_id, name, backing_logical_channel_id,
                      created_at, updated_at, trashed_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, directory.id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, directory.parent_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, directory.name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, directory.backing_logical_channel_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, directory.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, directory.updated_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, directory.trashed_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        for channel in &manifest.storage_channels {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_storage_channels
                     (generation, backing_channel_id, state, title, created_at, updated_at,
                      retirement_reason)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, channel.generation))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, channel.backing_channel_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, channel.state.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, channel.title.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, channel.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, channel.updated_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, channel.retirement_reason.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        for object in &manifest.objects {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_objects
                     (object_id, plaintext_size, encryption_version, crypto_id,
                      content_fingerprint, block_count, block_hashes_b64, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, object.object_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, object.plaintext_size as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, object.encryption_version as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, object.crypto_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, object.content_fingerprint.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, object.block_count as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, object.block_hashes_b64.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((8, object.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        for part in &manifest.parts {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_object_parts
                     (object_id, part_index, first_chunk_index, chunk_count,
                      backing_channel_id, message_id, ciphertext_size, sha256)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, part.object_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, part.part_index as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, part.first_chunk_index as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, part.chunk_count as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, part.backing_channel_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, part.message_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, part.ciphertext_size as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((8, part.sha256.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;

            let object = objects_by_id
                .get(part.object_id.as_str())
                .copied()
                .ok_or_else(|| "Drive V2 remote part references a missing object".to_string())?;
            let chunk_size = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
            let block_hashes = block_hashes_by_id
                .get(part.object_id.as_str())
                .ok_or_else(|| "Drive V2 object block hashes are missing".to_string())?;
            let mut part_offset = 0u64;
            let end_chunk = part
                .first_chunk_index
                .checked_add(part.chunk_count)
                .ok_or_else(|| "Drive V2 remote part chunk range overflow".to_string())?;
            for chunk_index in part.first_chunk_index..end_chunk {
                let plain_start = chunk_index
                    .checked_mul(chunk_size)
                    .ok_or_else(|| "Drive V2 plaintext offset overflow".to_string())?;
                let plain_len = object
                    .plaintext_size
                    .saturating_sub(plain_start)
                    .min(chunk_size);
                let encoded_len =
                    crate::drive_crypto::encoded_chunk_len(object.encryption_version, plain_len)?;
                let block_hash = block_hashes
                    .get(chunk_index as usize)
                    .ok_or_else(|| "Drive V2 object block hash index is missing".to_string())?;
                let mut block = conn
                    .prepare(
                        "INSERT INTO drive_object_blocks
                         (object_id, chunk_index, part_index, part_offset,
                          ciphertext_size, sha256, plaintext_sha256)
                         VALUES (?, ?, ?, ?, ?, ?, '')",
                    )
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((1, object.object_id.as_str()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((2, chunk_index as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((3, part.part_index as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((4, part_offset as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((5, encoded_len as i64))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block
                    .bind((6, block_hash.as_str()))
                    .map_err(|e: sqlite::Error| e.to_string())?;
                block.next().map_err(|e: sqlite::Error| e.to_string())?;
                part_offset = part_offset
                    .checked_add(encoded_len)
                    .ok_or_else(|| "Drive V2 remote part offset overflow".to_string())?;
            }
            if part_offset != part.ciphertext_size {
                return Err(
                    "Drive V2 remote part size does not match reconstructed blocks".to_string(),
                );
            }
        }

        for entry in &manifest.entries {
            let mut exists = conn
                .prepare("SELECT 1 FROM logical_files WHERE file_id = ? LIMIT 1")
                .map_err(|e: sqlite::Error| e.to_string())?;
            exists
                .bind((1, entry.file_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            let local_file_exists = matches!(
                exists.next().map_err(|e: sqlite::Error| e.to_string())?,
                sqlite::State::Row
            );
            drop(exists);
            if !local_file_exists {
                // Keep the canonical remote manifest intact. This entry will be
                // materialized on a later sync after its logical-file manifest
                // has been indexed on this device.
                continue;
            }

            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_file_entries
                     (entry_id, file_id, directory_id, display_name, created_at, updated_at, trashed_at,
                      encryption_version, plaintext_size, crypto_id, content_fingerprint)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, entry.entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, entry.file_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, entry.directory_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, entry.display_name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, entry.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, entry.updated_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, entry.trashed_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((8, entry.encryption_version as i64))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((9, entry.plaintext_size.map(|value| value as i64)))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((10, entry.crypto_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((11, entry.content_fingerprint.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        for entry in &manifest.v2_entries {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO drive_entries_v2
                     (entry_id, object_id, directory_id, display_name,
                      created_at, updated_at, trashed_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, entry.entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, entry.object_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, entry.directory_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, entry.display_name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, entry.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, entry.updated_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, entry.trashed_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        crate::drive_crypto::save_crypto_envelope(conn, manifest.crypto.as_ref())?;
        set_drive_clean(conn, manifest.revision, None)?;
        Ok::<(), String>(())
    })();

    match result {
        Ok(()) => conn
            .execute("COMMIT")
            .map_err(|e: sqlite::Error| e.to_string()),
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            Err(error)
        }
    }
}

pub fn is_drive_manifest_document(file_name: &str, caption: &str) -> bool {
    file_name == DRIVE_MANIFEST_FILE || caption.starts_with(DRIVE_MANIFEST_CAPTION)
}

async fn download_remote_manifest(
    client: &Client,
    media: &Media,
) -> Result<DriveManifestV1, String> {
    let mut data = Vec::new();
    let mut download = client.iter_download(media).chunk_size(64 * 1024);
    loop {
        match download.next().await {
            Ok(Some(chunk)) => {
                if data.len() as u64 + chunk.len() as u64 > MAX_DRIVE_MANIFEST_BYTES {
                    return Err("TeraRelay Drive metadata exceeds the safety limit".to_string());
                }
                data.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return Err(format!(
                    "Failed to download TeraRelay Drive metadata: {error}"
                ))
            }
        }
    }
    let manifest: DriveManifestV1 = serde_json::from_slice(&data)
        .map_err(|error| format!("Invalid TeraRelay Drive metadata: {error}"))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

async fn find_remote_manifests(
    client: &Client,
    peer: &Peer,
) -> Result<Vec<(i32, DriveManifestV1)>, String> {
    let mut found = Vec::new();
    let mut messages = client.iter_messages(peer).limit(200);
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        let Some(media) = message.media() else {
            continue;
        };
        let Media::Document(document) = &media else {
            continue;
        };
        if !is_drive_manifest_document(document.name(), message.text()) {
            continue;
        }
        match download_remote_manifest(client, &media).await {
            Ok(manifest) => found.push((message.id(), manifest)),
            Err(error) => log::warn!(
                "Ignoring invalid TeraRelay Drive metadata message {}: {}",
                message.id(),
                error
            ),
        }
    }
    Ok(found)
}

async fn stage_manifest(
    app: &AppHandle,
    manifest: &DriveManifestV1,
) -> Result<(PathBuf, u64), String> {
    validate_manifest(manifest)?;
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| format!("Failed to encode TeraRelay Drive metadata: {error}"))?;
    if bytes.len() as u64 > MAX_DRIVE_MANIFEST_BYTES {
        return Err("TeraRelay Drive metadata exceeds the safety limit".to_string());
    }
    let root = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("Failed to resolve TeraRelay cache directory: {e}"))?
        .join("drive-metadata-staging");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| format!("Failed to create Drive metadata staging directory: {e}"))?;
    let path = root.join(DRIVE_MANIFEST_FILE);
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| format!("Failed to stage TeraRelay Drive metadata: {e}"))?;
    Ok((path, bytes.len() as u64))
}

async fn publish_manifest(
    client: &Client,
    app: &AppHandle,
    peer: &Peer,
    manifest: &DriveManifestV1,
    stale_ids: &[i32],
) -> Result<i32, String> {
    let (path, size) = stage_manifest(app, manifest).await?;
    let result = async {
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| format!("Failed to open Drive metadata staging file: {e}"))?;
        let uploaded = client
            .upload_stream(&mut file, size as usize, DRIVE_MANIFEST_FILE.to_string())
            .await
            .map_err(map_error)?;
        let sent = client
            .send_message(
                peer,
                InputMessage::new()
                    .text(format!("{}:{}", DRIVE_MANIFEST_CAPTION, manifest.revision))
                    .file(uploaded),
            )
            .await
            .map_err(map_error)?;
        Ok::<i32, String>(sent.id())
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    let new_id = result?;
    let stale: Vec<i32> = stale_ids
        .iter()
        .copied()
        .filter(|id| *id != new_id)
        .collect();
    if !stale.is_empty() {
        if let Err(error) = client.delete_messages(peer, &stale).await {
            log::warn!(
                "Published TeraRelay Drive metadata but could not delete {} stale copy/copies: {}",
                stale.len(),
                error
            );
        }
    }
    Ok(new_id)
}

#[tauri::command]
pub async fn cmd_drive_sync_metadata(
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveSyncResult, String> {
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        init_drive_schema(&conn)?;
    }

    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let (revision, dirty) = state_row(&conn)?;
        return Ok(DriveSyncResult {
            revision,
            dirty,
            source: "qa-local".to_string(),
        });
    }

    let client = {
        state
            .client
            .lock()
            .await
            .clone()
            .ok_or_else(|| "Telegram client is not connected".to_string())?
    };
    let peer = resolve_peer(&client, None, &state.peer_cache).await?;
    let remote = find_remote_manifests(&client, &peer).await?;

    let (local_revision, local_dirty, local_manifest) = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let (revision, dirty) = state_row(&conn)?;
        (revision, dirty, build_manifest(&conn)?)
    };

    let remote_canonical = {
        let mut manifests = remote.iter().map(|(_, manifest)| manifest.clone());
        match manifests.next() {
            Some(mut canonical) => {
                for manifest in manifests {
                    canonical = merge_drive_manifests(&canonical, &manifest)?;
                }
                Some(canonical)
            }
            None => None,
        }
    };

    // Local unsynced changes are merged record-by-record with every remote
    // snapshot. This prevents two offline computers from overwriting unrelated
    // folder/file operations when they reconnect.
    if local_dirty {
        let mut manifest = match remote_canonical.as_ref() {
            Some(remote_manifest) => merge_drive_manifests(&local_manifest, remote_manifest)?,
            None => local_manifest,
        };
        manifest.revision = manifest
            .revision
            .max(local_revision)
            .max(now_ms())
            .saturating_add(1);
        let stale_ids: Vec<i32> = remote.iter().map(|(id, _)| *id).collect();
        let message_id =
            publish_manifest(&client, &app_handle, &peer, &manifest, &stale_ids).await?;
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        apply_manifest(&conn, &manifest)?;
        set_drive_clean(&conn, manifest.revision, Some(message_id as i64))?;
        return Ok(DriveSyncResult {
            revision: manifest.revision,
            dirty: false,
            source: "merged-published".to_string(),
        });
    }

    if let Some(mut manifest) = remote_canonical {
        // More than one valid remote index means concurrent or interrupted
        // publication. Merge every copy and publish one canonical snapshot so
        // all devices converge and stale metadata documents are cleaned.
        if remote.len() > 1 {
            if local_revision >= manifest.revision {
                manifest = merge_drive_manifests(&manifest, &local_manifest)?;
            }
            manifest.revision = manifest
                .revision
                .max(local_revision)
                .max(now_ms())
                .saturating_add(1);
            let stale_ids: Vec<i32> = remote.iter().map(|(id, _)| *id).collect();
            let message_id =
                publish_manifest(&client, &app_handle, &peer, &manifest, &stale_ids).await?;
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            apply_manifest(&conn, &manifest)?;
            set_drive_clean(&conn, manifest.revision, Some(message_id as i64))?;
            return Ok(DriveSyncResult {
                revision: manifest.revision,
                dirty: false,
                source: "remote-merged".to_string(),
            });
        }

        if manifest.revision > local_revision {
            let message_id = remote.first().map(|(id, _)| i64::from(*id));
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            apply_manifest(&conn, &manifest)?;
            set_drive_clean(&conn, manifest.revision, message_id)?;
            return Ok(DriveSyncResult {
                revision: manifest.revision,
                dirty: false,
                source: "remote".to_string(),
            });
        }

        // Re-apply an equal revision too. A logical-file manifest may have been
        // indexed after an earlier Drive sync skipped that entry, so this makes
        // cross-device metadata eventually materialize without requiring a
        // synthetic Drive edit.
        if manifest.revision == local_revision {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            apply_manifest(&conn, &manifest)?;
        }
    }

    Ok(DriveSyncResult {
        revision: local_revision,
        dirty: false,
        source: "current".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_db() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_channels (
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
            );",
        )
        .unwrap();
        init_drive_schema(&conn).unwrap();
        conn
    }

    fn insert_v2_test_file(
        conn: &sqlite::Connection,
        entry_id: &str,
        object_id: &str,
        directory_id: Option<&str>,
        display_name: &str,
        backing_channel_id: i64,
        message_id: i64,
    ) {
        conn.execute(format!(
            "INSERT INTO drive_objects
             (object_id, plaintext_size, encryption_version, crypto_id,
              content_fingerprint, block_count, created_at)
             VALUES ('{object_id}', 1, 0, NULL,
                     'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, 1);"
        ))
        .unwrap();
        let directory_sql = directory_id
            .map(|value| format!("'{value}'"))
            .unwrap_or_else(|| "NULL".to_string());
        conn.execute(format!(
            "INSERT INTO drive_entries_v2
             (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('{entry_id}', '{object_id}', {directory_sql}, '{display_name}', 1, 1, NULL);
             INSERT INTO drive_object_parts
             (object_id, part_index, first_chunk_index, chunk_count, backing_channel_id,
              message_id, ciphertext_size, sha256)
             VALUES ('{object_id}', 0, 0, 1, {backing_channel_id}, {message_id}, 1,
                     'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
             INSERT INTO drive_object_blocks
             (object_id, chunk_index, part_index, part_offset, ciphertext_size, sha256,
              plaintext_sha256)
             VALUES ('{object_id}', 0, 0, 0, 1,
                     'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',
                     'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd');"
        ))
        .unwrap();
    }

    #[test]
    fn new_pending_writes_are_v2_and_legacy_rows_migrate_as_v1() {
        let conn = memory_db();
        let pending = create_drive_pending_file(
            &conn,
            "11111111222222223333333344444444",
            None,
            "new-v2.bin",
            "/tmp/new-v2.bin",
        )
        .unwrap();
        assert_eq!(pending.format_version, 2);

        let legacy = sqlite::open(":memory:").unwrap();
        legacy
            .execute(
                "CREATE TABLE logical_channels (
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
                CREATE TABLE drive_pending_files (
                    id TEXT PRIMARY KEY,
                    directory_id TEXT,
                    display_name TEXT NOT NULL,
                    staging_path TEXT NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0,
                    backing_channel_id INTEGER NOT NULL,
                    base_entry_id TEXT,
                    encryption_version INTEGER NOT NULL DEFAULT 0,
                    crypto_id TEXT,
                    closed_at INTEGER,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO drive_pending_files
                    (id, directory_id, display_name, staging_path, size, backing_channel_id,
                     base_entry_id, encryption_version, crypto_id, closed_at, created_at, updated_at)
                VALUES ('legacy-pending', NULL, 'legacy.bin', '/tmp/legacy.bin', 1, 101,
                        NULL, 0, NULL, 1, 1, 1);",
            )
            .unwrap();
        init_drive_schema(&legacy).unwrap();
        let migrated = drive_pending_by_id(&legacy, "legacy-pending")
            .unwrap()
            .unwrap();
        assert_eq!(migrated.format_version, 1);
    }

    #[test]
    fn truncated_write_ranges_do_not_reveal_discarded_bytes_after_extension() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let pending_id = "0123456789abcdef0123456789abcdef";
        create_drive_pending_file(
            &conn,
            pending_id,
            Some("channel-abc"),
            "truncation-test.txt",
            "/tmp/qa-drive-truncation-marker",
        )
        .unwrap();

        record_drive_pending_range(&conn, pending_id, 0, 10).unwrap();
        update_drive_pending_size(&conn, pending_id, 10).unwrap();
        clear_drive_pending_ranges_for_span(&conn, pending_id, 4, 10).unwrap();
        update_drive_pending_size(&conn, pending_id, 4).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 4)]
        );

        update_drive_pending_size(&conn, pending_id, 9).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 4)],
            "Growing a previously truncated file must not resurrect deleted bytes"
        );

        record_drive_pending_range(&conn, pending_id, 7, 9).unwrap();
        clear_drive_pending_ranges_for_span(&conn, pending_id, 2, 8).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 2), (8, 9)],
            "Partial truncation must preserve only still-valid dirty ranges"
        );
    }

    #[test]
    fn drive_snapshot_does_not_auto_import_new_user_channels() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('user-channel', 101, 'Personal', 'owner', 1, 1, 1);",
        )
        .unwrap();

        let snapshot = drive_snapshot(&conn).unwrap();
        assert!(snapshot.directories.is_empty());
        assert!(snapshot.files.is_empty());
    }

    #[test]
    fn drive_storage_channels_are_never_imported_as_virtual_folders() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('drive-internal', 202, 'TeraRelay Drive Storage', 'drive_storage', 1, 1, 1);",
        )
        .unwrap();

        assert!(!ensure_imported_layout(&conn).unwrap());
        assert!(drive_snapshot(&conn).unwrap().directories.is_empty());
    }

    #[test]
    fn new_pending_file_is_unassigned_until_drive_storage_is_provisioned() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('user-channel', 101, 'Personal', 'owner', 1, 1, 1);",
        )
        .unwrap();
        let folder = create_drive_directory(&conn, None, "Projects").unwrap();
        let pending = create_drive_pending_file(
            &conn,
            "00112233445566778899aabbccddeeff",
            Some(&folder.id),
            "archive.bin",
            "/tmp/terarelay-drive-test-pending",
        )
        .unwrap();
        assert_eq!(folder.backing_logical_channel_id, None);
        assert_eq!(pending.backing_channel_id, 0);
    }

    #[test]
    fn pending_parts_round_trip_and_invalidation_is_part_aligned() {
        let conn = memory_db();
        upsert_drive_pending_part(
            &conn,
            "pending-a",
            0,
            0,
            15,
            202,
            5001,
            240_000_000,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        upsert_drive_pending_part(
            &conn,
            "pending-a",
            1,
            15,
            3,
            303,
            5002,
            48_000_000,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();

        let parts = drive_pending_parts(&conn, "pending-a").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].first_chunk_index, 0);
        assert_eq!(parts[0].chunk_count, 15);
        assert_eq!(parts[0].backing_channel_id, 202);
        assert_eq!(parts[1].backing_channel_id, 303);
        assert_eq!(parts[1].message_id, 5002);

        let removed = take_drive_pending_parts_from_chunk(&conn, "pending-a", 16).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].part_index, 1);
        assert_eq!(drive_pending_parts(&conn, "pending-a").unwrap().len(), 1);

        let removed = take_drive_pending_parts_from_chunk(&conn, "pending-a", 7).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(drive_pending_parts(&conn, "pending-a").unwrap().is_empty());
    }

    #[test]
    fn pending_part_tail_pruning_keeps_boundary_part_as_rewrite_source() {
        let conn = memory_db();
        for (part_index, first_chunk, channel_id, message_id) in [
            (0u64, 0u64, 202i64, 5001i64),
            (1, 15, 303, 5002),
            (2, 30, 404, 5003),
        ] {
            upsert_drive_pending_part(
                &conn,
                "pending-tail",
                part_index,
                first_chunk,
                15,
                channel_id,
                message_id,
                240_000_000,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap();
        }

        let removed = take_drive_pending_parts_starting_at_chunk(&conn, "pending-tail", 6).unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0].part_index, 1);
        assert_eq!(removed[1].part_index, 2);
        let remaining = drive_pending_parts(&conn, "pending-tail").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].part_index, 0);
        assert_eq!(remaining[0].message_id, 5001);
    }

    #[test]
    fn v2_finalization_publishes_cross_channel_parts_only_when_complete() {
        let conn = memory_db();
        let pending = create_drive_pending_file(
            &conn,
            "11112222333344445555666677778888",
            None,
            "ten-tb-style.bin",
            "/tmp/terarelay-drive-v2-finalize",
        )
        .unwrap();
        let block = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
        update_drive_pending_size(&conn, &pending.id, 16 * block).unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
        for chunk_index in 0..16u64 {
            let message_id = if chunk_index < 15 { 5001 } else { 5002 };
            upsert_drive_pending_chunk(
                &conn,
                &pending.id,
                chunk_index,
                message_id,
                100,
                &format!("{:064x}", chunk_index + 1),
                &format!("{:064x}", chunk_index + 101),
            )
            .unwrap();
        }
        upsert_drive_pending_part(
            &conn,
            &pending.id,
            0,
            0,
            15,
            202,
            5001,
            1500,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        upsert_drive_pending_part(
            &conn,
            &pending.id,
            1,
            15,
            1,
            303,
            5002,
            100,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();

        let result = finalize_drive_v2_metadata(
            &conn,
            &pending.id,
            "aaaaaaaa11111111bbbbbbbb22222222",
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        assert!(!result.reused_existing_object);
        assert_eq!(result.object_id, "aaaaaaaa11111111bbbbbbbb22222222");

        let mut parts = conn
            .prepare(
                "SELECT part_index, backing_channel_id, message_id FROM drive_object_parts
                 WHERE object_id = ? ORDER BY part_index",
            )
            .unwrap();
        parts.bind((1, result.object_id.as_str())).unwrap();
        assert!(matches!(parts.next().unwrap(), sqlite::State::Row));
        assert_eq!(parts.read::<i64, _>("backing_channel_id").unwrap(), 202);
        assert_eq!(parts.read::<i64, _>("message_id").unwrap(), 5001);
        assert!(matches!(parts.next().unwrap(), sqlite::State::Row));
        assert_eq!(parts.read::<i64, _>("backing_channel_id").unwrap(), 303);
        assert_eq!(parts.read::<i64, _>("message_id").unwrap(), 5002);
        assert!(matches!(parts.next().unwrap(), sqlite::State::Done));

        let mut blocks = conn
            .prepare("SELECT count(*) FROM drive_object_blocks WHERE object_id = ?")
            .unwrap();
        blocks.bind((1, result.object_id.as_str())).unwrap();
        blocks.next().unwrap();
        assert_eq!(blocks.read::<i64, _>(0).unwrap(), 16);

        let (block14, part0) = drive_object_block_location(&conn, &result.object_id, 14)
            .unwrap()
            .unwrap();
        assert_eq!(block14.part_index, 0);
        assert_eq!(block14.part_offset, 1400);
        assert_eq!(part0.backing_channel_id, 202);
        assert_eq!(part0.message_id, 5001);
        let (block15, part1) = drive_object_block_location(&conn, &result.object_id, 15)
            .unwrap()
            .unwrap();
        assert_eq!(block15.part_index, 1);
        assert_eq!(block15.part_offset, 0);
        assert_eq!(part1.backing_channel_id, 303);
        assert_eq!(part1.message_id, 5002);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
    }

    #[test]
    fn v2_finalization_refuses_incomplete_pending_object() {
        let conn = memory_db();
        let pending = create_drive_pending_file(
            &conn,
            "99990000aaaabbbbccccddddeeeeffff",
            None,
            "incomplete.bin",
            "/tmp/terarelay-drive-v2-incomplete",
        )
        .unwrap();
        update_drive_pending_size(&conn, &pending.id, 1).unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        assert!(finalize_drive_v2_metadata(
            &conn,
            &pending.id,
            "dddddddd11111111eeeeeeee22222222",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )
        .is_err());
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
    }

    #[test]
    fn legacy_and_v2_entries_coexist_in_one_snapshot_without_channel_import_side_effects() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('legacy-channel', 101, 'Legacy', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('legacy-file', 'legacy-channel', 99, 'old.bin', 7,
                     'application/octet-stream', 1, NULL, 1, 2, 'complete');
             INSERT INTO drive_file_entries
             (entry_id, file_id, directory_id, display_name, created_at, updated_at,
              trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('legacy-entry', 'legacy-file', NULL, 'old.bin', 2, 2,
                     NULL, 0, 7, NULL, NULL);",
        )
        .unwrap();

        let pending = create_drive_pending_file(
            &conn,
            "abab1111cdcd2222efef333344445555",
            None,
            "new.bin",
            "/tmp/terarelay-drive-v2-coexist",
        )
        .unwrap();
        update_drive_pending_size(&conn, &pending.id, 1).unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
        upsert_drive_pending_chunk(
            &conn,
            &pending.id,
            0,
            8101,
            1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        upsert_drive_pending_part(
            &conn,
            &pending.id,
            0,
            0,
            1,
            202,
            8101,
            1,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        finalize_drive_v2_metadata(
            &conn,
            &pending.id,
            "12121212343434345656565678787878",
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        )
        .unwrap();

        // V2 names participate in the same namespace as legacy entries.
        assert!(rename_drive_file(&conn, &pending.id, None, "old.bin").is_err());
        let moved = create_drive_directory(&conn, None, "Moved").unwrap();
        rename_drive_file(&conn, &pending.id, Some(&moved.id), "renamed.bin").unwrap();
        let part_before = drive_object_parts(&conn, "12121212343434345656565678787878")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.files.len(), 2);
        let legacy = snapshot
            .files
            .iter()
            .find(|file| file.entry_id == "legacy-entry")
            .unwrap();
        let v2 = snapshot
            .files
            .iter()
            .find(|file| file.entry_id == pending.id)
            .unwrap();
        assert_eq!(legacy.storage_version, 1);
        assert_eq!(v2.storage_version, 2);
        assert_eq!(v2.display_name, "renamed.bin");
        assert_eq!(v2.directory_id.as_deref(), Some(moved.id.as_str()));
        assert_eq!(v2.backing_channel_id, 202);
        assert_eq!(v2.first_message_id, 8101);
        let part_after = drive_object_parts(&conn, &v2.file_id)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(part_after, part_before);
    }

    #[test]
    fn v2_copy_on_write_pending_preserves_old_object_until_replacement_finalizes_or_cancels() {
        let conn = memory_db();
        let object_id = "eeeeeeee11111111ffffffff22222222";
        insert_v2_test_file(
            &conn,
            "v2-edit-entry",
            object_id,
            None,
            "editable.bin",
            202,
            8201,
        );
        let part_before = drive_object_parts(&conn, object_id).unwrap();

        let pending = create_drive_pending_replacement(
            &conn,
            "1234567890abcdef1234567890abcdef",
            "v2-edit-entry",
            "/tmp/terarelay-drive-v2-edit",
        )
        .unwrap();
        assert_eq!(pending.base_entry_id.as_deref(), Some("v2-edit-entry"));
        assert_eq!(pending.size, 1);
        assert_eq!(pending.backing_channel_id, 0);
        assert_eq!(drive_object_parts(&conn, object_id).unwrap(), part_before);

        let snapshot = drive_snapshot(&conn).unwrap();
        assert!(snapshot
            .files
            .iter()
            .all(|file| file.entry_id != "v2-edit-entry"));
        assert!(snapshot.pending.iter().any(|item| item.id == pending.id));

        delete_drive_pending(&conn, &pending.id).unwrap();
        let restored = drive_file_by_entry_id(&conn, "v2-edit-entry")
            .unwrap()
            .unwrap();
        assert_eq!(restored.storage_version, 2);
        assert_eq!(restored.file_id, object_id);
        assert_eq!(drive_object_parts(&conn, object_id).unwrap(), part_before);
    }

    #[test]
    fn v2_trash_restore_collision_keeps_object_and_chooses_safe_name() {
        let conn = memory_db();
        let original_object = "abababab11111111cdcdcdcd22222222";
        insert_v2_test_file(
            &conn,
            "v2-trash-entry",
            original_object,
            None,
            "report.txt",
            202,
            8301,
        );
        let part_before = drive_object_parts(&conn, original_object).unwrap();

        trash_drive_file(&conn, "v2-trash-entry").unwrap();
        let trash = list_drive_trash(&conn).unwrap();
        let item = trash
            .iter()
            .find(|item| item.id == "v2-trash-entry")
            .expect("V2 file should appear in Drive trash");
        assert!(item.restorable);
        assert_eq!(item.size, Some(1));

        insert_v2_test_file(
            &conn,
            "v2-collision-entry",
            "34343434565656567878787890909090",
            None,
            "report.txt",
            202,
            8302,
        );
        let restored_name = restore_drive_trash_item(&conn, "file", "v2-trash-entry").unwrap();
        assert_eq!(restored_name, "report (restored).txt");

        let restored = drive_file_by_entry_id(&conn, "v2-trash-entry")
            .unwrap()
            .unwrap();
        assert_eq!(restored.display_name, restored_name);
        assert_eq!(restored.file_id, original_object);
        assert_eq!(
            drive_object_parts(&conn, original_object).unwrap(),
            part_before
        );
    }

    #[test]
    fn v2_file_blocks_directory_trash_as_nonempty() {
        let conn = memory_db();
        let folder = create_drive_directory(&conn, None, "Projects").unwrap();
        insert_v2_test_file(
            &conn,
            "v2-folder-entry",
            "565656567878787890909090abababab",
            Some(&folder.id),
            "inside.bin",
            202,
            8401,
        );
        assert_eq!(
            trash_drive_directory(&conn, &folder.id).unwrap_err(),
            "Folder is not empty"
        );
        assert!(drive_snapshot(&conn)
            .unwrap()
            .directories
            .iter()
            .any(|directory| directory.id == folder.id));
    }

    #[test]
    fn v2_finalization_reuses_verified_duplicate_object_and_returns_only_temp_parts_for_cleanup() {
        let conn = memory_db();
        let fingerprint = "abababababababababababababababababababababababababababababababab";
        let mut finalized_object = String::new();
        for (ordinal, pending_id, proposed_object, message_id) in [
            (
                0u64,
                "12121212343434345656565678787878",
                "01010101232323234545454567676767",
                6101i64,
            ),
            (
                1u64,
                "90909090ababababcdcdcdcdefefefef",
                "11111111333333335555555577777777",
                7101i64,
            ),
        ] {
            let pending = create_drive_pending_file(
                &conn,
                pending_id,
                None,
                &format!("duplicate-{ordinal}.bin"),
                &format!("/tmp/terarelay-drive-v2-dedup-{ordinal}"),
            )
            .unwrap();
            update_drive_pending_size(&conn, &pending.id, 1).unwrap();
            mark_drive_pending_closed(&conn, &pending.id).unwrap();
            assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
            upsert_drive_pending_chunk(
                &conn,
                &pending.id,
                0,
                message_id,
                1,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap();
            upsert_drive_pending_part(
                &conn,
                &pending.id,
                0,
                0,
                1,
                202,
                message_id,
                1,
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            )
            .unwrap();
            let result =
                finalize_drive_v2_metadata(&conn, &pending.id, proposed_object, fingerprint)
                    .unwrap();
            if ordinal == 0 {
                assert!(!result.reused_existing_object);
                assert!(result.cleanup_parts.is_empty());
                finalized_object = result.object_id;
            } else {
                assert!(result.reused_existing_object);
                assert_eq!(result.object_id, finalized_object);
                assert_eq!(result.cleanup_parts.len(), 1);
                assert_eq!(result.cleanup_parts[0].message_id, message_id);
            }
        }
        let mut objects = conn.prepare("SELECT count(*) FROM drive_objects").unwrap();
        objects.next().unwrap();
        assert_eq!(objects.read::<i64, _>(0).unwrap(), 1);
        let mut entries = conn
            .prepare("SELECT count(*) FROM drive_entries_v2")
            .unwrap();
        entries.next().unwrap();
        assert_eq!(entries.read::<i64, _>(0).unwrap(), 2);
    }

    #[test]
    fn pending_backing_channel_assignment_is_one_way() {
        let conn = memory_db();
        let pending = create_drive_pending_file(
            &conn,
            "ffeeddccbbaa99887766554433221100",
            None,
            "huge-backup.img",
            "/tmp/terarelay-drive-assignment-test",
        )
        .unwrap();
        assert_eq!(pending.backing_channel_id, 0);

        let assigned = assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
        assert_eq!(assigned.backing_channel_id, 202);
        assert_eq!(
            assign_drive_pending_backing_channel(&conn, &pending.id, 202)
                .unwrap()
                .backing_channel_id,
            202
        );
        assert!(assign_drive_pending_backing_channel(&conn, &pending.id, 203).is_err());
    }

    #[test]
    fn imported_channels_become_normal_top_level_drive_folders() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123, 'video/x-matroska', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();

        assert!(ensure_imported_layout(&conn).unwrap());
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.directories.len(), 1);
        assert_eq!(snapshot.directories[0].name, "Movies");
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].display_name, "Dune.mkv");
        assert_eq!(
            snapshot.files[0].directory_id.as_deref(),
            Some("channel-abc")
        );
    }

    #[test]
    fn folders_are_virtual_and_do_not_create_new_telegram_channels() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let directory = create_drive_directory(&conn, None, "Photos").unwrap();
        assert_eq!(directory.backing_logical_channel_id, None);

        let mut channels = conn
            .prepare("SELECT count(*) FROM logical_channels")
            .unwrap();
        channels.next().unwrap();
        assert_eq!(channels.read::<i64, _>(0).unwrap(), 1);
    }

    #[test]
    fn renames_and_moves_are_metadata_only() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123, 'video/x-matroska', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let sci_fi = create_drive_directory(&conn, None, "Sci-Fi").unwrap();
        rename_drive_file(&conn, "import-file1", Some(&sci_fi.id), "Dune Part Two.mkv").unwrap();

        let snapshot = drive_snapshot(&conn).unwrap();
        let file = snapshot
            .files
            .iter()
            .find(|file| file.file_id == "file1")
            .unwrap();
        assert_eq!(file.display_name, "Dune Part Two.mkv");
        assert_eq!(file.directory_id.as_deref(), Some(sci_fi.id.as_str()));

        let mut source = conn
            .prepare("SELECT display_name, logical_channel_id FROM logical_files WHERE file_id = 'file1'")
            .unwrap();
        source.next().unwrap();
        assert_eq!(source.read::<String, _>(0).unwrap(), "Dune.mkv");
        assert_eq!(source.read::<String, _>(1).unwrap(), "abc");
    }

    #[test]
    fn trashed_drive_file_restores_without_deleting_telegram_content() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        trash_drive_file(&conn, "import-file1").unwrap();
        assert!(drive_snapshot(&conn).unwrap().files.is_empty());
        assert_eq!(
            restore_drive_trash_item(&conn, "file", "import-file1").unwrap(),
            "Dune.mkv"
        );
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].entry_id, "import-file1");
        assert_eq!(snapshot.files[0].file_id, "file1");
        let mut source = conn
            .prepare("SELECT status FROM logical_files WHERE file_id = 'file1'")
            .unwrap();
        assert!(matches!(source.next().unwrap(), sqlite::State::Row));
        assert_eq!(source.read::<String, _>(0).unwrap(), "complete");
    }

    #[test]
    fn recently_deleted_marks_missing_backing_files_as_unrestorable() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Available.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete'),
                    ('file2', 'abc', 100, 'Missing.mkv', 234,
                     'video/x-matroska', 1, NULL, 1, 3, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        trash_drive_file(&conn, "import-file1").unwrap();
        trash_drive_file(&conn, "import-file2").unwrap();
        conn.execute("UPDATE logical_files SET status = 'missing' WHERE file_id = 'file2'")
            .unwrap();

        let trash = list_drive_trash(&conn).unwrap();
        let available = trash
            .iter()
            .find(|item| item.name == "Available.mkv")
            .unwrap();
        let missing = trash
            .iter()
            .find(|item| item.name == "Missing.mkv")
            .unwrap();
        assert!(available.restorable);
        assert!(!missing.restorable);
        assert!(restore_drive_trash_item(&conn, "file", &missing.id).is_err());
        assert_eq!(
            restore_drive_trash_item(&conn, "file", &available.id).unwrap(),
            "Available.mkv"
        );
    }

    #[test]
    fn restoring_a_trashed_drive_file_never_overwrites_a_newer_name_collision() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete'),
                    ('file2', 'abc', 100, 'Dune (restored).mkv', 234,
                     'video/x-matroska', 1, NULL, 1, 3, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        // A separate item takes the original name while the old one is gone.
        trash_drive_file(&conn, "import-file1").unwrap();
        rename_drive_file(&conn, "import-file2", Some("channel-abc"), "Dune.mkv").unwrap();
        let restored = restore_drive_trash_item(&conn, "file", "import-file1").unwrap();
        assert_eq!(restored, "Dune (restored).mkv");
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|f| f.entry_id == "import-file2")
                .unwrap()
                .display_name,
            "Dune.mkv"
        );
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|f| f.entry_id == "import-file1")
                .unwrap()
                .display_name,
            "Dune (restored).mkv"
        );
    }

    #[test]
    fn trashed_empty_folder_restores_at_root_if_its_parent_was_removed() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let parent = create_drive_directory(&conn, None, "Projects").unwrap();
        let child = create_drive_directory(&conn, Some(&parent.id), "Videos").unwrap();
        trash_drive_directory(&conn, &child.id).unwrap();
        trash_drive_directory(&conn, &parent.id).unwrap();
        assert_eq!(
            restore_drive_trash_item(&conn, "folder", &child.id).unwrap(),
            "Videos"
        );
        let snapshot = drive_snapshot(&conn).unwrap();
        let restored = snapshot
            .directories
            .iter()
            .find(|d| d.id == child.id)
            .unwrap();
        assert_eq!(restored.parent_id, None);
    }

    #[test]
    fn empty_folder_can_be_removed_after_backing_file_becomes_unavailable() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123, 'video/x-matroska', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();

        let folder = create_drive_directory(&conn, None, "Temporary").unwrap();
        rename_drive_file(&conn, "import-file1", Some(&folder.id), "Dune.mkv").unwrap();

        // Logical-file deletion/reconciliation can make a Drive entry
        // unrenderable without first touching the user's virtual layout.
        conn.execute("UPDATE logical_files SET status = 'missing' WHERE file_id = 'file1';")
            .unwrap();
        let snapshot = drive_snapshot(&conn).unwrap();
        assert!(snapshot.files.iter().all(|file| file.file_id != "file1"));

        trash_drive_directory(&conn, &folder.id).unwrap();
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .all(|directory| directory.id != folder.id));
        assert!(load_entries(&conn)
            .unwrap()
            .iter()
            .all(|entry| entry.file_id != "file1"));
        build_manifest(&conn).unwrap();
    }

    #[test]
    fn duplicate_content_can_have_two_independent_drive_entries() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, 'Dune.mkv', 123, 'video/x-matroska', 1, 'hash', 1, 2, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();

        let copies = create_drive_directory(&conn, None, "Copies").unwrap();
        let pending_id = "0123456789abcdef0123456789abcdef";
        let pending = create_drive_pending_file(
            &conn,
            pending_id,
            Some(&copies.id),
            "Dune Copy.mkv",
            "/tmp/drive-copy",
        )
        .unwrap();
        assert_eq!(pending.id, pending_id);

        let now = now_ms();
        conn.execute(
            "DELETE FROM drive_pending_files WHERE id = '0123456789abcdef0123456789abcdef'",
        )
        .unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                  trashed_at, encryption_version, plaintext_size, crypto_id)
                 VALUES (?, 'file1', ?, 'Dune Copy.mkv', ?, ?, NULL, 0, NULL, NULL)",
            )
            .unwrap();
        insert.bind((1, pending_id)).unwrap();
        insert.bind((2, copies.id.as_str())).unwrap();
        insert.bind((3, now)).unwrap();
        insert.bind((4, now)).unwrap();
        insert.next().unwrap();

        let snapshot = drive_snapshot(&conn).unwrap();
        let refs: Vec<_> = snapshot
            .files
            .iter()
            .filter(|file| file.file_id == "file1")
            .collect();
        assert_eq!(refs.len(), 2);
        assert_ne!(refs[0].entry_id, refs[1].entry_id);
    }

    #[test]
    fn legacy_v1_manifest_without_encryption_fields_still_decodes() {
        let raw = serde_json::json!({
            "schema_version": LEGACY_DRIVE_SCHEMA_VERSION,
            "revision": 7,
            "directories": [],
            "entries": [{
                "entry_id": "legacy-entry",
                "file_id": "legacy-file",
                "directory_id": null,
                "display_name": "legacy.txt",
                "created_at": 1,
                "updated_at": 2,
                "trashed_at": null
            }]
        });
        let manifest: DriveManifestV1 = serde_json::from_value(raw).unwrap();
        assert_eq!(manifest.entries[0].encryption_version, 0);
        assert_eq!(manifest.entries[0].plaintext_size, None);
        assert_eq!(manifest.entries[0].crypto_id, None);
        assert_eq!(manifest.entries[0].content_fingerprint, None);
        validate_manifest(&manifest).unwrap();
    }

    #[test]
    fn encrypted_dedup_reuses_the_existing_ciphertext_crypto_identity() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Files', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('file1', 'abc', 99, '.terarelay-drive-file-file1.blob', 64,
                     'application/octet-stream', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        let fingerprint = "a".repeat(64);
        let now = now_ms();
        let mut existing = conn
            .prepare(
                "INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                  trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
                 VALUES ('existing-entry', 'file1', NULL, 'existing.bin', ?, ?,
                         NULL, 1, 3, 'existing-crypto', ?)",
            )
            .unwrap();
        existing.bind((1, now)).unwrap();
        existing.bind((2, now)).unwrap();
        existing.bind((3, fingerprint.as_str())).unwrap();
        existing.next().unwrap();

        let found = find_drive_content_match(&conn, &fingerprint, 3, 1)
            .unwrap()
            .unwrap();
        assert_eq!(found.file_id, "file1");
        assert_eq!(found.crypto_id.as_deref(), Some("existing-crypto"));

        let pending_id = "fedcba9876543210fedcba9876543210";
        let mut pending = conn
            .prepare(
                "INSERT INTO drive_pending_files
                 (id, directory_id, display_name, staging_path, size,
                  backing_channel_id, base_entry_id, encryption_version, crypto_id,
                  closed_at, created_at, updated_at)
                 VALUES (?, NULL, 'copy.bin', '/tmp/dedupe-copy', 3,
                         101, NULL, 1, 'new-crypto', ?, ?, ?)",
            )
            .unwrap();
        pending.bind((1, pending_id)).unwrap();
        pending.bind((2, now)).unwrap();
        pending.bind((3, now)).unwrap();
        pending.bind((4, now)).unwrap();
        pending.next().unwrap();

        finalize_drive_streamed_pending(
            &conn,
            pending_id,
            &found.file_id,
            3,
            Some(&fingerprint),
            found.crypto_id.as_deref(),
        )
        .unwrap();

        let mut result = conn
            .prepare(
                "SELECT crypto_id, file_id
                 FROM drive_file_entries
                 WHERE entry_id = ?",
            )
            .unwrap();
        result.bind((1, pending_id)).unwrap();
        assert!(matches!(result.next().unwrap(), sqlite::State::Row));
        assert_eq!(
            result
                .read::<Option<String>, _>("crypto_id")
                .unwrap()
                .as_deref(),
            Some("existing-crypto")
        );
        assert_eq!(result.read::<String, _>("file_id").unwrap(), "file1");
    }

    #[test]
    fn concurrent_encryption_keys_never_silently_replace_one_another() {
        let envelope = crate::drive_crypto::DriveCryptoEnvelope {
            version: crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            updated_at: 10,
            salt_b64: "salt".into(),
            nonce_b64: "nonce".into(),
            wrapped_master_key_b64: "wrapped-a".into(),
            key_check_b64: Some("master-key-a".into()),
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        };
        let first = DriveManifestV1 {
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 10,
            crypto: Some(envelope.clone()),
            directories: vec![],
            entries: vec![],
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        let mut second = first.clone();
        second.revision = 11;
        second.crypto.as_mut().unwrap().updated_at = 11;
        second.crypto.as_mut().unwrap().key_check_b64 = Some("master-key-b".into());
        assert!(merge_drive_manifests(&first, &second)
            .unwrap_err()
            .contains("Conflicting TeraRelay Drive encryption keys"));

        // Changing the passphrase only rotates the wrapper, not the
        // underlying master key. This should still merge normally.
        second.crypto.as_mut().unwrap().key_check_b64 = Some("master-key-a".into());
        second.crypto.as_mut().unwrap().wrapped_master_key_b64 = "wrapped-new".into();
        let merged = merge_drive_manifests(&first, &second).unwrap();
        assert_eq!(merged.crypto.unwrap().wrapped_master_key_b64, "wrapped-new");

        // Older metadata without a verifier cannot prove two independently
        // wrapped keys represent the same master key. Reject ambiguity.
        let mut legacy = first.clone();
        legacy.crypto.as_mut().unwrap().key_check_b64 = None;
        assert!(merge_drive_manifests(&legacy, &second).is_err());
    }

    #[test]
    fn remote_manifest_cannot_erase_or_replace_existing_encryption_key() {
        let conn = memory_db();
        let local = crate::drive_crypto::DriveCryptoEnvelope {
            version: crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            updated_at: 10,
            salt_b64: "salt".into(),
            nonce_b64: "nonce".into(),
            wrapped_master_key_b64: "wrapped-a".into(),
            key_check_b64: Some("master-a".into()),
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        };
        crate::drive_crypto::save_crypto_envelope(&conn, Some(&local)).unwrap();
        let mut incoming = DriveManifestV1 {
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 50,
            crypto: None,
            directories: vec![],
            entries: vec![],
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        assert!(apply_manifest(&conn, &incoming)
            .unwrap_err()
            .contains("Refusing to erase"));
        assert_eq!(
            crate::drive_crypto::load_crypto_envelope(&conn).unwrap(),
            Some(local.clone())
        );

        let mut incompatible = local.clone();
        incompatible.key_check_b64 = Some("master-b".into());
        incoming.crypto = Some(incompatible);
        assert!(apply_manifest(&conn, &incoming)
            .unwrap_err()
            .contains("does not match"));
        assert_eq!(
            crate::drive_crypto::load_crypto_envelope(&conn).unwrap(),
            Some(local.clone())
        );

        let mut rotated_passphrase = local.clone();
        rotated_passphrase.wrapped_master_key_b64 = "new-wrap".into();
        incoming.crypto = Some(rotated_passphrase.clone());
        apply_manifest(&conn, &incoming).unwrap();
        assert_eq!(
            crate::drive_crypto::load_crypto_envelope(&conn).unwrap(),
            Some(rotated_passphrase)
        );
    }

    #[test]
    fn legacy_schema_two_manifest_decodes_with_empty_storage_pool_records() {
        let raw = serde_json::json!({
            "schema_version": 2,
            "revision": 12,
            "crypto": null,
            "directories": [],
            "entries": []
        });
        let manifest: DriveManifestV1 = serde_json::from_value(raw).unwrap();
        assert!(manifest.storage_channels.is_empty());
        assert!(manifest.objects.is_empty());
        assert!(manifest.parts.is_empty());
        assert!(manifest.v2_entries.is_empty());
        validate_manifest(&manifest).unwrap();
    }

    #[test]
    fn storage_pool_manifest_round_trip_reconstructs_v2_file_on_another_device() {
        let source = memory_db();
        crate::drive_storage::init_drive_storage_schema(&source).unwrap();
        crate::drive_storage::register_storage_channel(
            &source,
            1,
            202,
            "TeraRelay Drive Storage",
            "active",
            None,
        )
        .unwrap();
        let object_id = "12121212343434345656565678787878";
        insert_v2_test_file(
            &source,
            "synced-v2-entry",
            object_id,
            None,
            "synced.bin",
            202,
            9101,
        );
        let manifest = build_manifest(&source).unwrap();
        assert_eq!(manifest.storage_channels.len(), 1);
        assert_eq!(manifest.objects.len(), 1);
        assert_eq!(manifest.parts.len(), 1);
        assert_eq!(manifest.v2_entries.len(), 1);

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let decoded: DriveManifestV1 = serde_json::from_slice(&bytes).unwrap();
        let target = memory_db();
        apply_manifest(&target, &decoded).unwrap();

        let restored = drive_file_by_entry_id(&target, "synced-v2-entry")
            .unwrap()
            .unwrap();
        assert_eq!(restored.storage_version, 2);
        assert_eq!(restored.file_id, object_id);
        let (restored_block, restored_part) = drive_object_block_location(&target, object_id, 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            restored_block.sha256,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(restored_part.backing_channel_id, 202);
        assert_eq!(restored_part.message_id, 9101);
        assert_eq!(
            crate::drive_storage::active_storage_channel(&target)
                .unwrap()
                .unwrap()
                .backing_channel_id,
            202
        );
    }

    #[test]
    fn manifest_merge_cannot_erase_known_storage_channel_or_completed_v2_object() {
        let source = memory_db();
        crate::drive_storage::init_drive_storage_schema(&source).unwrap();
        crate::drive_storage::register_storage_channel(
            &source,
            1,
            202,
            "TeraRelay Drive Storage",
            "active",
            None,
        )
        .unwrap();
        insert_v2_test_file(
            &source,
            "merge-v2-entry",
            "90909090ababababcdcdcdcdefefefef",
            None,
            "keep.bin",
            202,
            9201,
        );
        let left = build_manifest(&source).unwrap();
        let mut right = left.clone();
        right.revision += 10;
        right.storage_channels.clear();
        right.objects.clear();
        right.parts.clear();
        right.v2_entries.clear();

        let merged = merge_drive_manifests(&left, &right).unwrap();
        assert_eq!(merged.storage_channels.len(), 1);
        assert_eq!(merged.objects.len(), 1);
        assert_eq!(merged.parts.len(), 1);
        assert_eq!(merged.v2_entries.len(), 1);
    }

    #[test]
    fn manifest_rejects_missing_parent_references() {
        let manifest = DriveManifestV1 {
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 1,
            crypto: None,
            directories: vec![DriveDirectoryRecord {
                id: "child".into(),
                parent_id: Some("missing".into()),
                name: "Child".into(),
                backing_logical_channel_id: None,
                created_at: 1,
                updated_at: 1,
                trashed_at: None,
            }],
            entries: Vec::new(),
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        assert!(validate_manifest(&manifest).is_err());
    }
}
