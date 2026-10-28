use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use grammers_client::types::{Media, Peer};
use grammers_client::{Client, InputMessage};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};
use unicode_normalization::UnicodeNormalization;

#[cfg(test)]
std::thread_local! {
    static DRIVE_SCHEMA_INIT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_drive_schema_init_call() {
    DRIVE_SCHEMA_INIT_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_schema_init_call() {}

#[cfg(test)]
std::thread_local! {
    static DRIVE_STATE_ROW_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_drive_state_row_call() {
    DRIVE_STATE_ROW_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_state_row_call() {}

pub const DRIVE_SCHEMA_VERSION: u32 = 4;
const PRE_PROJECT_DRIVE_SCHEMA_VERSION: u32 = 3;
const LEGACY_DRIVE_SCHEMA_VERSION: u32 = 1;
const PRE_STORAGE_POOL_DRIVE_SCHEMA_VERSION: u32 = 2;
const DRIVE_MANIFEST_FILE: &str = ".terarelay-drive-index.json";
const DRIVE_MANIFEST_CAPTION: &str = "TRD1:index";
// V3 carries cross-device storage-pool part metadata. A 10 TB mounted file
// can contain tens of thousands of remote parts, so the old 4 MiB cap was too
// small even though the actual file bytes never live in this manifest.
const MAX_DRIVE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DRIVE_DIRECTORY_DEPTH: usize = 512;

#[cfg(debug_assertions)]
#[path = "drive_project_qa.rs"]
mod project_qa;
#[cfg(debug_assertions)]
pub use project_qa::{qa_export_manifest, qa_restore_manifest};

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
    /// Optional durable POSIX inode identity, distinct from immutable object ID
    /// and individual directory-entry ID. Currently no writer creates aliases.
    /// Keep absent in legacy schema-4 manifests until hardlink semantics and
    /// a new incompatible manifest version are fully supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode_id: Option<String>,
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
    #[serde(default)]
    pub symlinks: Vec<crate::drive_posix::DriveSymlinkRecord>,
    #[serde(default)]
    pub permissions: Vec<crate::drive_posix::DrivePermissionRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveFileRecord {
    pub entry_id: String,
    pub file_id: String,
    /// Durable shared-inode identity, distinct from immutable deduplicated
    /// object ID. Absent for all schema-4 files until full hardlinks ship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode_id: Option<String>,
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
    pub symlinks: Vec<crate::drive_posix::DriveSymlinkRecord>,
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

pub(crate) fn validate_drive_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("Drive name cannot be empty".to_string());
    }
    // The Drive namespace is shared by Linux, Windows and macOS. Keep names
    // portable at the metadata boundary instead of allowing one device to
    // create an item that another device cannot project into Explorer/Finder.
    if name.trim() != name {
        return Err("Drive name cannot start or end with whitespace".to_string());
    }
    if name == "." || name == ".." || name.ends_with('.') {
        return Err("Drive name is reserved".to_string());
    }
    if name.chars().any(|ch| {
        ch == '\0'
            || ch.is_control()
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err("Drive name contains a cross-platform invalid character".to_string());
    }
    let device_stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved_device = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || matches!(
            device_stem.as_str(),
            "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
                | "COM¹"
                | "COM²"
                | "COM³"
                | "LPT¹"
                | "LPT²"
                | "LPT³"
        );
    if reserved_device {
        return Err("Drive name is reserved by Windows".to_string());
    }
    if name.len() > 255 {
        return Err("Drive name is too long".to_string());
    }
    Ok(name.to_string())
}

pub fn init_drive_schema(conn: &sqlite::Connection) -> Result<(), String> {
    record_drive_schema_init_call();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            revision INTEGER NOT NULL DEFAULT 0,
            dirty INTEGER NOT NULL DEFAULT 0,
            last_remote_message_id INTEGER
        );
        INSERT OR IGNORE INTO drive_state(singleton, revision, dirty)
            VALUES (1, 0, 0);

        CREATE TABLE IF NOT EXISTS drive_provider_local_state (
            provider TEXT PRIMARY KEY,
            schema_version INTEGER NOT NULL,
            state_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );

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
            inode_id TEXT,
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
        (
            "drive_entries_v2",
            "inode_id",
            "ALTER TABLE drive_entries_v2 ADD COLUMN inode_id TEXT",
        ),
    ] {
        if !has_column(conn, table, column)? {
            conn.execute(sql).map_err(|e: sqlite::Error| {
                format!("Failed to migrate TeraRelay Drive {table}.{column}: {e}")
            })?;
        }
    }

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_drive_entries_v2_inode
         ON drive_entries_v2(inode_id, trashed_at)",
    )
    .map_err(|e: sqlite::Error| format!("Failed to index Drive inode identities: {e}"))?;
    crate::drive_crypto::init_crypto_schema(conn)?;
    crate::drive_posix::init_schema(conn)?;
    Ok(())
}

fn state_row(conn: &sqlite::Connection) -> Result<(i64, bool), String> {
    record_drive_state_row_call();
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
    let now = now_ms();
    let mut stmt = conn
        .prepare(
            "UPDATE drive_state
             SET revision = CASE
                 WHEN revision >= 9223372036854775807 THEN 9223372036854775807
                 ELSE max(revision + 1, ?)
             END,
             dirty = 1
             WHERE singleton = 1
             RETURNING revision",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, now)).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        stmt.read::<i64, _>(0).map_err(|e| e.to_string())
    } else {
        Err("Drive sync state row is missing; refusing to mark a mutation as synced".to_string())
    }
}

/// Reject network-era snapshots if another local Drive edit happened while
/// metadata sync was awaiting Telegram. The caller holds the DB mutex for
/// this check and the following manifest application, closing the TOCTOU gap.
fn ensure_drive_sync_snapshot_current(
    conn: &sqlite::Connection,
    expected_revision: i64,
    expected_dirty: bool,
) -> Result<(), String> {
    let (revision, dirty) = state_row(conn)?;
    if revision != expected_revision || dirty != expected_dirty {
        return Err(
            "Drive metadata changed during sync; preserving newer local edits for retry"
                .to_string(),
        );
    }
    Ok(())
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

pub(crate) fn portable_name_key(name: &str) -> String {
    name.nfc().flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
thread_local! {
    static DRIVE_NAME_COLLISION_QUERY_STEPS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_drive_name_collision_query_steps() {
    DRIVE_NAME_COLLISION_QUERY_STEPS.with(|steps| steps.set(0));
}

#[cfg(test)]
fn drive_name_collision_query_steps() -> u64 {
    DRIVE_NAME_COLLISION_QUERY_STEPS.with(|steps| steps.get())
}

#[cfg(test)]
fn record_drive_name_collision_query() {
    DRIVE_NAME_COLLISION_QUERY_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_name_collision_query() {}

#[cfg(test)]
thread_local! {
    static DRIVE_NATIVE_FILE_RENAME_NAMESPACE_QUERY_STEPS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_drive_native_file_rename_namespace_query_steps() {
    DRIVE_NATIVE_FILE_RENAME_NAMESPACE_QUERY_STEPS.with(|steps| steps.set(0));
}

#[cfg(test)]
fn drive_native_file_rename_namespace_query_steps() -> u64 {
    DRIVE_NATIVE_FILE_RENAME_NAMESPACE_QUERY_STEPS.with(|steps| steps.get())
}

#[cfg(test)]
fn record_drive_native_file_rename_namespace_query() {
    DRIVE_NATIVE_FILE_RENAME_NAMESPACE_QUERY_STEPS
        .with(|steps| steps.set(steps.get().saturating_add(1)));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_native_file_rename_namespace_query() {}

#[cfg(test)]
thread_local! {
    static DRIVE_RENAME_FILE_VERSION_PROBE_CALLS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_drive_rename_file_version_probe_calls() {
    DRIVE_RENAME_FILE_VERSION_PROBE_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn drive_rename_file_version_probe_calls() -> u64 {
    DRIVE_RENAME_FILE_VERSION_PROBE_CALLS.with(|calls| calls.get())
}

#[cfg(test)]
fn record_drive_rename_file_version_probe() {
    DRIVE_RENAME_FILE_VERSION_PROBE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_rename_file_version_probe() {}

#[cfg(test)]
thread_local! {
    static DRIVE_TRASH_FILE_VERSION_PROBE_CALLS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_drive_trash_file_version_probe_calls() {
    DRIVE_TRASH_FILE_VERSION_PROBE_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn drive_trash_file_version_probe_calls() -> u64 {
    DRIVE_TRASH_FILE_VERSION_PROBE_CALLS.with(|calls| calls.get())
}

#[cfg(test)]
fn record_drive_trash_file_version_probe() {
    DRIVE_TRASH_FILE_VERSION_PROBE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
}

#[cfg(not(test))]
#[inline(always)]
fn record_drive_trash_file_version_probe() {}

fn directory_name_exists(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    name: &str,
    ignore_dir_id: Option<&str>,
) -> Result<bool, String> {
    let wanted = portable_name_key(name);
    record_drive_name_collision_query();
    let mut stmt = conn
        .prepare(
            "SELECT id, name FROM drive_directories
             WHERE ((parent_id IS NULL AND ? IS NULL) OR parent_id = ?)
               AND trashed_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let id = stmt.read::<String, _>("id").map_err(|e| e.to_string())?;
        if ignore_dir_id == Some(id.as_str()) {
            continue;
        }
        let existing = stmt.read::<String, _>("name").map_err(|e| e.to_string())?;
        if portable_name_key(&existing) == wanted {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_name_exists(
    conn: &sqlite::Connection,
    directory_id: Option<&str>,
    name: &str,
    ignore_entry_id: Option<&str>,
    ignore_pending_id: Option<&str>,
) -> Result<bool, String> {
    let wanted = portable_name_key(name);
    for (table, id_column, name_column, trashed) in [
        ("drive_file_entries", "entry_id", "display_name", true),
        ("drive_entries_v2", "entry_id", "display_name", true),
        ("drive_symlinks", "id", "display_name", true),
        ("drive_pending_files", "id", "display_name", false),
    ] {
        record_drive_name_collision_query();
        let sql = match table {
            "drive_file_entries" => format!(
                "SELECT e.{id_column}, e.{name_column} FROM {table} e
                 WHERE ((e.directory_id IS NULL AND ? IS NULL) OR e.directory_id = ?)
                   AND e.trashed_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_entries_v2 shadow
                       JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                       WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                   )"
            ),
            "drive_entries_v2" => format!(
                "SELECT e.{id_column}, e.{name_column} FROM {table} e
                 JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE ((e.directory_id IS NULL AND ? IS NULL) OR e.directory_id = ?)
                   AND e.trashed_at IS NULL"
            ),
            _ if trashed => format!(
                "SELECT {id_column}, {name_column} FROM {table}
                 WHERE ((directory_id IS NULL AND ? IS NULL) OR directory_id = ?)
                   AND trashed_at IS NULL"
            ),
            _ => format!(
                "SELECT {id_column}, {name_column} FROM {table}
                 WHERE ((directory_id IS NULL AND ? IS NULL) OR directory_id = ?)"
            ),
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, directory_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, directory_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            let id = stmt
                .read::<String, _>(id_column)
                .map_err(|e| e.to_string())?;
            let ignored = if table == "drive_pending_files" {
                ignore_pending_id == Some(id.as_str())
            } else {
                ignore_entry_id == Some(id.as_str())
            };
            if ignored {
                continue;
            }
            let existing = stmt
                .read::<String, _>(name_column)
                .map_err(|e| e.to_string())?;
            if portable_name_key(&existing) == wanted {
                return Ok(true);
            }
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
    let wanted = portable_name_key(name);
    record_drive_name_collision_query();
    let mut stmt = conn
        .prepare(
            "SELECT 0 AS kind, id, name AS display_name
             FROM drive_directories
             WHERE ((parent_id IS NULL AND ?1 IS NULL) OR parent_id = ?2)
               AND trashed_at IS NULL
             UNION ALL
             SELECT 1 AS kind, entry_id AS id, display_name
             FROM drive_file_entries e
             WHERE ((e.directory_id IS NULL AND ?1 IS NULL) OR e.directory_id = ?2)
               AND e.trashed_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM drive_entries_v2 shadow
                   JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                   WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
               )
             UNION ALL
             SELECT 2 AS kind, e.entry_id AS id, e.display_name
             FROM drive_entries_v2 e
             JOIN drive_objects o ON o.object_id = e.object_id
             WHERE ((e.directory_id IS NULL AND ?1 IS NULL) OR e.directory_id = ?2)
               AND e.trashed_at IS NULL
             UNION ALL
             SELECT 3 AS kind, id, display_name
             FROM drive_symlinks
             WHERE ((directory_id IS NULL AND ?1 IS NULL) OR directory_id = ?2)
               AND trashed_at IS NULL
             UNION ALL
             SELECT 4 AS kind, id, display_name
             FROM drive_pending_files
             WHERE ((directory_id IS NULL AND ?1 IS NULL) OR directory_id = ?2)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, parent_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let kind = stmt.read::<i64, _>("kind").map_err(|e| e.to_string())?;
        let id = stmt.read::<String, _>("id").map_err(|e| e.to_string())?;
        let ignored = match kind {
            0 => ignore_dir_id == Some(id.as_str()),
            1..=3 => ignore_entry_id == Some(id.as_str()),
            4 => ignore_pending_id == Some(id.as_str()),
            _ => return Err("Invalid Drive namespace collision row".into()),
        };
        if ignored {
            continue;
        }
        let existing = stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?;
        if portable_name_key(&existing) == wanted {
            return Err(format!("A drive item named \"{name}\" already exists"));
        }
    }
    Ok(())
}

fn imported_directory_id(logical_channel_id: &str) -> String {
    format!("channel-{logical_channel_id}")
}

/// Pick an unambiguous portable name across *all* visible Drive node kinds.
/// The importer must not insert a duplicate directory/file/symlink name just
/// because it has a different SQLite table or the first fallback is occupied.
fn available_imported_name(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    requested: &str,
    stable_id: &str,
    file: bool,
) -> Result<String, String> {
    let taken = |candidate: &str| -> Result<bool, String> {
        Ok(directory_name_exists(conn, parent, candidate, None)?
            || file_name_exists(conn, parent, candidate, None, None)?)
    };
    if !taken(requested)? {
        return Ok(requested.to_string());
    }
    let suffix: String = stable_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(if file { 6 } else { 8 })
        .collect();
    if suffix.is_empty() {
        return Err("Imported Drive item has no usable stable identifier".to_string());
    }
    let (stem, extension) = if file {
        let path = std::path::Path::new(requested);
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!(".{extension}"))
            .unwrap_or_default();
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("file");
        (stem, extension)
    } else {
        (requested, String::new())
    };
    for index in 1..=10000usize {
        // Preserve existing imported names ("name id" for channels and
        // "name (fileid).ext" for files) to avoid cosmetic migration churn.
        let marker = match (file, index) {
            (true, 1) => format!(" ({suffix})"),
            (true, n) => format!(" ({suffix}-{n})"),
            (false, 1) => format!(" {suffix}"),
            (false, n) => format!(" {suffix}-{n}"),
        };
        let reserve = marker.len().saturating_add(extension.len());
        if reserve >= 255 {
            return Err("Imported Drive name suffix is too long".to_string());
        }
        let max_stem = 255 - reserve;
        let mut end = stem.len().min(max_stem);
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}{}{}", &stem[..end], marker, extension);
        if validate_drive_name(&candidate).is_ok() && !taken(&candidate)? {
            return Ok(candidate);
        }
    }
    Err("Could not choose a unique imported Drive name".into())
}

pub fn ensure_imported_layout(conn: &sqlite::Connection) -> Result<bool, String> {
    ensure_imported_layout_scoped(conn, None)
}

/// An ordinary completed upload only needs its own file and channel folder
/// imported. Scanning every channel/file per upload grows quadratically across
/// a large batch and is unnecessary; startup/recovery still uses the full pass.
fn ensure_imported_layout_scoped(
    conn: &sqlite::Connection,
    only_file_id: Option<&str>,
) -> Result<bool, String> {
    init_drive_schema(conn)?;
    // A failed sync revision must not leave newly imported folders/files
    // behind without a dirty marker. This savepoint can nest in callers.
    conn.execute("SAVEPOINT drive_imported_layout")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut changed = false;
        let now = now_ms();

        let mut channels = conn
            .prepare(
                "SELECT logical_id, name
             FROM logical_channels
             WHERE role != 'drive_storage'
               AND (?1 IS NULL OR logical_id = (
                   SELECT logical_channel_id FROM logical_files WHERE file_id = ?1
               ))
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        channels
            .bind((1, only_file_id))
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
                let requested = validate_drive_name(&channel_name)
                    .unwrap_or_else(|_| "TeraRelay Storage".to_string());
                let name = available_imported_name(conn, None, &requested, &logical_id, false)?;
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
                    "SELECT f.file_id, f.display_name, f.created_at
                 FROM logical_files AS f
                 WHERE f.logical_channel_id = ?1 AND f.status = 'complete'
                   AND f.display_name NOT LIKE '.terarelay-drive-file-%'
                   AND (?2 IS NULL OR f.file_id = ?2)
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_file_entries AS mapped
                       WHERE mapped.file_id = f.file_id
                   )",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            files
                .bind((1, logical_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            files
                .bind((2, only_file_id))
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
                // Already-mapped files were excluded in the indexed SQL query,
                // rather than pulled into memory and looked up one by one.
                let requested =
                    validate_drive_name(&file_name).unwrap_or_else(|_| "file".to_string());
                let name =
                    available_imported_name(conn, Some(&dir_id), &requested, &file_id, true)?;

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
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_imported_layout", result)
}

/// Committed upload notification: parent directory, visible filename,
/// optionally new top-level channel folder, and stable file-entry inode ID.
pub(crate) type CompletedDriveEntryNotification = (String, String, Option<String>, String);

/// Commit a completed Telegram manifest and its native Drive namespace entry
/// together. Previously the importer ran only at mount startup, so ordinary
/// uploads were invisible in an already-mounted Drive until app restart.
pub(crate) fn publish_completed_upload_to_drive(
    conn: &sqlite::Connection,
    manifest: &crate::commands::logical_files::LogicalFileManifestV1,
) -> Result<Option<CompletedDriveEntryNotification>, String> {
    conn.execute("SAVEPOINT drive_completed_upload_publish")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        // An ordinary upload can create the channel's top-level Drive folder.
        // Notify its parent (the Drive root), not only the uploaded file's
        // folder, when the channel appears for the first time.
        let dir_id = imported_directory_id(&manifest.logical_channel_id);
        let mut folder = conn
            .prepare("SELECT name FROM drive_directories WHERE id=?1 AND trashed_at IS NULL")
            .map_err(|e: sqlite::Error| e.to_string())?;
        folder
            .bind((1, dir_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        let old_folder_exists =
            folder.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row;
        drop(folder);
        crate::commands::logical_files::persist_manifest(conn, manifest)?;
        ensure_imported_layout_scoped(conn, Some(&manifest.file_id))?;
        let new_folder_name = if old_folder_exists {
            None
        } else {
            let mut folder = conn
                .prepare("SELECT name FROM drive_directories WHERE id=?1 AND trashed_at IS NULL")
                .map_err(|e: sqlite::Error| e.to_string())?;
            folder
                .bind((1, dir_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if folder.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
                Some(
                    folder
                        .read::<String, _>("name")
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            }
        };

        // Drive-backed files have a separate pending/finalization lifecycle.
        // They deliberately use a hidden backing manifest and have no imported
        // entry until their native namespace operation commits.
        let mut entry = conn
            .prepare(
                "SELECT entry_id, directory_id, display_name FROM drive_file_entries
                 WHERE file_id = ?1 AND trashed_at IS NULL LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry
            .bind((1, manifest.file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if entry.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
            return Err("Completed upload has no visible native Drive entry".into());
        }
        let parent = drive_nullable_text(&entry, "directory_id")?
            .ok_or("Completed upload Drive entry unexpectedly has no parent directory")?;
        let name = entry
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?;
        let entry_id = entry
            .read::<String, _>("entry_id")
            .map_err(|e| e.to_string())?;
        drop(entry);
        let mut active_parent = conn
            .prepare("SELECT 1 FROM drive_directories WHERE id=?1 AND trashed_at IS NULL")
            .map_err(|e: sqlite::Error| e.to_string())?;
        active_parent
            .bind((1, parent.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if active_parent
            .next()
            .map_err(|e: sqlite::Error| e.to_string())?
            != sqlite::State::Row
        {
            return Err("Completed upload targets a missing or trashed Drive directory".into());
        }
        Ok(Some((parent, name, new_folder_name, entry_id)))
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_completed_upload_publish", result)
}

/// Nullable persisted Drive fields must distinguish SQL NULL from malformed
/// SQLite values. Otherwise a corrupt parent or tombstone can silently become
/// a new root or a live item in the sync manifest.
fn drive_nullable_text(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<String>, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid Drive {field}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::String(value) => Ok(Some(value)),
        _ => Err(format!("Invalid Drive {field} type")),
    }
}

fn drive_nullable_integer(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<i64>, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid Drive {field}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::Integer(value) => Ok(Some(value)),
        _ => Err(format!("Invalid Drive {field} type")),
    }
}

fn drive_entry_encryption_version(stmt: &sqlite::Statement<'_>) -> Result<u32, String> {
    match stmt
        .read::<sqlite::Value, _>("encryption_version")
        .map_err(|e| format!("Invalid Drive file encryption version: {e}"))?
    {
        sqlite::Value::Integer(0) => Ok(0),
        sqlite::Value::Integer(1) => Ok(crate::drive_crypto::DRIVE_ENCRYPTION_VERSION),
        _ => Err("Invalid or unsupported Drive file encryption version".to_string()),
    }
}

fn drive_nullable_plaintext_size(stmt: &sqlite::Statement<'_>) -> Result<Option<u64>, String> {
    drive_nullable_integer(stmt, "plaintext_size")?
        .map(|value| u64::try_from(value).map_err(|_| "Invalid Drive plaintext size".to_string()))
        .transpose()
}

fn drive_required_plaintext_size(stmt: &sqlite::Statement<'_>) -> Result<u64, String> {
    drive_nullable_plaintext_size(stmt)?
        .ok_or_else(|| "Drive object plaintext size cannot be NULL".to_string())
}

fn drive_required_integer(stmt: &sqlite::Statement<'_>, field: &str) -> Result<i64, String> {
    drive_nullable_integer(stmt, field)?.ok_or_else(|| format!("Drive {field} cannot be NULL"))
}

fn drive_nonnegative_integer(stmt: &sqlite::Statement<'_>, field: &str) -> Result<u64, String> {
    drive_nullable_integer(stmt, field)?
        .ok_or_else(|| format!("Drive {field} cannot be NULL"))
        .and_then(|value| u64::try_from(value).map_err(|_| format!("Invalid Drive {field}")))
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
            parent_id: drive_nullable_text(&stmt, "parent_id")?,
            name: stmt.read::<String, _>("name").map_err(|e| e.to_string())?,
            backing_logical_channel_id: drive_nullable_text(&stmt, "backing_logical_channel_id")?,
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
            parent_id: drive_nullable_text(&stmt, "parent_id")?,
            name: stmt.read::<String, _>("name").map_err(|e| e.to_string())?,
            backing_logical_channel_id: drive_nullable_text(&stmt, "backing_logical_channel_id")?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            trashed_at: drive_nullable_integer(&stmt, "trashed_at")?,
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
            directory_id: drive_nullable_text(&stmt, "directory_id")?,
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            encryption_version: drive_entry_encryption_version(&stmt)?,
            plaintext_size: drive_nullable_plaintext_size(&stmt)?,
            crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
            content_fingerprint: drive_nullable_text(&stmt, "content_fingerprint")?,
            trashed_at: drive_nullable_integer(&stmt, "trashed_at")?,
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
            retirement_reason: drive_nullable_text(&stmt, "retirement_reason")?,
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
    // A corrupt local object may advertise i64::MAX blocks. Bound the
    // allocation before reserving memory; the manifest itself is capped at
    // MAX_DRIVE_MANIFEST_BYTES and each checksum consumes 32 bytes.
    let hash_bytes = block_count
        .checked_mul(32)
        .filter(|bytes| *bytes <= MAX_DRIVE_MANIFEST_BYTES)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "Drive V2 block hash count exceeds manifest limits".to_string())?;
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
    let mut raw = Vec::with_capacity(hash_bytes);
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
        let block_count = drive_nonnegative_integer(&stmt, "block_count")?;
        let persisted_hashes =
            drive_nullable_text(&stmt, "block_hashes_b64")?.filter(|value| !value.is_empty());
        let block_hashes_b64 = match persisted_hashes {
            Some(value) => Some(value),
            None => derive_block_hashes_b64(conn, &object_id, block_count)?,
        };
        rows.push(DriveObjectRecord {
            object_id,
            plaintext_size: drive_required_plaintext_size(&stmt)?,
            encryption_version: drive_entry_encryption_version(&stmt)?,
            crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
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
            part_index: drive_nonnegative_integer(&stmt, "part_index")?,
            first_chunk_index: drive_nonnegative_integer(&stmt, "first_chunk_index")?,
            chunk_count: drive_nonnegative_integer(&stmt, "chunk_count")?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            ciphertext_size: drive_nonnegative_integer(&stmt, "ciphertext_size")?,
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
            "SELECT entry_id, object_id, inode_id, directory_id, display_name,
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
            inode_id: drive_nullable_text(&stmt, "inode_id")?,
            directory_id: drive_nullable_text(&stmt, "directory_id")?,
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            updated_at: stmt
                .read::<i64, _>("updated_at")
                .map_err(|e| e.to_string())?,
            trashed_at: drive_nullable_integer(&stmt, "trashed_at")?,
        });
    }
    Ok(rows)
}

fn load_v2_files(conn: &sqlite::Connection) -> Result<Vec<DriveFileRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_id, e.object_id, e.inode_id, e.directory_id, e.display_name,
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
        let plaintext_size = drive_required_plaintext_size(&stmt)?;
        files.push(DriveFileRecord {
            entry_id: stmt
                .read::<String, _>("entry_id")
                .map_err(|e| e.to_string())?,
            file_id: stmt
                .read::<String, _>("object_id")
                .map_err(|e| e.to_string())?,
            inode_id: drive_nullable_text(&stmt, "inode_id")?,
            directory_id: drive_nullable_text(&stmt, "directory_id")?,
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
            encryption_version: drive_entry_encryption_version(&stmt)?,
            plaintext_size: Some(plaintext_size),
            crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
            content_fingerprint: drive_nullable_text(&stmt, "content_fingerprint")?,
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
            "SELECT e.entry_id, e.object_id, e.inode_id, e.directory_id, e.display_name,
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
    let plaintext_size = drive_required_plaintext_size(&stmt)?;
    Ok(Some(DriveFileRecord {
        entry_id: stmt
            .read::<String, _>("entry_id")
            .map_err(|e| e.to_string())?,
        file_id: stmt
            .read::<String, _>("object_id")
            .map_err(|e| e.to_string())?,
        inode_id: drive_nullable_text(&stmt, "inode_id")?,
        directory_id: drive_nullable_text(&stmt, "directory_id")?,
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
        encryption_version: drive_entry_encryption_version(&stmt)?,
        plaintext_size: Some(plaintext_size),
        crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
        content_fingerprint: drive_nullable_text(&stmt, "content_fingerprint")?,
        storage_version: 2,
    }))
}

#[cfg(test)]
thread_local! {
    static DRIVE_SNAPSHOT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_drive_snapshot_calls() {
    DRIVE_SNAPSHOT_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn drive_snapshot_call_count() -> u64 {
    DRIVE_SNAPSHOT_CALLS.with(|calls| calls.get())
}

pub fn drive_snapshot(conn: &sqlite::Connection) -> Result<DriveSnapshot, String> {
    #[cfg(test)]
    DRIVE_SNAPSHOT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
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
               AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
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
            directory_id: drive_nullable_text(&stmt, "directory_id")?,
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            total_size: drive_nonnegative_integer(&stmt, "total_size")?,
            mime_type: drive_nullable_text(&stmt, "mime_type")?,
            first_message_id: drive_required_integer(&stmt, "first_message_id")?,
            backing_channel_id: drive_required_integer(&stmt, "backing_channel_id")?,
            logical_channel_id: stmt
                .read::<String, _>("logical_channel_id")
                .map_err(|e| e.to_string())?,
            created_at: drive_required_integer(&stmt, "created_at")?,
            updated_at: drive_required_integer(&stmt, "updated_at")?,
            encryption_version: drive_entry_encryption_version(&stmt)?,
            plaintext_size: drive_nullable_plaintext_size(&stmt)?,
            crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
            content_fingerprint: drive_nullable_text(&stmt, "content_fingerprint")?,
            storage_version: 1,
            inode_id: None,
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
        pending.push(crate::drive_lookup::read_pending_record(&pending_stmt)?);
    }

    Ok(DriveSnapshot {
        revision,
        dirty,
        directories,
        files,
        pending,
        symlinks: crate::drive_posix::load_symlinks(conn, false)?,
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
               AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
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
        directory_id: drive_nullable_text(&stmt, "directory_id")?,
        display_name: stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?,
        total_size: drive_nonnegative_integer(&stmt, "total_size")?,
        mime_type: drive_nullable_text(&stmt, "mime_type")?,
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
        encryption_version: drive_entry_encryption_version(&stmt)?,
        plaintext_size: drive_nullable_plaintext_size(&stmt)?,
        crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
        content_fingerprint: drive_nullable_text(&stmt, "content_fingerprint")?,
        storage_version: 1,
        inode_id: None,
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
    drive_file_by_entry(conn, entry_id)
}

pub(crate) fn create_drive_symlink(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    name: &str,
    target: &str,
) -> Result<crate::drive_posix::DriveSymlinkRecord, String> {
    init_drive_schema(conn)?;
    let name = validate_drive_name(name)?;
    crate::drive_posix::validate_target(target)?;
    ensure_name_available(conn, parent, &name, None, None, None)?;
    if let Some(id) = parent {
        if crate::drive_lookup::directory(conn, id)?.is_none() {
            return Err("Parent folder does not exist".into());
        }
    }
    let now = now_ms();
    let record = crate::drive_posix::DriveSymlinkRecord {
        id: new_drive_id(),
        directory_id: parent.map(str::to_string),
        display_name: name,
        target: target.to_string(),
        created_at: now,
        updated_at: now,
        trashed_at: None,
    };
    conn.execute("SAVEPOINT drive_symlink_create")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        crate::drive_posix::upsert_symlink(conn, &record)?;
        mark_drive_dirty(conn)?;
        Ok(record)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_symlink_create", result)
}

pub(crate) fn rename_drive_symlink(
    conn: &sqlite::Connection,
    id: &str,
    parent: Option<&str>,
    name: &str,
) -> Result<(), String> {
    let name = validate_drive_name(name)?;
    let mut link =
        crate::drive_posix::symlink_by_id(conn, id)?.ok_or("Drive symlink does not exist")?;
    if let Some(parent) = parent {
        if crate::drive_lookup::directory(conn, parent)?.is_none() {
            return Err("Parent folder does not exist".into());
        }
    }
    ensure_name_available(conn, parent, &name, None, Some(id), None)?;
    link.directory_id = parent.map(str::to_string);
    link.display_name = name;
    link.updated_at = now_ms().max(link.updated_at.saturating_add(1));
    // A failed revision write must not leave a renamed link that cannot sync.
    // This remains safely nested inside callers' outer rename transactions.
    conn.execute("SAVEPOINT drive_symlink_rename")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        crate::drive_posix::upsert_symlink(conn, &link)?;
        mark_drive_dirty(conn)?;
        Ok(())
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_symlink_rename", result)
}

pub(crate) fn trash_drive_symlink(conn: &sqlite::Connection, id: &str) -> Result<(), String> {
    let mut link =
        crate::drive_posix::symlink_by_id(conn, id)?.ok_or("Drive symlink does not exist")?;
    link.updated_at = now_ms().max(link.updated_at.saturating_add(1));
    link.trashed_at = Some(link.updated_at);
    // Roll back the tombstone with the failed revision update, including
    // when called inside a larger native replacement transaction.
    conn.execute("SAVEPOINT drive_symlink_trash")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        crate::drive_posix::upsert_symlink(conn, &link)?;
        mark_drive_dirty(conn)?;
        Ok(())
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_symlink_trash", result)
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
    validate_prospective_directory_parent(conn, &id, parent_id, true)?;
    let now = now_ms();
    // Commit the folder and its sync revision together. A failed mkdir must
    // not leave a hidden successful insert or reserve its name for retry.
    conn.execute("SAVEPOINT drive_create_directory")
        .map_err(|e| e.to_string())?;
    let result = (|| {
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
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_create_directory", result)
}

/// Native mkdir must commit the folder and its requested mode together.
/// Nested helpers retain their own savepoints without committing this operation.
pub(crate) fn create_drive_directory_with_mode(
    conn: &sqlite::Connection,
    parent_id: Option<&str>,
    name: &str,
    mode: u32,
) -> Result<DriveDirectoryRecord, String> {
    conn.execute("SAVEPOINT drive_mkdir_with_mode")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let directory = create_drive_directory(conn, parent_id, name)?;
        crate::drive_posix::set_permission(conn, "directory", &directory.id, mode)?;
        Ok(directory)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_mkdir_with_mode", result)
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
    Ok(Some(crate::drive_lookup::read_pending_record(&stmt)?))
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
    drop(stmt);
    // SQLite reports success even when the pending row was already deleted.
    // Never acknowledge a finished upload whose record no longer exists.
    let mut changed = conn
        .prepare("SELECT changes()")
        .map_err(|e: sqlite::Error| e.to_string())?;
    changed.next().map_err(|e: sqlite::Error| e.to_string())?;
    if changed.read::<i64, _>(0).map_err(|e| e.to_string())? != 1 {
        return Err("Drive pending write no longer exists".to_string());
    }
    Ok(closed_at)
}

pub fn update_drive_pending_size(
    conn: &sqlite::Connection,
    pending_id: &str,
    size: u64,
) -> Result<(), String> {
    let sqlite_size = i64::try_from(size)
        .map_err(|_| "Drive pending file size exceeds SQLite integer limits".to_string())?;
    let mut stmt = conn
        .prepare(
            "UPDATE drive_pending_files
             SET size = ?, updated_at = ?
             WHERE id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, sqlite_size))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, now_ms()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    drop(stmt);
    // The pending row may have disappeared after an open handle cached its
    // identity. SQLite treats a zero-row UPDATE as success unless checked.
    let mut changed = conn
        .prepare("SELECT changes()")
        .map_err(|e: sqlite::Error| e.to_string())?;
    changed.next().map_err(|e: sqlite::Error| e.to_string())?;
    if changed.read::<i64, _>(0).map_err(|e| e.to_string())? != 1 {
        return Err("Drive pending write no longer exists".to_string());
    }
    Ok(())
}

/// Until the next manifest version and atomic alias-wide POSIX operations are
/// implemented, no entry carrying a shared-inode identity may be mutated as a
/// separate file. This deliberately checks trashed rows as well: restoring a
/// tombstone must not reactivate just one stale alias with inconsistent bytes.
pub(crate) fn reject_unimplemented_shared_inode_mutation(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<(), String> {
    let mut statement = conn
        .prepare(
            "SELECT inode_id FROM drive_entries_v2
             WHERE entry_id=? LIMIT 1",
        )
        .map_err(|error: sqlite::Error| error.to_string())?;
    statement
        .bind((1, entry_id))
        .map_err(|error: sqlite::Error| error.to_string())?;
    if statement
        .next()
        .map_err(|error: sqlite::Error| error.to_string())?
        == sqlite::State::Row
        && drive_nullable_text(&statement, "inode_id")?.is_some()
    {
        return Err("Drive hardlink mutation requires shared-inode POSIX support".to_string());
    }
    Ok(())
}

fn reject_linked_v2_entry_cow(conn: &sqlite::Connection, entry_id: &str) -> Result<(), String> {
    reject_unimplemented_shared_inode_mutation(conn, entry_id).map_err(|error| {
        if error.contains("requires shared-inode POSIX support") {
            "Drive hardlink editing requires shared-inode copy-on-write support".into()
        } else {
            error
        }
    })
}

pub fn create_drive_pending_replacement(
    conn: &sqlite::Connection,
    pending_id: &str,
    base_entry_id: &str,
    staging_path: &str,
) -> Result<DrivePendingRecord, String> {
    init_drive_schema(conn)?;
    // Do not split a future linked inode with today's per-entry COW path.
    reject_linked_v2_entry_cow(conn, base_entry_id)?;
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
                   AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
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
                drive_nullable_text(&stmt, "directory_id")?,
                stmt.read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?,
                drive_nonnegative_integer(&stmt, "visible_size")?,
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
        .bind((
            5,
            i64::try_from(size)
                .map_err(|_| "Drive replacement size exceeds SQLite integer limits".to_string())?,
        ))
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
    if end < start {
        return Err("Drive write range has reversed bounds".to_string());
    }
    if end == start {
        return Ok(());
    }
    if end > i64::MAX as u64 {
        return Err("Drive write range is too large".to_string());
    }
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = record_drive_pending_range_in_transaction(conn, pending_id, start, end);
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

/// Mutate pending dirty ranges inside an existing caller-owned transaction.
/// Never begin/commit here: resize groups ranges, metadata and parts atomically.
pub fn record_drive_pending_range_in_transaction(
    conn: &sqlite::Connection,
    pending_id: &str,
    start: u64,
    end: u64,
) -> Result<(), String> {
    if end < start {
        return Err("Drive write range has reversed bounds".to_string());
    }
    if end == start {
        return Ok(());
    }
    if end > i64::MAX as u64 {
        return Err("Drive write range is too large".to_string());
    }
    // The range journal has no foreign key: a stale FUSE writer must not
    // recreate orphan dirty bytes after the pending file was removed.
    // Check under the caller's write transaction before replacing ranges.
    let mut existing = conn
        .prepare("SELECT 1 FROM drive_pending_files WHERE id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    existing
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if existing.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Err("Drive pending write no longer exists".to_string());
    }
    drop(existing);
    let mut ranges = drive_pending_ranges(conn, pending_id)?;
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

    {
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
        Ok(())
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
        let start = drive_nonnegative_integer(&stmt, "range_start")?;
        let end = drive_nonnegative_integer(&stmt, "range_end")?;
        if end <= start {
            return Err("Drive pending range has invalid bounds".to_string());
        }
        ranges.push((start, end));
    }
    Ok(ranges)
}

pub fn clear_drive_pending_ranges_for_span(
    conn: &sqlite::Connection,
    pending_id: &str,
    span_start: u64,
    span_end: u64,
) -> Result<(), String> {
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result =
        clear_drive_pending_ranges_for_span_in_transaction(conn, pending_id, span_start, span_end);
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

/// Clear a dirty span as one step of a caller-owned transaction. This helper
/// deliberately does not begin, commit or roll back a transaction.
pub fn clear_drive_pending_ranges_for_span_in_transaction(
    conn: &sqlite::Connection,
    pending_id: &str,
    span_start: u64,
    span_end: u64,
) -> Result<(), String> {
    if span_end < span_start {
        return Err("Drive pending clear span has reversed bounds".to_string());
    }
    let ranges = drive_pending_ranges(conn, pending_id)?;
    if span_end == span_start {
        return Ok(());
    }
    {
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
        Ok(())
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
            chunk_index: drive_nonnegative_integer(&stmt, "chunk_index")?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            chunk_size: drive_nonnegative_integer(&stmt, "chunk_size")?,
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

#[expect(
    clippy::too_many_arguments,
    reason = "Persisted chunk-part metadata has an explicit stable field contract"
)]
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
            part_index: drive_nonnegative_integer(&stmt, "part_index")?,
            first_chunk_index: drive_nonnegative_integer(&stmt, "first_chunk_index")?,
            chunk_count: drive_nonnegative_integer(&stmt, "chunk_count")?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            part_size: drive_nonnegative_integer(&stmt, "part_size")?,
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
            part_index: drive_nonnegative_integer(&stmt, "part_index")?,
            first_chunk_index: drive_nonnegative_integer(&stmt, "first_chunk_index")?,
            chunk_count: drive_nonnegative_integer(&stmt, "chunk_count")?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            message_id: stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            ciphertext_size: drive_nonnegative_integer(&stmt, "ciphertext_size")?,
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
            chunk_index: drive_nonnegative_integer(&stmt, "chunk_index")?,
            part_index: drive_nonnegative_integer(&stmt, "part_index")?,
            part_offset: drive_nonnegative_integer(&stmt, "part_offset")?,
            ciphertext_size: drive_nonnegative_integer(&stmt, "ciphertext_size")?,
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

#[cfg(test)]
static DRIVE_OBJECT_BLOCK_LOCATION_QUERY_STEPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn reset_drive_object_block_location_query_steps() {
    DRIVE_OBJECT_BLOCK_LOCATION_QUERY_STEPS.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn drive_object_block_location_query_steps() -> usize {
    DRIVE_OBJECT_BLOCK_LOCATION_QUERY_STEPS.load(std::sync::atomic::Ordering::SeqCst)
}

fn record_drive_object_block_location_query_step() {
    #[cfg(test)]
    DRIVE_OBJECT_BLOCK_LOCATION_QUERY_STEPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn drive_object_block_location_query(
    conn: &sqlite::Connection,
    object_id: &str,
    chunk_index: u64,
) -> Result<Option<(DriveObjectBlockRecord, DriveObjectPartRecord)>, String> {
    let Ok(chunk_index) = i64::try_from(chunk_index) else {
        return Ok(None);
    };
    record_drive_object_block_location_query_step();
    let mut stmt = conn
        .prepare(
            "SELECT b.object_id AS block_object_id, b.chunk_index AS block_chunk_index,
                    b.part_index AS block_part_index, b.part_offset AS block_part_offset,
                    b.ciphertext_size AS block_ciphertext_size, b.sha256 AS block_sha256,
                    b.plaintext_sha256 AS block_plaintext_sha256,
                    p.object_id AS part_object_id, p.part_index AS part_part_index,
                    p.first_chunk_index AS part_first_chunk_index,
                    p.chunk_count AS part_chunk_count,
                    p.backing_channel_id AS part_backing_channel_id,
                    p.message_id AS part_message_id,
                    p.ciphertext_size AS part_ciphertext_size, p.sha256 AS part_sha256
             FROM drive_object_blocks b
             LEFT JOIN drive_object_parts p
               ON p.object_id=b.object_id AND p.part_index=b.part_index
             WHERE b.object_id=?1 AND b.chunk_index=?2
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, object_id)).map_err(|e| e.to_string())?;
    stmt.bind((2, chunk_index)).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let block = DriveObjectBlockRecord {
        object_id: stmt
            .read::<String, _>("block_object_id")
            .map_err(|e| e.to_string())?,
        chunk_index: drive_nonnegative_integer(&stmt, "block_chunk_index")?,
        part_index: drive_nonnegative_integer(&stmt, "block_part_index")?,
        part_offset: drive_nonnegative_integer(&stmt, "block_part_offset")?,
        ciphertext_size: drive_nonnegative_integer(&stmt, "block_ciphertext_size")?,
        sha256: stmt
            .read::<String, _>("block_sha256")
            .map_err(|e| e.to_string())?,
        plaintext_sha256: stmt
            .read::<String, _>("block_plaintext_sha256")
            .map_err(|e| e.to_string())?,
    };
    let part_object_id = stmt
        .read::<Option<String>, _>("part_object_id")
        .map_err(|e| e.to_string())?;
    if part_object_id.is_none() {
        return Err("Drive V2 block references a missing remote part".to_string());
    }
    let part = DriveObjectPartRecord {
        object_id: part_object_id.unwrap(),
        part_index: drive_nonnegative_integer(&stmt, "part_part_index")?,
        first_chunk_index: drive_nonnegative_integer(&stmt, "part_first_chunk_index")?,
        chunk_count: drive_nonnegative_integer(&stmt, "part_chunk_count")?,
        backing_channel_id: stmt
            .read::<i64, _>("part_backing_channel_id")
            .map_err(|e| e.to_string())?,
        message_id: stmt
            .read::<i64, _>("part_message_id")
            .map_err(|e| e.to_string())?,
        ciphertext_size: drive_nonnegative_integer(&stmt, "part_ciphertext_size")?,
        sha256: stmt
            .read::<String, _>("part_sha256")
            .map_err(|e| e.to_string())?,
    };
    Ok(Some((block, part)))
}

pub fn drive_object_block_location(
    conn: &sqlite::Connection,
    object_id: &str,
    chunk_index: u64,
) -> Result<Option<(DriveObjectBlockRecord, DriveObjectPartRecord)>, String> {
    init_drive_schema(conn)?;
    drive_object_block_location_query(conn, object_id, chunk_index)
}

pub(crate) fn drive_object_block_location_preinitialized(
    conn: &sqlite::Connection,
    object_id: &str,
    chunk_index: u64,
) -> Result<Option<(DriveObjectBlockRecord, DriveObjectPartRecord)>, String> {
    drive_object_block_location_query(conn, object_id, chunk_index)
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
    crate::drive_posix::remove_pending_permission(conn, pending_id)?;
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

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeRenameKind {
    Directory,
    File,
    Pending,
    Symlink,
}

#[cfg(target_os = "linux")]
struct NativeRenameNamespaceRow {
    kind: i64,
    id: String,
    display_name: String,
    base_entry_id: Option<String>,
    destination_visible: bool,
    has_v2_id: bool,
    base_has_v2: bool,
}

#[cfg(target_os = "linux")]
struct NativeFileRenameNamespace {
    rows: Vec<NativeRenameNamespaceRow>,
    destination: Option<(NativeRenameKind, String)>,
    parent_exists: bool,
    source_storage_version: Option<u32>,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
enum NativeRenameReplacement {
    File {
        id: String,
        is_v2: bool,
    },
    Symlink {
        id: String,
    },
    Pending {
        id: String,
        base_entry_id: Option<String>,
        base_is_v2: bool,
    },
}

#[cfg(target_os = "linux")]
impl NativeFileRenameNamespace {
    fn has_live_v2_entry(&self, id: &str) -> bool {
        self.rows
            .iter()
            .find(|row| matches!(row.kind, 3 | 4) && row.id == id)
            .is_some_and(|row| row.has_v2_id)
    }

    fn pending_base_has_live_v2_entry(&self, pending_id: &str, base_id: &str) -> bool {
        self.rows
            .iter()
            .find(|row| row.kind == 2 && row.id == pending_id)
            .is_some_and(|row| row.base_entry_id.as_deref() == Some(base_id) && row.base_has_v2)
    }

    fn has_portable_name_collision(
        &self,
        requested_name: &str,
        source_id: &str,
        replaced_destination: Option<&NativeRenameReplacement>,
    ) -> bool {
        let requested_key = portable_name_key(requested_name);
        self.rows.iter().any(|row| {
            // File rename historically ignores this ID in the legacy/V2/symlink
            // tables, but never ignores a directory or pending row with that ID.
            if matches!(row.kind, 0 | 3 | 4) && row.id == source_id {
                return false;
            }

            // Trashing the authoritative V2 row must not reveal a shadowed
            // legacy alias at a different name after this savepoint commits.
            let legacy_alias_would_resurface = match replaced_destination {
                Some(NativeRenameReplacement::File { id, is_v2: true }) => {
                    row.kind == 3 && row.id == *id
                }
                Some(NativeRenameReplacement::Pending {
                    base_entry_id: Some(base_entry_id),
                    base_is_v2: true,
                    ..
                }) => row.kind == 3 && row.id == *base_entry_id,
                _ => false,
            };
            if legacy_alias_would_resurface {
                return true;
            }

            // A V2 metadata row without its referenced object is not a live
            // entry and must not reserve a name over a valid legacy fallback.
            if row.kind == 4 && !row.has_v2_id {
                return false;
            }
            if portable_name_key(&row.display_name) != requested_key {
                return false;
            }

            let removed_by_replacement = match replaced_destination {
                Some(NativeRenameReplacement::File { id, is_v2 }) => {
                    row.id.as_str() == id.as_str() && row.kind == if *is_v2 { 4 } else { 3 }
                }
                Some(NativeRenameReplacement::Symlink { id }) => row.kind == 0 && row.id == *id,
                Some(NativeRenameReplacement::Pending {
                    id,
                    base_entry_id,
                    base_is_v2,
                }) => {
                    (row.kind == 2 && row.id == *id)
                        || base_entry_id.as_deref().is_some_and(|base| {
                            row.id == base && row.kind == if *base_is_v2 { 4 } else { 3 }
                        })
                }
                None => false,
            };
            !removed_by_replacement
        })
    }
}

#[cfg(target_os = "linux")]
fn native_file_rename_namespace(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    name: &str,
    source_id: &str,
) -> Result<NativeFileRenameNamespace, String> {
    record_drive_native_file_rename_namespace_query();
    let mut stmt = conn
        .prepare(
            "SELECT kind, id, display_name, base_entry_id, destination_visible,
                    has_v2_id, base_has_v2, parent_exists, source_storage_version
             FROM (
                 SELECT -1 AS kind, ?2 AS id, '' AS display_name, NULL AS base_entry_id,
                        0 AS destination_visible, 0 AS has_v2_id, 0 AS base_has_v2,
                        CASE WHEN ?1 IS NULL OR EXISTS (
                                  SELECT 1 FROM drive_directories d
                                  WHERE d.id = ?1 AND d.trashed_at IS NULL
                             )
                             THEN 1 ELSE 0 END AS parent_exists,
                        CASE
                            WHEN EXISTS (
                                SELECT 1 FROM drive_entries_v2 e
                                JOIN drive_objects o ON o.object_id = e.object_id
                                WHERE e.entry_id = ?2 AND e.trashed_at IS NULL
                            ) THEN 2
                            WHEN EXISTS (
                                SELECT 1 FROM drive_entries_v2 shadow
                                JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                                WHERE shadow.entry_id = ?2
                            ) THEN NULL
                            WHEN EXISTS (
                                SELECT 1 FROM drive_file_entries e
                                JOIN logical_files f ON f.file_id = e.file_id
                                JOIN logical_channels c ON c.logical_id = f.logical_channel_id
                                WHERE e.entry_id = ?2 AND e.trashed_at IS NULL
                                  AND f.status = 'complete'
                                  AND NOT EXISTS (
                                      SELECT 1 FROM drive_entries_v2 shadow
                                      JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                                      WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                                  )
                            ) THEN 1
                            ELSE NULL
                        END AS source_storage_version
                 UNION ALL
                 SELECT 0 AS kind, id, display_name, NULL AS base_entry_id,
                        1 AS destination_visible, 0 AS has_v2_id, 0 AS base_has_v2,
                        0 AS parent_exists, NULL AS source_storage_version
                 FROM drive_symlinks
                 WHERE directory_id IS ?1 AND trashed_at IS NULL
                 UNION ALL
                 SELECT 1 AS kind, id, name AS display_name, NULL AS base_entry_id,
                        1 AS destination_visible, 0 AS has_v2_id, 0 AS base_has_v2,
                        0 AS parent_exists, NULL AS source_storage_version
                 FROM drive_directories
                 WHERE parent_id IS ?1 AND trashed_at IS NULL
                 UNION ALL
                 SELECT 2 AS kind, id, display_name, base_entry_id, 1 AS destination_visible,
                        0 AS has_v2_id,
                        CASE WHEN base_entry_id IS NOT NULL AND EXISTS (
                                  SELECT 1 FROM drive_entries_v2 v
                                  JOIN drive_objects o ON o.object_id = v.object_id
                                  WHERE v.entry_id = drive_pending_files.base_entry_id
                             )
                             THEN 1 ELSE 0 END AS base_has_v2,
                        0 AS parent_exists, NULL AS source_storage_version
                 FROM drive_pending_files
                 WHERE directory_id IS ?1
                 UNION ALL
                 SELECT 3 AS kind, e.entry_id AS id, e.display_name, NULL AS base_entry_id,
                        CASE WHEN f.status = 'complete' AND c.logical_id IS NOT NULL
                                  AND NOT EXISTS (
                                      SELECT 1 FROM drive_pending_files p
                                      WHERE p.base_entry_id = e.entry_id
                                  )
                                  AND NOT EXISTS (
                                      SELECT 1 FROM drive_entries_v2 v
                                      JOIN drive_objects o ON o.object_id = v.object_id
                                      WHERE v.entry_id = e.entry_id
                                  )
                             THEN 1 ELSE 0 END AS destination_visible,
                        CASE WHEN EXISTS (
                                  SELECT 1 FROM drive_entries_v2 v
                                  JOIN drive_objects o ON o.object_id = v.object_id
                                  WHERE v.entry_id = e.entry_id
                             )
                             THEN 1 ELSE 0 END AS has_v2_id,
                        0 AS base_has_v2, 0 AS parent_exists, NULL AS source_storage_version
                 FROM drive_file_entries e
                 LEFT JOIN logical_files f ON f.file_id = e.file_id
                 LEFT JOIN logical_channels c ON c.logical_id = f.logical_channel_id
                 WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL
                 UNION ALL
                 SELECT 4 AS kind, e.entry_id AS id, e.display_name, NULL AS base_entry_id,
                        CASE WHEN o.object_id IS NOT NULL
                                  AND NOT EXISTS (
                                      SELECT 1 FROM drive_pending_files p
                                      WHERE p.base_entry_id = e.entry_id
                                  )
                             THEN 1 ELSE 0 END AS destination_visible,
                        CASE WHEN o.object_id IS NOT NULL THEN 1 ELSE 0 END AS has_v2_id,
                        0 AS base_has_v2,
                        0 AS parent_exists, NULL AS source_storage_version
                 FROM drive_entries_v2 e
                 LEFT JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE e.directory_id IS ?1 AND e.trashed_at IS NULL
             )
             ORDER BY kind ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, parent)).map_err(|e| e.to_string())?;
    stmt.bind((2, source_id)).map_err(|e| e.to_string())?;

    let mut rows = Vec::new();
    let mut metadata = None;
    while let sqlite::State::Row = stmt.next().map_err(|e| e.to_string())? {
        let kind = stmt.read::<i64, _>("kind").map_err(|e| e.to_string())?;
        if kind == -1 {
            let parent_exists = stmt
                .read::<i64, _>("parent_exists")
                .map_err(|e| e.to_string())?
                != 0;
            let source_storage_version = stmt
                .read::<Option<i64>, _>("source_storage_version")
                .map_err(|e| e.to_string())?
                .map(|version| version as u32);
            metadata = Some((parent_exists, source_storage_version));
            continue;
        }
        rows.push(NativeRenameNamespaceRow {
            kind,
            id: stmt.read::<String, _>("id").map_err(|e| e.to_string())?,
            display_name: stmt
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            base_entry_id: stmt
                .read::<Option<String>, _>("base_entry_id")
                .map_err(|e| e.to_string())?,
            destination_visible: stmt
                .read::<i64, _>("destination_visible")
                .map_err(|e| e.to_string())?
                != 0,
            has_v2_id: stmt
                .read::<i64, _>("has_v2_id")
                .map_err(|e| e.to_string())?
                != 0,
            base_has_v2: stmt
                .read::<i64, _>("base_has_v2")
                .map_err(|e| e.to_string())?
                != 0,
        });
    }
    let (parent_exists, source_storage_version) =
        metadata.ok_or_else(|| "Drive rename namespace metadata is missing".to_string())?;
    let destination = rows
        .iter()
        .find(|row| row.destination_visible && row.display_name == name)
        .map(|row| {
            let kind = match row.kind {
                0 => NativeRenameKind::Symlink,
                1 => NativeRenameKind::Directory,
                2 => NativeRenameKind::Pending,
                3 | 4 => NativeRenameKind::File,
                _ => unreachable!("unknown Drive namespace row kind"),
            };
            (kind, row.id.clone())
        });
    Ok(NativeFileRenameNamespace {
        rows,
        destination,
        parent_exists,
        source_storage_version,
    })
}

/// POSIX replacement is one metadata transaction. Remote content remains in
/// trash; pending content is only replaceable before it has remote parts and
/// after its writers have closed. The caller cleans local staging after commit.
#[cfg(target_os = "linux")]
#[cfg(test)]
pub(crate) fn rename_drive_item_native(
    conn: &sqlite::Connection,
    kind: NativeRenameKind,
    id: &str,
    parent: Option<&str>,
    name: &str,
    no_replace: bool,
    pending_busy: impl Fn(&str) -> bool,
) -> Result<Option<DrivePendingRecord>, String> {
    rename_drive_item_native_with_destination(
        conn,
        kind,
        id,
        parent,
        name,
        no_replace,
        pending_busy,
        |_, _| Ok(()),
    )
}

/// As above, with one callback when a compatible existing destination is
/// about to be replaced. The callback runs inside the savepoint before trashing
/// the destination, so failure rolls the metadata transaction back atomically.
/// It is not called for absent destinations, same-identity renames, or NOREPLACE.
#[cfg(target_os = "linux")]
#[expect(
    clippy::too_many_arguments,
    reason = "Atomic rename callback contract preserves transaction and conflict semantics"
)]
pub(crate) fn rename_drive_item_native_with_destination(
    conn: &sqlite::Connection,
    kind: NativeRenameKind,
    id: &str,
    parent: Option<&str>,
    name: &str,
    no_replace: bool,
    pending_busy: impl Fn(&str) -> bool,
    mut before_replace: impl FnMut(NativeRenameKind, &str) -> Result<(), String>,
) -> Result<Option<DrivePendingRecord>, String> {
    let name = validate_drive_name(name)?;
    conn.execute("SAVEPOINT native_drive_rename")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let native_file_namespace = if kind == NativeRenameKind::File {
            let namespace = native_file_rename_namespace(conn, parent, &name, id)?;
            // Preserve the previous error precedence: validate the destination
            // parent before reporting a missing source identity.
            if !namespace.parent_exists {
                return Err("Destination folder does not exist".to_string());
            }
            if namespace.source_storage_version.is_none() {
                return Err("Drive rename source does not exist".to_string());
            }
            // Check before destination retirement callbacks. A linked source
            // cannot be moved using the independent-entry namespace contract.
            reject_unimplemented_shared_inode_mutation(conn, id)?;
            Some(namespace)
        } else {
            if let Some(parent_id) = parent {
                if crate::drive_lookup::directory(conn, parent_id)?.is_none() {
                    return Err("Destination folder does not exist".to_string());
                }
            }
            let source_exists = match kind {
                NativeRenameKind::Directory => crate::drive_lookup::directory(conn, id)?.is_some(),
                NativeRenameKind::Pending => drive_pending_by_id(conn, id)?.is_some(),
                NativeRenameKind::Symlink => crate::drive_posix::symlink_by_id(conn, id)?.is_some(),
                NativeRenameKind::File => unreachable!("file rename uses fused namespace lookup"),
            };
            if !source_exists {
                return Err("Drive rename source does not exist".to_string());
            }
            None
        };
        let destination = if let Some(namespace) = native_file_namespace.as_ref() {
            namespace.destination.clone()
        } else {
            crate::drive_lookup::child_with_symlink(conn, parent, &name)?.map(
                |(item, is_symlink)| match item {
                    crate::drive_fs_core::DriveNodeId::Directory(id) => {
                        (NativeRenameKind::Directory, id)
                    }
                    crate::drive_fs_core::DriveNodeId::Pending(id) => {
                        (NativeRenameKind::Pending, id)
                    }
                    crate::drive_fs_core::DriveNodeId::File(id) if is_symlink => {
                        (NativeRenameKind::Symlink, id)
                    }
                    crate::drive_fs_core::DriveNodeId::File(id) => (NativeRenameKind::File, id),
                    crate::drive_fs_core::DriveNodeId::Root => {
                        unreachable!("Drive root is not a child")
                    }
                },
            )
        };
        let mut removed_pending = None;
        let mut replaced_destination: Option<NativeRenameReplacement> = None;
        if let Some((destination_kind, destination_id)) = destination {
            if no_replace {
                return Err("A drive item already exists".to_string());
            }
            if kind == destination_kind && id == destination_id {
                return Ok(None);
            }
            // An overwrite callback can retire a kernel-visible reader.
            // Reject unsupported linked destinations *before* calling it,
            // even though the enclosing DB savepoint could roll back SQL.
            if destination_kind == NativeRenameKind::File {
                reject_unimplemented_shared_inode_mutation(conn, &destination_id)?;
            }
            if kind == NativeRenameKind::Directory
                && destination_kind != NativeRenameKind::Directory
            {
                return Err("Destination is not a directory".to_string());
            }
            if kind != NativeRenameKind::Directory
                && destination_kind == NativeRenameKind::Directory
            {
                return Err("Destination is a directory".to_string());
            }
            match destination_kind {
                NativeRenameKind::Directory => {
                    before_replace(destination_kind, &destination_id)?;
                    trash_drive_directory(conn, &destination_id)?;
                }
                NativeRenameKind::File => {
                    before_replace(destination_kind, &destination_id)?;
                    let destination_is_v2 = native_file_namespace
                        .as_ref()
                        .is_some_and(|namespace| namespace.has_live_v2_entry(&destination_id));
                    if native_file_namespace.is_some() {
                        trash_drive_file_with_known_version(
                            conn,
                            &destination_id,
                            destination_is_v2,
                        )?;
                    } else {
                        trash_drive_file(conn, &destination_id)?;
                    }
                    if kind == NativeRenameKind::File {
                        replaced_destination = Some(NativeRenameReplacement::File {
                            id: destination_id,
                            is_v2: destination_is_v2,
                        });
                    }
                }
                NativeRenameKind::Symlink => {
                    before_replace(destination_kind, &destination_id)?;
                    trash_drive_symlink(conn, &destination_id)?;
                    if kind == NativeRenameKind::File {
                        replaced_destination =
                            Some(NativeRenameReplacement::Symlink { id: destination_id });
                    }
                }
                NativeRenameKind::Pending => {
                    let pending = drive_pending_by_id(conn, &destination_id)?
                        .ok_or_else(|| "Drive destination disappeared".to_string())?;
                    if pending.closed_at.is_none()
                        || pending_busy(&pending.id)
                        || !drive_pending_parts(conn, &pending.id)?.is_empty()
                        || !drive_pending_chunks(conn, &pending.id)?.is_empty()
                    {
                        return Err(
                            "Destination has an active or uploaded pending write".to_string()
                        );
                    }
                    let base_entry_id = pending.base_entry_id.clone();
                    if let Some(base) = base_entry_id.as_deref() {
                        reject_unimplemented_shared_inode_mutation(conn, base)?;
                    }
                    let base_is_v2 = base_entry_id.as_deref().is_some_and(|base| {
                        native_file_namespace.as_ref().is_some_and(|namespace| {
                            namespace.pending_base_has_live_v2_entry(&destination_id, base)
                        })
                    });
                    before_replace(destination_kind, &destination_id)?;
                    if let Some(base) = base_entry_id.as_deref() {
                        if native_file_namespace.is_some() {
                            trash_drive_file_with_known_version(conn, base, base_is_v2)?;
                        } else {
                            trash_drive_file(conn, base)?;
                        }
                    }
                    delete_drive_pending(conn, &pending.id)?;
                    if kind == NativeRenameKind::File {
                        replaced_destination = Some(NativeRenameReplacement::Pending {
                            id: pending.id.clone(),
                            base_entry_id,
                            base_is_v2,
                        });
                    }
                    removed_pending = Some(pending);
                }
            }
        }
        if kind == NativeRenameKind::File {
            let namespace = native_file_namespace
                .as_ref()
                .ok_or_else(|| "Drive rename namespace was not resolved".to_string())?;
            if namespace.has_portable_name_collision(&name, id, replaced_destination.as_ref()) {
                return Err(format!("A drive item named \"{name}\" already exists"));
            }
        }
        match kind {
            NativeRenameKind::Directory => rename_drive_directory(conn, id, parent, &name)?,
            NativeRenameKind::File => {
                let storage_version = native_file_namespace
                    .as_ref()
                    .and_then(|namespace| namespace.source_storage_version)
                    .ok_or_else(|| "Drive rename source does not exist".to_string())?;
                let is_v2 = storage_version == 2;
                update_drive_file_location(conn, id, parent, &name, is_v2)?;
                if is_v2 {
                    retire_legacy_fallback_for_authoritative_v2(conn, id)?;
                }
            }
            NativeRenameKind::Pending => rename_drive_pending(conn, id, parent, &name)?,
            NativeRenameKind::Symlink => rename_drive_symlink(conn, id, parent, &name)?,
        }
        Ok(removed_pending)
    })();
    match result {
        Ok(value) => {
            if let Err(error) = conn.execute("RELEASE native_drive_rename") {
                let _ =
                    conn.execute("ROLLBACK TO native_drive_rename; RELEASE native_drive_rename");
                return Err(error.to_string());
            }
            Ok(value)
        }
        Err(error) => {
            conn.execute("ROLLBACK TO native_drive_rename; RELEASE native_drive_rename")
                .map_err(|rollback| format!("{error}; Drive rename rollback failed: {rollback}"))?;
            Err(error)
        }
    }
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
    validate_prospective_directory_parent(conn, id, new_parent_id, false)?;
    // The folder change and its sync revision must either both persist or both roll back.
    // This savepoint also nests safely inside an existing native rename transaction.
    conn.execute("SAVEPOINT drive_folder_rename")
        .map_err(|e| e.to_string())?;
    let result = (|| {
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
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_folder_rename", result)
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
    record_drive_rename_file_version_probe();
    let is_v2 = {
        let mut check = conn
            .prepare(
                "SELECT 1 FROM drive_entries_v2 e
                 JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE e.entry_id = ? AND e.trashed_at IS NULL
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
    if !is_v2 {
        let mut trashed_v2 = conn
            .prepare(
                "SELECT 1 FROM drive_entries_v2 e
                 JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE e.entry_id = ? AND e.trashed_at IS NOT NULL
                 LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        trashed_v2
            .bind((1, entry_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if matches!(
            trashed_v2
                .next()
                .map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        ) {
            return Err("Drive rename source is in Trash".to_string());
        }
    }

    conn.execute("SAVEPOINT public_drive_file_rename")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        update_drive_file_location(conn, entry_id, new_directory_id, &new_name, is_v2)?;
        if is_v2 {
            retire_legacy_fallback_for_authoritative_v2(conn, entry_id)?;
        }
        Ok(())
    })();
    crate::drive_posix::finish_savepoint(conn, "public_drive_file_rename", result)
}

fn retire_legacy_fallback_for_authoritative_v2(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<(), String> {
    let now = now_ms();
    let mut stmt = conn
        .prepare(
            "UPDATE drive_file_entries
             SET trashed_at = ?, updated_at = ?
             WHERE entry_id = ? AND trashed_at IS NULL
               AND EXISTS (
                   SELECT 1 FROM drive_entries_v2 v
                   JOIN drive_objects o ON o.object_id = v.object_id
                   WHERE v.entry_id = ?
               )",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, now)).map_err(|e| e.to_string())?;
    stmt.bind((2, now)).map_err(|e| e.to_string())?;
    stmt.bind((3, entry_id)).map_err(|e| e.to_string())?;
    stmt.bind((4, entry_id)).map_err(|e| e.to_string())?;
    stmt.next().map_err(|e| e.to_string())?;
    Ok(())
}

fn update_drive_file_location(
    conn: &sqlite::Connection,
    entry_id: &str,
    new_directory_id: Option<&str>,
    new_name: &str,
    is_v2: bool,
) -> Result<(), String> {
    if is_v2 {
        reject_unimplemented_shared_inode_mutation(conn, entry_id)?;
    }
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
    stmt.bind((2, new_name))
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
    record_drive_trash_file_version_probe();
    let is_v2 = {
        let mut check = conn
            .prepare(
                "SELECT 1 FROM drive_entries_v2 e
                 JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE e.entry_id = ?
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
    trash_drive_file_with_known_version(conn, entry_id, is_v2)
}

fn trash_drive_file_with_known_version(
    conn: &sqlite::Connection,
    entry_id: &str,
    is_v2: bool,
) -> Result<(), String> {
    conn.execute("SAVEPOINT known_version_drive_trash")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        if is_v2 {
            reject_unimplemented_shared_inode_mutation(conn, entry_id)?;
        }
        let now = now_ms();
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
        stmt.bind((1, now)).map_err(|e| e.to_string())?;
        stmt.bind((2, now)).map_err(|e| e.to_string())?;
        stmt.bind((3, entry_id)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        drop(stmt);

        if is_v2 {
            retire_legacy_fallback_for_authoritative_v2(conn, entry_id)?;
        }
        mark_drive_dirty(conn)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = conn.execute("RELEASE known_version_drive_trash") {
                let _ = conn.execute(
                    "ROLLBACK TO known_version_drive_trash; RELEASE known_version_drive_trash",
                );
                return Err(error.to_string());
            }
            Ok(())
        }
        Err(error) => {
            conn.execute(
                "ROLLBACK TO known_version_drive_trash; RELEASE known_version_drive_trash",
            )
            .map_err(|rollback| format!("{error}; Drive trash rollback failed: {rollback}"))?;
            Err(error)
        }
    }
}

pub(crate) fn restore_parent_if_active(
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

pub(crate) fn available_restored_name(
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
               AND NOT EXISTS (
                   SELECT 1 FROM drive_entries_v2 v
                   JOIN drive_objects o ON o.object_id = v.object_id
                   WHERE v.entry_id = e.entry_id
               )
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
            original_directory_id: drive_nullable_text(&files, "directory_id")?,
            size: drive_nullable_integer(&files, "total_size")?
                .map(|value| {
                    u64::try_from(value).map_err(|_| "Invalid Drive trash file size".to_string())
                })
                .transpose()?,
            restorable: drive_nullable_text(&files, "file_status")?.as_deref() == Some("complete"),
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
            original_directory_id: drive_nullable_text(&v2_files, "directory_id")?,
            size: Some(drive_required_plaintext_size(&v2_files)?),
            restorable: true,
        });
    }
    drop(v2_files);

    let mut links = conn
        .prepare(
            "SELECT id, display_name, directory_id, trashed_at
             FROM drive_symlinks WHERE trashed_at IS NOT NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = links.next().map_err(|e: sqlite::Error| e.to_string())? {
        items.push(DriveTrashItem {
            kind: "symlink".to_string(),
            id: links.read::<String, _>("id").map_err(|e| e.to_string())?,
            name: links
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?,
            deleted_at: links
                .read::<i64, _>("trashed_at")
                .map_err(|e| e.to_string())?,
            original_directory_id: drive_nullable_text(&links, "directory_id")?,
            size: None,
            restorable: true,
        });
    }
    drop(links);

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
            original_directory_id: drive_nullable_text(&directories, "parent_id")?,
            size: None,
            restorable: true,
        });
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.deleted_at));
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
    if kind == "file" {
        reject_unimplemented_shared_inode_mutation(conn, id)?;
    }
    let now = now_ms();

    match kind {
        "symlink" => {
            conn.execute("SAVEPOINT drive_symlink_restore")
                .map_err(|e| e.to_string())?;
            let result = (|| {
                let mut read = conn
                    .prepare(
                        "SELECT directory_id, display_name, updated_at
                     FROM drive_symlinks WHERE id = ? AND trashed_at IS NOT NULL",
                    )
                    .map_err(|e| e.to_string())?;
                read.bind((1, id)).map_err(|e| e.to_string())?;
                if read.next().map_err(|e| e.to_string())? != sqlite::State::Row {
                    return Err("Drive trash symlink no longer exists".to_string());
                }
                let parent = drive_nullable_text(&read, "directory_id")?;
                let name = read
                    .read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?;
                let previous_update = read
                    .read::<i64, _>("updated_at")
                    .map_err(|e| e.to_string())?;
                drop(read);
                let parent = restore_parent_if_active(conn, parent.as_deref())?;
                let name = available_restored_name(conn, parent.as_deref(), &name, None, None)?;
                let mut update = conn
                    .prepare(
                        "UPDATE drive_symlinks SET directory_id = ?, display_name = ?,
                     trashed_at = NULL, updated_at = ? WHERE id = ? AND trashed_at IS NOT NULL",
                    )
                    .map_err(|e| e.to_string())?;
                update
                    .bind((1, parent.as_deref()))
                    .map_err(|e| e.to_string())?;
                update.bind((2, name.as_str())).map_err(|e| e.to_string())?;
                update
                    .bind((3, now.max(previous_update.saturating_add(1))))
                    .map_err(|e| e.to_string())?;
                update.bind((4, id)).map_err(|e| e.to_string())?;
                update.next().map_err(|e| e.to_string())?;
                drop(update);
                mark_drive_dirty(conn)?;
                Ok(name)
            })();
            crate::drive_posix::finish_savepoint(conn, "drive_symlink_restore", result)
        }
        "file" => {
            let mut v2_read = conn
                .prepare(
                    "SELECT e.directory_id, e.display_name
                     FROM drive_entries_v2 e
                     JOIN drive_objects o ON o.object_id = e.object_id
                     WHERE e.entry_id = ? AND e.trashed_at IS NOT NULL
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
                let original_parent = drive_nullable_text(&v2_read, "directory_id")?;
                let original_name = v2_read
                    .read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?;
                drop(v2_read);
                let target_parent = restore_parent_if_active(conn, original_parent.as_deref())?;
                let restored_name = available_restored_name(
                    conn,
                    target_parent.as_deref(),
                    &original_name,
                    None,
                    Some(id),
                )?;
                conn.execute("SAVEPOINT drive_v2_restore")
                    .map_err(|e| e.to_string())?;
                let result = (|| {
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
                    drop(update);
                    retire_legacy_fallback_for_authoritative_v2(conn, id)?;
                    mark_drive_dirty(conn)?;
                    Ok(restored_name)
                })();
                return crate::drive_posix::finish_savepoint(conn, "drive_v2_restore", result);
            }
            drop(v2_read);

            let mut active_v2 = conn
                .prepare(
                    "SELECT 1 FROM drive_entries_v2 v
                     JOIN drive_objects o ON o.object_id = v.object_id
                     WHERE v.entry_id = ? AND v.trashed_at IS NULL
                     LIMIT 1",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            active_v2.bind((1, id)).map_err(|e| e.to_string())?;
            if matches!(
                active_v2.next().map_err(|e| e.to_string())?,
                sqlite::State::Row
            ) {
                return Err(
                    "A legacy trash entry is shadowed by an authoritative V2 Drive object"
                        .to_string(),
                );
            }
            drop(active_v2);

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
            let original_parent = drive_nullable_text(&read, "directory_id")?;
            let original_name = read
                .read::<String, _>("display_name")
                .map_err(|e| e.to_string())?;
            let status = drive_nullable_text(&read, "status")?;
            drop(read);
            if status.as_deref() != Some("complete") {
                return Err("The underlying TeraRelay file is no longer available".to_string());
            }
            let target_parent = restore_parent_if_active(conn, original_parent.as_deref())?;
            let restored_name = available_restored_name(
                conn,
                target_parent.as_deref(),
                &original_name,
                None,
                Some(id),
            )?;
            conn.execute("SAVEPOINT drive_legacy_file_restore")
                .map_err(|e| e.to_string())?;
            let result = (|| {
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
                mark_drive_dirty(conn)?;
                Ok(restored_name)
            })();
            crate::drive_posix::finish_savepoint(conn, "drive_legacy_file_restore", result)
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
            let original_parent = drive_nullable_text(&read, "parent_id")?;
            let original_name = read.read::<String, _>("name").map_err(|e| e.to_string())?;
            drop(read);
            let target_parent = restore_parent_if_active(conn, original_parent.as_deref())?;
            let restored_name = available_restored_name(
                conn,
                target_parent.as_deref(),
                &original_name,
                Some(id),
                None,
            )?;
            conn.execute("SAVEPOINT drive_folder_restore")
                .map_err(|e| e.to_string())?;
            let result = (|| {
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
                mark_drive_dirty(conn)?;
                Ok(restored_name)
            })();
            crate::drive_posix::finish_savepoint(conn, "drive_folder_restore", result)
        }
        _ => Err("Unknown TeraRelay Drive trash item type".to_string()),
    }
}

#[tauri::command]
pub async fn cmd_drive_restore_trash_item(
    kind: String,
    id: String,
    db_pool: State<'_, DbConnection>,
    mount_state: State<'_, crate::drive_mount::DriveMountState>,
) -> Result<String, String> {
    let db = db_pool.inner().clone();
    #[cfg(target_os = "linux")]
    let mount = mount_state.inner().clone();
    #[cfg(not(target_os = "linux"))]
    let _ = mount_state;
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            mount.restore_trash_item(&db, &kind, &id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
            restore_drive_trash_item(&conn, &kind, &id)
        }
    })
    .await
    .map_err(|e| format!("Drive restore worker failed: {e}"))?
}

pub fn delete_drive_pending(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<Option<String>, String> {
    conn.execute("SAVEPOINT drive_pending_discard")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let pending = drive_pending_by_id(conn, pending_id)?;
        if let Some(record) = pending.as_ref() {
            if let Some(base) = record.base_entry_id.as_deref() {
                crate::drive_posix::retain_pending_permission(conn, pending_id, base)?;
            }
        }
        delete_drive_pending_state(conn, pending_id)?;
        let mut delete = conn
            .prepare("DELETE FROM drive_pending_files WHERE id = ?")
            .map_err(|e| e.to_string())?;
        delete.bind((1, pending_id)).map_err(|e| e.to_string())?;
        delete.next().map_err(|e| e.to_string())?;
        Ok(pending.map(|record| record.staging_path))
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_pending_discard", result)
}

/// Native unlink of a pending copy-on-write overlay removes the namespace
/// entry, not merely the overlay. Trash the hidden base and discard the
/// pending row in one savepoint so either both changes publish or neither does.
#[cfg(target_os = "linux")]
pub(crate) fn unlink_drive_pending_native(
    conn: &sqlite::Connection,
    pending_id: &str,
) -> Result<DrivePendingRecord, String> {
    conn.execute("SAVEPOINT native_drive_unlink")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let pending = drive_pending_by_id(conn, pending_id)?
            .ok_or_else(|| "Drive unlink source does not exist".to_string())?;
        if let Some(base_entry_id) = pending.base_entry_id.as_deref() {
            trash_drive_file(conn, base_entry_id)?;
        }
        delete_drive_pending(conn, pending_id)?;
        Ok(pending)
    })();
    crate::drive_posix::finish_savepoint(conn, "native_drive_unlink", result)
}

pub fn trash_drive_directory(conn: &sqlite::Connection, id: &str) -> Result<(), String> {
    let mut links = conn
        .prepare("SELECT 1 FROM drive_symlinks WHERE directory_id=? AND trashed_at IS NULL LIMIT 1")
        .map_err(|e| e.to_string())?;
    links.bind((1, id)).map_err(|e| e.to_string())?;
    if links.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        return Err("Folder is not empty".into());
    }
    drop(links);
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
               AND NOT EXISTS (
                   SELECT 1 FROM drive_entries_v2 shadow
                   JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                   WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
               )
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
            "SELECT 1 FROM drive_entries_v2 e
             JOIN drive_objects o ON o.object_id = e.object_id
             WHERE e.directory_id = ? AND e.trashed_at IS NULL
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

    // The folder change and its sync revision must either both persist or both roll back.
    // This savepoint also nests safely inside an existing native rename transaction.
    conn.execute("SAVEPOINT drive_folder_trash")
        .map_err(|e| e.to_string())?;
    let result = (|| {
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
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_folder_trash", result)
}

// Legacy and V2 Drive entries use the same logical ID namespace. A fresh
// upload must not shadow an existing entry in the other table, including Trash.
fn ensure_fresh_drive_entry_id_available(
    conn: &sqlite::Connection,
    entry_id: &str,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM drive_file_entries WHERE entry_id = ?1
             UNION ALL
             SELECT 1 FROM drive_entries_v2 WHERE entry_id = ?1
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, entry_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if stmt.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
        return Err("Drive upload entry ID already exists".to_string());
    }
    Ok(())
}

fn finalize_legacy_upload_metadata(
    conn: &sqlite::Connection,
    pending_id: &str,
    logical_file_id: &str,
) -> Result<String, String> {
    let mut pending = conn
        .prepare(
            "SELECT directory_id, display_name, staging_path, created_at
             FROM drive_pending_files
             WHERE id = ?
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    pending
        .bind((1, pending_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        pending.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Err("Drive upload staging record no longer exists".to_string());
    }
    let directory_id = drive_nullable_text(&pending, "directory_id")?;
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
        .bind((1, logical_file_id))
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

    // Finalizing a completed Telegram upload must atomically publish the
    // Drive entry, retire its pending row and update the sync revision.
    conn.execute("SAVEPOINT drive_legacy_upload_finalize")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        ensure_fresh_drive_entry_id_available(conn, pending_id)?;
        let now = now_ms();
        let mut insert = conn
            .prepare(
                "INSERT INTO drive_file_entries
             (entry_id, file_id, directory_id, display_name, created_at, updated_at, trashed_at,
              encryption_version, plaintext_size, crypto_id)
             VALUES (?, ?, ?, ?, ?, ?, NULL, 0, NULL, NULL)",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((2, logical_file_id))
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
            .bind((1, pending_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        delete.next().map_err(|e: sqlite::Error| e.to_string())?;
        mark_drive_dirty(conn)?;
        drop(delete);
        Ok(staging_path)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_legacy_upload_finalize", result)
}

#[tauri::command]
pub fn cmd_drive_finalize_upload(
    pending_id: String,
    logical_file_id: String,
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<(), String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    init_drive_schema(&conn)?;
    let pending = drive_pending_by_id(&conn, &pending_id)?
        .ok_or("Drive upload staging record no longer exists")?;
    let staging_path = finalize_legacy_upload_metadata(&conn, &pending_id, &logical_file_id)?;
    drop(conn);
    notify_native_drive_entry_changed(
        &app_handle,
        pending.directory_id.as_deref(),
        &pending.display_name,
        Some(&pending.id),
    );

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
    let size_for_sqlite = i64::try_from(plaintext_size)
        .map_err(|_| "Drive deduplication plaintext size exceeds SQLite limits".to_string())?;
    if encryption_version != 0
        && encryption_version != crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
    {
        return Err("Unsupported Drive deduplication encryption version".to_string());
    }
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
    stmt.bind((2, size_for_sqlite))
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
            crypto_id: drive_nullable_text(&stmt, "crypto_id")?,
        }))
    } else {
        Ok(None)
    }
}

fn valid_drive_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// Deduplication is only safe when the referenced remote object is complete.
// Otherwise the caller may delete the newly uploaded remote parts after reuse.
fn validate_drive_duplicate_object(
    conn: &sqlite::Connection,
    object_id: &str,
    expected_chunks: u64,
) -> Result<(), String> {
    let mut object = conn
        .prepare("SELECT block_count FROM drive_objects WHERE object_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    object
        .bind((1, object_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if object.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row
        || drive_nonnegative_integer(&object, "block_count")? != expected_chunks
    {
        return Err("Drive V2 deduplication source has an invalid block count".to_string());
    }
    drop(object);

    let blocks = drive_object_blocks(conn, object_id)?;
    let parts = drive_object_parts(conn, object_id)?;
    let expected_block_len = usize::try_from(expected_chunks)
        .map_err(|_| "Drive V2 deduplication block count exceeds memory limits".to_string())?;
    let expected_part_len =
        usize::try_from(crate::drive_stream::remote_part_count(expected_chunks))
            .map_err(|_| "Drive V2 deduplication part count exceeds memory limits".to_string())?;
    if blocks.len() != expected_block_len || parts.len() != expected_part_len {
        return Err(
            "Drive V2 deduplication source is missing remote block or part metadata".to_string(),
        );
    }
    for (part_index, part) in parts.iter().enumerate() {
        let index = part_index as u64;
        let (first, end) = crate::drive_stream::remote_part_chunk_range(index, expected_chunks)
            .ok_or_else(|| "Drive V2 deduplication source part layout is invalid".to_string())?;
        if part.part_index != index
            || part.first_chunk_index != first
            || part.chunk_count != end - first
            || part.backing_channel_id <= 0
            || part.message_id <= 0
            || part.ciphertext_size == 0
            || !valid_drive_hex(&part.sha256, 64)
        {
            return Err("Drive V2 deduplication source remote part is invalid".to_string());
        }
        let mut offset = 0u64;
        for (local_index, block) in blocks[first as usize..end as usize].iter().enumerate() {
            if block.chunk_index != first + local_index as u64
                || block.part_index != index
                || block.part_offset != offset
                || block.ciphertext_size == 0
                || !valid_drive_hex(&block.sha256, 64)
                || !valid_drive_hex(&block.plaintext_sha256, 64)
            {
                return Err("Drive V2 deduplication source remote block is invalid".to_string());
            }
            offset = offset.checked_add(block.ciphertext_size).ok_or_else(|| {
                "Drive V2 deduplication source ciphertext size overflow".to_string()
            })?;
        }
        if part.ciphertext_size != offset {
            return Err(
                "Drive V2 deduplication source part size does not match its blocks".to_string(),
            );
        }
    }
    Ok(())
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
                "SELECT object_id, crypto_id FROM drive_objects
                 WHERE content_fingerprint = ? AND plaintext_size = ? AND encryption_version = ?
                 ORDER BY created_at ASC",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, content_fingerprint))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, pending.size as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((3, pending.encryption_version as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        let mut duplicate = None;
        while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            // A matching content fingerprint is not enough to reuse ciphertext
            // encrypted under another key. Reject corrupted stored identities.
            let crypto_id = drive_nullable_text(&stmt, "crypto_id")?;
            if crypto_id.as_deref() == pending.crypto_id.as_deref() {
                duplicate = Some(
                    stmt.read::<String, _>("object_id")
                        .map_err(|e| e.to_string())?,
                );
                break;
            }
        }
        duplicate
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
        if reused_existing_object {
            validate_drive_duplicate_object(conn, &object_id, expected_chunks)?;
        }
        if let Some(base_entry_id) = pending.base_entry_id.as_deref() {
            // The source may have been deleted or trashed while its local
            // copy-on-write editor was open. Never resurrect a stale alias.
            if crate::drive_lookup::file(conn, base_entry_id)?.is_none() {
                return Err("Drive copy-on-write source entry is no longer active".to_string());
            }
            // Revalidate inside the finalize transaction: a restore/sync
            // could have introduced an inode group after editing began.
            // Never publish a new object for just one hardlink alias.
            reject_linked_v2_entry_cow(conn, base_entry_id)?;
        } else {
            ensure_fresh_drive_entry_id_available(conn, &entry_id)?;
        }
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
                    trashed_at = NULL
                 WHERE ? AND drive_entries_v2.trashed_at IS NULL",
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
        entry
            .bind((7, i64::from(pending.base_entry_id.is_some())))
            .map_err(|e: sqlite::Error| e.to_string())?;
        entry.next().map_err(|e: sqlite::Error| e.to_string())?;
        drop(entry);
        // A new upload must not overwrite a row reserved by another entry,
        // even if that row is trashed. Roll back without consuming the pending.
        let mut changed = conn
            .prepare("SELECT changes()")
            .map_err(|e: sqlite::Error| e.to_string())?;
        changed.next().map_err(|e: sqlite::Error| e.to_string())?;
        if changed.read::<i64, _>(0).map_err(|e| e.to_string())? != 1 {
            return Err("Drive upload entry ID already exists".to_string());
        }
        drop(changed);

        if pending.base_entry_id.is_some() {
            let mut legacy = conn
                .prepare("DELETE FROM drive_file_entries WHERE entry_id = ?")
                .map_err(|e: sqlite::Error| e.to_string())?;
            legacy
                .bind((1, entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            legacy.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        crate::drive_posix::transfer_pending_permission(conn, pending_id, &entry_id)?;
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

    let plaintext_size = i64::try_from(plaintext_size)
        .map_err(|_| "Drive file size exceeds SQLite integer limits".to_string())?;
    // A remote logical-file row can exist while upload or recovery is unfinished.
    // Do not discard the only pending upload in favor of an unreadable backing file.
    let mut complete_file = conn
        .prepare("SELECT 1 FROM logical_files WHERE file_id = ? AND status = 'complete' LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    complete_file
        .bind((1, logical_file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if complete_file
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
        != sqlite::State::Row
    {
        return Err("Uploaded logical file is not indexed as complete".to_string());
    }
    drop(complete_file);
    let effective_crypto_id = crypto_id_override.or(pending.crypto_id.as_deref());
    if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
        && effective_crypto_id.is_none()
    {
        return Err("Encrypted Drive entry is missing its crypto identity".to_string());
    }

    conn.execute("SAVEPOINT drive_v1_finalize")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        if pending.base_entry_id.is_none() {
            ensure_fresh_drive_entry_id_available(conn, &pending.id)?;
        }
        let now = now_ms();
        if let Some(base_entry_id) = pending.base_entry_id.as_deref() {
            let mut update = conn
                .prepare(
                    "UPDATE drive_file_entries
                 SET file_id = ?, directory_id = ?, display_name = ?, updated_at = ?,
                     trashed_at = NULL, encryption_version = ?, plaintext_size = ?,
                     crypto_id = ?, content_fingerprint = ?
                 WHERE entry_id = ? AND trashed_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_entries_v2 shadow
                       WHERE shadow.entry_id = drive_file_entries.entry_id
                   )",
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
                .bind((6, plaintext_size))
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
            drop(update);
            let mut updated = conn
                .prepare("SELECT changes()")
                .map_err(|e: sqlite::Error| e.to_string())?;
            updated.next().map_err(|e: sqlite::Error| e.to_string())?;
            if updated.read::<i64, _>(0).map_err(|e| e.to_string())? != 1 {
                return Err("Drive copy-on-write source entry is no longer active".to_string());
            }
            drop(updated);
        } else {
            let mut insert = conn
                .prepare(
                    "INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                  trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
                 VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)",
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
                .bind((8, plaintext_size))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((9, effective_crypto_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert
                .bind((10, content_fingerprint))
                .map_err(|e: sqlite::Error| e.to_string())?;
            insert.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        crate::drive_posix::transfer_pending_permission(
            conn,
            pending_id,
            pending.base_entry_id.as_deref().unwrap_or(&pending.id),
        )?;
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
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_v1_finalize", result)
}

const MAX_DRIVE_PROVIDER_LOCAL_STATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DRIVE_PROVIDER_PROJECTION_RECORDS: usize = 100_000;

pub fn load_drive_provider_projection(
    conn: &sqlite::Connection,
    provider: &str,
    expected_schema_version: i64,
) -> Result<Option<Vec<crate::drive_fs_core::DriveProjectionRecord>>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT schema_version, length(state_json) AS state_bytes, state_json
             FROM drive_provider_local_state WHERE provider = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, provider))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    let schema_version = stmt
        .read::<i64, _>("schema_version")
        .map_err(|e| e.to_string())?;
    if schema_version != expected_schema_version {
        return Err(format!(
            "Drive provider checkpoint schema mismatch: expected {expected_schema_version}, found {schema_version}"
        ));
    }
    let state_bytes = stmt
        .read::<i64, _>("state_bytes")
        .map_err(|e| e.to_string())?;
    if state_bytes < 0 || state_bytes as usize > MAX_DRIVE_PROVIDER_LOCAL_STATE_BYTES {
        return Err("Drive provider checkpoint exceeds the safe size limit".to_string());
    }
    let payload = stmt
        .read::<String, _>("state_json")
        .map_err(|e| e.to_string())?;
    let records: Vec<crate::drive_fs_core::DriveProjectionRecord> = serde_json::from_str(&payload)
        .map_err(|e| format!("Drive provider checkpoint JSON is invalid: {e}"))?;
    if records.len() > MAX_DRIVE_PROVIDER_PROJECTION_RECORDS {
        return Err("Drive provider checkpoint contains too many records".to_string());
    }
    let mut identities = HashSet::new();
    if records.iter().any(|record| !identities.insert(&record.id)) {
        return Err("Drive provider checkpoint contains duplicate identities".to_string());
    }
    Ok(Some(records))
}

pub fn save_drive_provider_projection(
    conn: &sqlite::Connection,
    provider: &str,
    schema_version: i64,
    records: &[crate::drive_fs_core::DriveProjectionRecord],
) -> Result<(), String> {
    if records.len() > MAX_DRIVE_PROVIDER_PROJECTION_RECORDS {
        return Err("Drive provider checkpoint contains too many records".to_string());
    }
    let mut identities = HashSet::new();
    if records.iter().any(|record| !identities.insert(&record.id)) {
        return Err("Drive provider checkpoint contains duplicate identities".to_string());
    }
    let payload = serde_json::to_string(records)
        .map_err(|e| format!("Could not serialize Drive provider checkpoint: {e}"))?;
    if payload.len() > MAX_DRIVE_PROVIDER_LOCAL_STATE_BYTES {
        return Err("Drive provider checkpoint exceeds the safe size limit".to_string());
    }
    let mut stmt = conn
        .prepare(
            "INSERT INTO drive_provider_local_state(provider, schema_version, state_json, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(provider) DO UPDATE SET
                 schema_version = excluded.schema_version,
                 state_json = excluded.state_json,
                 updated_at = excluded.updated_at",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, provider))
        .and_then(|_| stmt.bind((2, schema_version)))
        .and_then(|_| stmt.bind((3, payload.as_str())))
        .and_then(|_| stmt.bind((4, now_ms())))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

fn validate_directory_topology(directories: &[DriveDirectoryRecord]) -> Result<(), String> {
    let parents: HashMap<&str, Option<&str>> = directories
        .iter()
        .map(|directory| (directory.id.as_str(), directory.parent_id.as_deref()))
        .collect();
    let mut resolved_depths: HashMap<&str, usize> = HashMap::new();

    for directory in directories {
        if resolved_depths.contains_key(directory.id.as_str()) {
            continue;
        }
        let mut current = directory.id.as_str();
        let mut chain = Vec::new();
        let mut active = HashSet::new();
        let base_depth = loop {
            if let Some(depth) = resolved_depths.get(current) {
                break *depth;
            }
            if !active.insert(current) {
                return Err("Drive metadata contains a directory cycle".to_string());
            }
            chain.push(current);
            match parents.get(current).copied().flatten() {
                Some(parent) => {
                    if !parents.contains_key(parent) {
                        return Err("Drive metadata references a missing parent folder".to_string());
                    }
                    current = parent;
                }
                None => break 0,
            }
        };

        let mut depth = base_depth;
        while let Some(id) = chain.pop() {
            depth = depth
                .checked_add(1)
                .ok_or_else(|| "Drive directory depth overflow".to_string())?;
            if depth > MAX_DRIVE_DIRECTORY_DEPTH {
                return Err("Drive metadata directory hierarchy is too deep".to_string());
            }
            resolved_depths.insert(id, depth);
        }
    }
    Ok(())
}

fn validate_prospective_directory_parent(
    conn: &sqlite::Connection,
    directory_id: &str,
    parent_id: Option<&str>,
    is_new: bool,
) -> Result<(), String> {
    if is_new {
        // A fresh leaf cannot change another subtree's depth. Validate only its
        // ancestry; moves and incoming manifests still validate the full graph.
        let mut existing = conn
            .prepare("SELECT 1 FROM drive_directories WHERE id = ? LIMIT 1")
            .map_err(|e| e.to_string())?;
        existing
            .bind((1, directory_id))
            .map_err(|e| e.to_string())?;
        if existing.next().map_err(|e| e.to_string())? == sqlite::State::Row {
            return Err("Drive directory ID already exists".to_string());
        }
        drop(existing);

        let mut parent = parent_id.map(str::to_string);
        let mut visited = HashSet::new();
        visited.insert(directory_id.to_string());
        let mut depth = 1;
        let mut stmt = conn
            .prepare("SELECT parent_id, trashed_at FROM drive_directories WHERE id = ?")
            .map_err(|e| e.to_string())?;
        while let Some(id) = parent {
            if !visited.insert(id.clone()) {
                return Err("Drive metadata contains a directory cycle".to_string());
            }
            depth += 1;
            if depth > MAX_DRIVE_DIRECTORY_DEPTH {
                return Err("Drive metadata directory hierarchy is too deep".to_string());
            }
            stmt.reset().map_err(|e| e.to_string())?;
            stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
            if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
                return Err("Destination folder does not exist".to_string());
            }
            if stmt
                .read::<Option<i64>, _>("trashed_at")
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Err("Destination folder is in trash".to_string());
            }
            parent = stmt
                .read::<Option<String>, _>("parent_id")
                .map_err(|e| e.to_string())?;
        }
        return Ok(());
    }

    let mut directories = load_manifest_directories(conn)?;
    let directory = directories
        .iter_mut()
        .find(|directory| directory.id == directory_id && directory.trashed_at.is_none())
        .ok_or_else(|| "Drive folder does not exist".to_string())?;
    directory.parent_id = parent_id.map(str::to_string);
    validate_directory_topology(&directories)
}

fn drive_timestamps_nonnegative(created_at: i64, updated_at: i64, trashed_at: Option<i64>) -> bool {
    created_at >= 0 && updated_at >= 0 && trashed_at.is_none_or(|value| value >= 0)
}

// Preparatory validator for the next manifest version: deduplicated objects
// are *not* POSIX inodes. Active aliases of the same logical inode must name
// the same immutable content snapshot. Historical tombstones may differ.
fn validate_v2_inode_groups(entries: &[DriveEntryV2Record]) -> Result<(), String> {
    let mut active: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in entries {
        let Some(id) = entry.inode_id.as_deref() else {
            continue;
        };
        if id.len() != 32
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Drive V2 hardlink inode identity is invalid".into());
        }
        if entry.trashed_at.is_none() {
            match active.insert(id, &entry.object_id) {
                Some(existing) if existing != entry.object_id => {
                    return Err(
                        "Drive V2 hardlink inode aliases disagree on their content object".into(),
                    );
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &DriveManifestV1) -> Result<(), String> {
    if manifest.revision < 0 {
        return Err("Drive metadata revision must not be negative".to_string());
    }
    if manifest.schema_version != LEGACY_DRIVE_SCHEMA_VERSION
        && manifest.schema_version != PRE_STORAGE_POOL_DRIVE_SCHEMA_VERSION
        && manifest.schema_version != PRE_PROJECT_DRIVE_SCHEMA_VERSION
        && manifest.schema_version != DRIVE_SCHEMA_VERSION
    {
        return Err(format!(
            "Unsupported TeraRelay Drive metadata version {}",
            manifest.schema_version
        ));
    }
    if manifest.schema_version < DRIVE_SCHEMA_VERSION
        && (!manifest.symlinks.is_empty() || !manifest.permissions.is_empty())
    {
        return Err("Filesystem metadata requires Drive schema 4".into());
    }
    // Older devices discard fields they do not understand. Do not publish or
    // import shared-inode identities via schema 4: a round trip through an old
    // client would silently sever hardlinks. A future schema upgrade must
    // happen together with fully atomic alias-wide writes and POSIX handling.
    if manifest
        .v2_entries
        .iter()
        .any(|entry| entry.inode_id.is_some())
    {
        return Err("Drive hardlink inode aliases require a newer Drive manifest schema".into());
    }
    if let Some(crypto) = manifest.crypto.as_ref() {
        if crypto.version != crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
            return Err(format!(
                "Unsupported TeraRelay Drive encryption metadata version {}",
                crypto.version
            ));
        }
        crate::drive_crypto::validate_envelope_metadata(crypto)?;
    }
    let mut directory_ids = HashSet::new();
    let mut active_namespace = HashSet::new();
    for directory in &manifest.directories {
        if directory.id.is_empty() || !directory_ids.insert(directory.id.clone()) {
            return Err("Drive metadata contains an invalid or duplicate directory ID".to_string());
        }
        if !drive_timestamps_nonnegative(
            directory.created_at,
            directory.updated_at,
            directory.trashed_at,
        ) {
            return Err("Drive folder has invalid timestamps".to_string());
        }
        validate_drive_name(&directory.name)?;
        if directory.trashed_at.is_none()
            && !active_namespace.insert((
                directory.parent_id.clone(),
                portable_name_key(&directory.name),
            ))
        {
            return Err("Drive metadata contains colliding sibling names".to_string());
        }
    }
    for directory in &manifest.directories {
        if let Some(parent) = directory.parent_id.as_deref() {
            if !directory_ids.contains(parent) {
                return Err("Drive metadata references a missing parent folder".to_string());
            }
        }
    }
    validate_directory_topology(&manifest.directories)?;
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
        if !drive_timestamps_nonnegative(entry.created_at, entry.updated_at, entry.trashed_at) {
            return Err("Drive legacy file has invalid timestamps".to_string());
        }
        if entry
            .plaintext_size
            .is_some_and(|size| i64::try_from(size).is_err())
        {
            return Err("Drive metadata plaintext size exceeds SQLite limits".to_string());
        }
        validate_drive_name(&entry.display_name)?;
        if let Some(directory_id) = entry.directory_id.as_deref() {
            if !directory_ids.contains(directory_id) {
                return Err("Drive metadata references a missing file folder".to_string());
            }
        }
        if entry.trashed_at.is_none()
            && !active_namespace.insert((
                entry.directory_id.clone(),
                portable_name_key(&entry.display_name),
            ))
        {
            return Err("Drive metadata contains colliding sibling names".to_string());
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

    if manifest.schema_version < PRE_PROJECT_DRIVE_SCHEMA_VERSION
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
            || channel.created_at < 0
            || channel.updated_at < 0
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
            || object.created_at < 0
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
            if part.ciphertext_size == 0 || i64::try_from(part.ciphertext_size).is_err() {
                return Err("Drive V2 remote part has an invalid ciphertext size".to_string());
            }
            let end_chunk = expected_chunk
                .checked_add(part.chunk_count)
                .ok_or_else(|| "Drive V2 block count overflow".to_string())?;
            if end_chunk > object.block_count {
                return Err("Drive V2 remote part exceeds the object block count".to_string());
            }
            let mut expected_ciphertext_size = 0u64;
            for chunk_index in expected_chunk..end_chunk {
                let plain_start = chunk_index
                    .checked_mul(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
                    .ok_or_else(|| "Drive V2 plaintext offset overflow".to_string())?;
                let plain_len = object
                    .plaintext_size
                    .saturating_sub(plain_start)
                    .min(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
                let encoded_len =
                    crate::drive_crypto::encoded_chunk_len(object.encryption_version, plain_len)?;
                expected_ciphertext_size = expected_ciphertext_size
                    .checked_add(encoded_len)
                    .ok_or_else(|| "Drive V2 remote part size overflow".to_string())?;
            }
            if part.ciphertext_size != expected_ciphertext_size {
                return Err("Drive V2 remote part size does not match its blocks".to_string());
            }
            expected_part = expected_part.saturating_add(1);
            expected_chunk = end_chunk;
        }
        if expected_chunk != object.block_count {
            return Err("Drive V2 remote parts do not cover the complete object".to_string());
        }
        // V3 manifests carry one compact encoded-block checksum per block so a
        // second device can verify random-range reads without fetching a whole
        // remote part first.
        decode_manifest_block_hashes(object)?;
    }

    validate_v2_inode_groups(&manifest.v2_entries)?;
    let mut v2_entry_ids = HashSet::new();
    for entry in &manifest.v2_entries {
        if entry.entry_id.is_empty()
            || entry_ids.contains(&entry.entry_id)
            || !v2_entry_ids.insert(entry.entry_id.clone())
            || !objects_by_id.contains_key(&entry.object_id)
            || !drive_timestamps_nonnegative(entry.created_at, entry.updated_at, entry.trashed_at)
        {
            return Err("Drive metadata contains an invalid or duplicate V2 entry".to_string());
        }
        validate_drive_name(&entry.display_name)?;
        if let Some(directory_id) = entry.directory_id.as_deref() {
            if !directory_ids.contains(directory_id) {
                return Err("Drive V2 metadata references a missing file folder".to_string());
            }
        }
        if entry.trashed_at.is_none()
            && !active_namespace.insert((
                entry.directory_id.clone(),
                portable_name_key(&entry.display_name),
            ))
        {
            return Err("Drive metadata contains colliding sibling names".to_string());
        }
    }
    entry_ids.extend(v2_entry_ids);
    crate::drive_posix::validate_manifest(
        manifest,
        &directory_ids,
        &entry_ids,
        &mut active_namespace,
    )?;
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
        symlinks: crate::drive_posix::load_symlinks(conn, true)?,
        permissions: crate::drive_posix::load_permissions(conn)?,
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
    // A deletion at the same millisecond must not be undone by an offline
    // device advertising the old active version. Break other ties below.
    if candidate.trashed_at.is_some() != current.trashed_at.is_some() {
        return if candidate.trashed_at.is_some() {
            candidate
        } else {
            current
        };
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
    // A deletion at the same millisecond must not be undone by an offline
    // device advertising the old active version. Break other ties below.
    if candidate.trashed_at.is_some() != current.trashed_at.is_some() {
        return if candidate.trashed_at.is_some() {
            candidate
        } else {
            current
        };
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
    // A deletion at the same millisecond must not be undone by an offline
    // device advertising the old active version. Break other ties below.
    if candidate.trashed_at.is_some() != current.trashed_at.is_some() {
        return if candidate.trashed_at.is_some() {
            candidate
        } else {
            current
        };
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
        // A legacy envelope without a master-key verifier cannot prove a
        // rewrap refers to the same key. Its wrapper is comparable only
        // when every value affecting key derivation and decryption matches.
        // Equal ciphertext with a different salt or nonce is not equivalent.
        _ => {
            left.wrapped_master_key_b64 == right.wrapped_master_key_b64
                && left.salt_b64 == right.salt_b64
                && left.nonce_b64 == right.nonce_b64
                && left.memory_kib == right.memory_kib
                && left.iterations == right.iterations
                && left.parallelism == right.parallelism
        }
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

    // A second device may still advertise the V1 entry while another has
    // migrated the same stable entry ID to V2. Each source manifest is valid,
    // but publishing both versions would fail validation and stall sync.
    // Resolve one shared ID deterministically before validating the merged
    // namespace. Newer edits/deletions win; a tied tombstone beats an active
    // record, then V2 wins otherwise so an equal-time migration converges.
    let cross_format_ids: Vec<String> = entries
        .keys()
        .filter(|id| v2_entries.contains_key(*id))
        .cloned()
        .collect();
    for id in cross_format_ids {
        let legacy = &entries[&id];
        let v2 = &v2_entries[&id];
        let keep_v2 = match legacy.updated_at.cmp(&v2.updated_at) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => {
                match (legacy.trashed_at.is_some(), v2.trashed_at.is_some()) {
                    (true, false) => false,
                    (false, true) => true,
                    _ => true,
                }
            }
        };
        if keep_v2 {
            entries.remove(&id);
        } else {
            v2_entries.remove(&id);
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
        symlinks: crate::drive_posix::merge_records(
            &left.symlinks,
            &right.symlinks,
            |r| r.id.clone(),
            |r| r.updated_at,
            |r| r.trashed_at.is_some(),
        ),
        permissions: crate::drive_posix::merge_records(
            &left.permissions,
            &right.permissions,
            |r| format!("{}:{}", r.node_kind, r.node_id),
            |r| r.updated_at,
            |_| false,
        ),
    };
    validate_manifest(&merged)?;
    Ok(merged)
}

fn apply_manifest(conn: &sqlite::Connection, manifest: &DriveManifestV1) -> Result<(), String> {
    apply_manifest_with_cursor(conn, manifest, None)
}

/// Replace remote namespace records and the acknowledged remote cursor in the
/// same SQLite transaction; failure to persist the cursor must roll back the
/// entire remote filesystem update, not leave an unannounced partial sync.
fn apply_manifest_with_cursor(
    conn: &sqlite::Connection,
    manifest: &DriveManifestV1,
    cursor_override: Option<Option<i64>>,
) -> Result<(), String> {
    validate_manifest(manifest)?;
    if manifest.schema_version < DRIVE_SCHEMA_VERSION
        && (!crate::drive_posix::load_symlinks(conn, true)?.is_empty()
            || !crate::drive_posix::load_permissions(conn)?.is_empty())
    {
        return Err(
            "Refusing to downgrade Drive filesystem metadata; merge with schema 4 first".into(),
        );
    }
    crate::drive_storage::init_drive_storage_schema(conn)?;
    let last_remote_message_id = {
        let mut stmt = conn
            .prepare("SELECT last_remote_message_id FROM drive_state WHERE singleton = 1")
            .map_err(|e: sqlite::Error| e.to_string())?;
        if !matches!(
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
            sqlite::State::Row
        ) {
            return Err(
                "Drive sync state row is missing; refusing to replace metadata".to_string(),
            );
        }
        // A corrupted value must not be mistaken for NULL: doing so would
        // overwrite the saved remote sync cursor after manifest replacement.
        match stmt
            .read::<sqlite::Value, _>(0)
            .map_err(|e| format!("Invalid Drive remote sync cursor: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::Integer(id) if id > 0 => Some(id),
            _ => return Err("Invalid Drive remote sync cursor type or value".to_string()),
        }
    };
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
            "DELETE FROM drive_symlinks;
             DELETE FROM drive_permissions WHERE node_kind != 'pending';
             DELETE FROM drive_file_entries;
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
            let plaintext_size = entry
                .plaintext_size
                .map(i64::try_from)
                .transpose()
                .map_err(|_| "Drive metadata plaintext size exceeds SQLite limits".to_string())?;
            stmt.bind((9, plaintext_size))
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
                     (entry_id, object_id, inode_id, directory_id, display_name,
                      created_at, updated_at, trashed_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, entry.entry_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((2, entry.object_id.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((3, entry.inode_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((4, entry.directory_id.as_deref()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((5, entry.display_name.as_str()))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((6, entry.created_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((7, entry.updated_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((8, entry.trashed_at))
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
        }

        for link in &manifest.symlinks {
            crate::drive_posix::upsert_symlink(conn, link)?;
        }
        for permissions in &manifest.permissions {
            crate::drive_posix::upsert_permission(conn, permissions)?;
        }
        crate::drive_crypto::save_crypto_envelope(conn, manifest.crypto.as_ref())?;
        set_drive_clean(
            conn,
            manifest.revision,
            cursor_override.unwrap_or(last_remote_message_id),
        )?;
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

/// Give each concurrent publish a distinct on-disk source. The Telegram
/// document name stays DRIVE_MANIFEST_FILE; only the local staging path differs.
async fn stage_manifest_bytes(root: &std::path::Path, bytes: &[u8]) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;

    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| format!("Failed to create Drive metadata staging root: {e}"))?;

    for _ in 0..8 {
        let parent = root.join(format!("publish-{:032x}", rand::random::<u128>()));
        match tokio::fs::create_dir(&parent).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to reserve Drive metadata staging: {e}")),
        }

        // The staged manifest can include encrypted Drive key metadata.
        // Restrict the newly reserved directory before writing any content.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                tokio::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).await
            {
                let _ = tokio::fs::remove_dir(&parent).await;
                return Err(format!("Failed to protect Drive metadata staging: {e}"));
            }
        }

        let path = parent.join(DRIVE_MANIFEST_FILE);
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|e| format!("Failed to open Drive metadata staging file: {e}"))?;
            file.write_all(bytes)
                .await
                .map_err(|e| format!("Failed to write Drive metadata staging file: {e}"))?;
            file.flush()
                .await
                .map_err(|e| format!("Failed to flush Drive metadata staging file: {e}"))?;
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&path).await;
            let _ = tokio::fs::remove_dir(&parent).await;
            return Err(error);
        }
        return Ok(path);
    }

    Err("Unable to reserve a unique Drive metadata staging directory".to_string())
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
    let path = stage_manifest_bytes(&root, &bytes).await?;
    Ok((path, bytes.len() as u64))
}

async fn publish_manifest(
    client: &Client,
    app: &AppHandle,
    peer: &Peer,
    manifest: &DriveManifestV1,
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
    // Only remove this publish's private staging output. Never let one
    // concurrent sync delete another sync's in-flight manifest bytes.
    let _ = tokio::fs::remove_file(&path).await;
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::remove_dir(parent).await;
    }
    result
}

/// Never prune an earlier remote manifest until the new manifest has been
/// accepted into local Drive metadata. If applying a published snapshot fails
/// its local revision fence, leave the older remote copies available to merge.
fn stale_manifest_ids(previous: &[i32], published: i32) -> Vec<i32> {
    let mut seen = HashSet::new();
    previous
        .iter()
        .copied()
        .filter(|id| *id > 0 && *id != published && seen.insert(*id))
        .collect()
}

async fn cleanup_stale_remote_manifests(
    client: &Client,
    peer: &Peer,
    previous: &[i32],
    published: i32,
) {
    let stale = stale_manifest_ids(previous, published);
    if !stale.is_empty() {
        if let Err(error) = client.delete_messages(peer, &stale).await {
            log::warn!(
                "Applied TeraRelay Drive metadata, but could not delete {} stale copy/copies: {}",
                stale.len(),
                error
            );
        }
    }
}

/// Identify old and new dentry names and cached inode attributes changed by
/// a committed cross-device manifest. The kernel must learn about removals and
/// renames at BOTH paths; merely replacing SQLite rows cannot invalidate FUSE
/// negative dentries or file-manager caches.
#[cfg(any(test, target_os = "linux"))]
type DriveNamespaceName = (Option<String>, String);
#[cfg(any(test, target_os = "linux"))]
type DriveNamespaceInode = (String, String);

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Default, PartialEq, Eq)]
struct DriveNamespaceDelta {
    entries: Vec<DriveNamespaceName>,
    inodes: Vec<DriveNamespaceInode>,
}

#[cfg(any(test, target_os = "linux"))]
fn drive_manifest_namespace_delta(
    before: &DriveManifestV1,
    after: &DriveManifestV1,
) -> DriveNamespaceDelta {
    use std::collections::BTreeSet;

    fn collect_changed<T: Eq>(
        before: &[T],
        after: &[T],
        identity: fn(&T) -> &str,
        location: fn(&T) -> Option<DriveNamespaceName>,
        inode_kind: &str,
        entries: &mut BTreeSet<DriveNamespaceName>,
        inodes: &mut BTreeSet<DriveNamespaceInode>,
    ) {
        let old_by_id: BTreeMap<&str, &T> =
            before.iter().map(|item| (identity(item), item)).collect();
        let new_by_id: BTreeMap<&str, &T> =
            after.iter().map(|item| (identity(item), item)).collect();
        for (id, old) in &old_by_id {
            if new_by_id.get(id).copied() != Some(*old) {
                if let Some(entry) = location(old) {
                    entries.insert(entry);
                }
                inodes.insert((inode_kind.to_owned(), (*id).to_owned()));
            }
        }
        for (id, new) in &new_by_id {
            if old_by_id.get(id).copied() != Some(*new) {
                if let Some(entry) = location(new) {
                    entries.insert(entry);
                }
                inodes.insert((inode_kind.to_owned(), (*id).to_owned()));
            }
        }
    }

    let mut entries = BTreeSet::new();
    let mut inodes = BTreeSet::new();
    collect_changed(
        &before.directories,
        &after.directories,
        |record| &record.id,
        |record| {
            record
                .trashed_at
                .is_none()
                .then(|| (record.parent_id.clone(), record.name.clone()))
        },
        "dir",
        &mut entries,
        &mut inodes,
    );
    collect_changed(
        &before.entries,
        &after.entries,
        |record| &record.entry_id,
        |record| {
            record
                .trashed_at
                .is_none()
                .then(|| (record.directory_id.clone(), record.display_name.clone()))
        },
        "file",
        &mut entries,
        &mut inodes,
    );
    collect_changed(
        &before.v2_entries,
        &after.v2_entries,
        |record| &record.entry_id,
        |record| {
            record
                .trashed_at
                .is_none()
                .then(|| (record.directory_id.clone(), record.display_name.clone()))
        },
        "file",
        &mut entries,
        &mut inodes,
    );
    collect_changed(
        &before.symlinks,
        &after.symlinks,
        |record| &record.id,
        |record| {
            record
                .trashed_at
                .is_none()
                .then(|| (record.directory_id.clone(), record.display_name.clone()))
        },
        "file",
        &mut entries,
        &mut inodes,
    );

    // Mode-only edits do not rename a node, but stat(2) must not keep an old
    // positive inode attribute TTL after a remote chmod.
    let old_modes: BTreeMap<(&str, &str), &crate::drive_posix::DrivePermissionRecord> = before
        .permissions
        .iter()
        .map(|p| ((p.node_kind.as_str(), p.node_id.as_str()), p))
        .collect();
    for permission in &after.permissions {
        let key = (permission.node_kind.as_str(), permission.node_id.as_str());
        if old_modes.get(&key).copied() != Some(permission) {
            let kind = if permission.node_kind == "directory" {
                "dir"
            } else {
                "file"
            };
            inodes.insert((kind.into(), permission.node_id.clone()));
        }
    }
    let new_modes: BTreeMap<(&str, &str), &crate::drive_posix::DrivePermissionRecord> = after
        .permissions
        .iter()
        .map(|p| ((p.node_kind.as_str(), p.node_id.as_str()), p))
        .collect();
    for permission in &before.permissions {
        let key = (permission.node_kind.as_str(), permission.node_id.as_str());
        if !new_modes.contains_key(&key) {
            let kind = if permission.node_kind == "directory" {
                "dir"
            } else {
                "file"
            };
            inodes.insert((kind.into(), permission.node_id.clone()));
        }
    }
    DriveNamespaceDelta {
        entries: entries.into_iter().collect(),
        inodes: inodes.into_iter().collect(),
    }
}

fn notify_native_drive_metadata_changed(
    app_handle: &AppHandle,
    before: Option<&DriveManifestV1>,
    after: Option<&DriveManifestV1>,
) {
    #[cfg(target_os = "linux")]
    if let (Some(before), Some(after)) = (before, after) {
        if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
            let delta = drive_manifest_namespace_delta(before, after);
            mount.notify_namespace_changed(&delta.entries, &delta.inodes);
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (before, after);

    #[cfg(target_os = "windows")]
    if let Err(error) = crate::drive_windows::refresh() {
        log::warn!("Could not refresh Windows TeraRelay Drive after metadata sync: {error}");
    }

    #[cfg(target_os = "macos")]
    if let Err(error) = crate::drive_provider_ipc::signal_changes(app_handle) {
        log::warn!("Could not signal macOS TeraRelay Drive after metadata sync: {error}");
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = app_handle;
}

/// Dispatch only after the SQLite commit and after releasing the DB mutex.
/// A running Linux FUSE session must not rely on a restart to invalidate its
/// cached positive/negative directory entries.
pub(crate) fn notify_native_drive_entry_changed(
    app_handle: &AppHandle,
    parent: Option<&str>,
    name: &str,
    file_entry_id: Option<&str>,
) {
    #[cfg(target_os = "linux")]
    if let Some(mount) = app_handle.try_state::<crate::drive_mount::DriveMountState>() {
        let changed = [(parent.map(str::to_owned), name.to_owned())];
        let inodes: Vec<(String, String)> = file_entry_id
            .map(|id| ("file".to_string(), id.to_owned()))
            .into_iter()
            .collect();
        mount.notify_namespace_changed(&changed, &inodes);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (parent, name, file_entry_id);
    notify_native_drive_metadata_changed(app_handle, None, None);
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
            None => local_manifest.clone(),
        };
        manifest.revision = manifest
            .revision
            .max(local_revision)
            .max(now_ms())
            .saturating_add(1);
        let stale_ids: Vec<i32> = remote.iter().map(|(id, _)| *id).collect();
        let message_id = publish_manifest(&client, &app_handle, &peer, &manifest).await?;
        {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            ensure_drive_sync_snapshot_current(&conn, local_revision, local_dirty)?;
            apply_manifest_with_cursor(&conn, &manifest, Some(Some(message_id as i64)))?;
        }
        cleanup_stale_remote_manifests(&client, &peer, &stale_ids, message_id).await;
        notify_native_drive_metadata_changed(&app_handle, Some(&local_manifest), Some(&manifest));
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
            let message_id = publish_manifest(&client, &app_handle, &peer, &manifest).await?;
            {
                let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
                ensure_drive_sync_snapshot_current(&conn, local_revision, local_dirty)?;
                apply_manifest_with_cursor(&conn, &manifest, Some(Some(message_id as i64)))?;
            }
            cleanup_stale_remote_manifests(&client, &peer, &stale_ids, message_id).await;
            notify_native_drive_metadata_changed(
                &app_handle,
                Some(&local_manifest),
                Some(&manifest),
            );
            return Ok(DriveSyncResult {
                revision: manifest.revision,
                dirty: false,
                source: "remote-merged".to_string(),
            });
        }

        if manifest.revision > local_revision {
            let message_id = remote.first().map(|(id, _)| i64::from(*id));
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            ensure_drive_sync_snapshot_current(&conn, local_revision, local_dirty)?;
            apply_manifest_with_cursor(&conn, &manifest, Some(message_id))?;
            drop(conn);
            notify_native_drive_metadata_changed(
                &app_handle,
                Some(&local_manifest),
                Some(&manifest),
            );
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
            ensure_drive_sync_snapshot_current(&conn, local_revision, local_dirty)?;
            apply_manifest(&conn, &manifest)?;
            drop(conn);
            notify_native_drive_metadata_changed(
                &app_handle,
                Some(&local_manifest),
                Some(&manifest),
            );
        }
    }

    // Even a no-op result must not describe an obsolete snapshot as clean.
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    ensure_drive_sync_snapshot_current(&conn, local_revision, local_dirty)?;
    Ok(DriveSyncResult {
        revision: local_revision,
        dirty: false,
        source: "current".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_remote_cursor_write_rolls_back_imported_namespace_atomically() {
        let conn = memory_db();
        let original = build_manifest(&conn).unwrap();
        let state_before = state_row(&conn).unwrap();
        let mut incoming = original.clone();
        incoming.revision += 10;
        incoming.directories.push(DriveDirectoryRecord {
            id: "remote-added-folder".into(),
            parent_id: None,
            name: "Cross-device".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        conn.execute(
            "CREATE TRIGGER qa_fail_remote_cursor_write
             BEFORE UPDATE OF last_remote_message_id ON drive_state
             WHEN NEW.last_remote_message_id = 9999
             BEGIN SELECT RAISE(ABORT, 'qa remote cursor cannot commit'); END;",
        )
        .unwrap();
        let error = apply_manifest_with_cursor(&conn, &incoming, Some(Some(9999)))
            .expect_err("an unacknowledged manifest must not persist");
        assert!(error.contains("qa remote cursor cannot commit"), "{error}");
        assert_eq!(state_row(&conn).unwrap(), state_before);
        assert_eq!(build_manifest(&conn).unwrap(), original);
        assert_eq!(drive_snapshot(&conn).unwrap().directories.len(), 0);

        conn.execute("DROP TRIGGER qa_fail_remote_cursor_write")
            .unwrap();
        apply_manifest_with_cursor(&conn, &incoming, Some(Some(9999))).unwrap();
        let applied = build_manifest(&conn).unwrap();
        assert_eq!(applied.directories.len(), 1);
        assert_eq!(applied.directories[0].name, "Cross-device");
        assert_eq!(state_row(&conn).unwrap(), (incoming.revision, false));
        let mut cursor = conn
            .prepare("SELECT last_remote_message_id FROM drive_state WHERE singleton=1")
            .unwrap();
        assert_eq!(cursor.next().unwrap(), sqlite::State::Row);
        assert_eq!(cursor.read::<i64, _>(0).unwrap(), 9999);
    }

    #[test]
    fn remote_drive_namespace_delta_invalidates_both_sides_of_rename_and_delete() {
        let conn = memory_db();
        let mut before = build_manifest(&conn).unwrap();
        before.directories.push(DriveDirectoryRecord {
            id: "folder-one".into(),
            parent_id: None,
            name: "Old folder".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        before.entries.push(DriveEntryRecord {
            entry_id: "existing-entry".into(),
            file_id: "original-bytes".into(),
            directory_id: Some("folder-one".into()),
            display_name: "Before.bin".into(),
            created_at: 1,
            updated_at: 1,
            encryption_version: 0,
            plaintext_size: Some(1),
            crypto_id: None,
            content_fingerprint: None,
            trashed_at: None,
        });
        let mut after = before.clone();
        after.revision += 10;
        after.directories[0].name = "New folder".into();
        after.directories[0].updated_at = 2;
        after.entries[0].directory_id = None;
        after.entries[0].display_name = "After.bin".into();
        after.entries[0].updated_at = 2;
        let delta = drive_manifest_namespace_delta(&before, &after);
        assert_eq!(
            delta.entries,
            vec![
                (None, "After.bin".into()),
                (None, "New folder".into()),
                (None, "Old folder".into()),
                (Some("folder-one".into()), "Before.bin".into()),
            ]
        );
        assert_eq!(
            delta.inodes,
            vec![
                ("dir".into(), "folder-one".into()),
                ("file".into(), "existing-entry".into())
            ]
        );
        // Revision-only sync must not flush the mounted filesystem cache.
        let mut equivalent = before.clone();
        equivalent.revision += 1;
        assert_eq!(
            drive_manifest_namespace_delta(&before, &equivalent),
            DriveNamespaceDelta::default()
        );

        after.entries[0].trashed_at = Some(5);
        let trashed = drive_manifest_namespace_delta(&before, &after);
        assert!(trashed
            .entries
            .contains(&(Some("folder-one".into()), "Before.bin".into())));
        assert!(!trashed.entries.contains(&(None, "After.bin".into())));
    }

    #[test]
    fn remote_drive_namespace_delta_catches_content_permissions_and_symlinks() {
        let conn = memory_db();
        let mut before = build_manifest(&conn).unwrap();
        before.v2_entries.push(DriveEntryV2Record {
            entry_id: "v2-file".into(),
            object_id: "old-object".into(),
            inode_id: None,
            directory_id: None,
            display_name: "Movie.mkv".into(),
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        before
            .symlinks
            .push(crate::drive_posix::DriveSymlinkRecord {
                id: "link-one".into(),
                directory_id: None,
                display_name: "Shortcut".into(),
                target: "first".into(),
                created_at: 1,
                updated_at: 1,
                trashed_at: None,
            });
        before
            .permissions
            .push(crate::drive_posix::DrivePermissionRecord {
                node_kind: "directory".into(),
                node_id: "folder-mode-only".into(),
                mode: 0o750,
                updated_at: 1,
            });
        let mut after = before.clone();
        after.v2_entries[0].object_id = "replacement-object".into();
        after.v2_entries[0].updated_at = 2;
        after.symlinks[0].target = "second".into();
        after.symlinks[0].updated_at = 2;
        after.permissions[0].mode = 0o700;
        after.permissions[0].updated_at = 2;
        let delta = drive_manifest_namespace_delta(&before, &after);
        assert_eq!(
            delta.entries,
            vec![(None, "Movie.mkv".into()), (None, "Shortcut".into())]
        );
        assert_eq!(
            delta.inodes,
            vec![
                ("dir".into(), "folder-mode-only".into()),
                ("file".into(), "link-one".into()),
                ("file".into(), "v2-file".into()),
            ]
        );
        let unchanged = drive_manifest_namespace_delta(&after, &after);
        assert_eq!(unchanged, DriveNamespaceDelta::default());
    }

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

    #[test]
    fn drive_v2_inode_identity_migrates_old_rows_without_rewriting_existing_entries() {
        let conn = memory_db();
        // Emulate a real pre-hardlink database schema rather than discarding
        // existing files or replacing the user's database.
        conn.execute(
            "DROP TABLE drive_entries_v2;
             CREATE TABLE drive_entries_v2 (
                 entry_id TEXT PRIMARY KEY, object_id TEXT NOT NULL,
                 directory_id TEXT, display_name TEXT NOT NULL,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 trashed_at INTEGER
             );
             INSERT INTO drive_entries_v2
                 (entry_id,object_id,display_name,created_at,updated_at)
             VALUES ('legacy-alias','existing-object','legacy.bin',10,11);",
        )
        .unwrap();
        init_drive_schema(&conn).unwrap();
        let entries = load_manifest_v2_entries(&conn).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_id, "legacy-alias");
        assert_eq!(entries[0].object_id, "existing-object");
        assert_eq!(entries[0].inode_id, None);
        assert!(
            !serde_json::to_string(&entries[0])
                .unwrap()
                .contains("inode_id"),
            "older manifests must remain byte-shape-compatible when no hardlinks exist"
        );
        // Run the idempotent migration again: it must not overwrite data.
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
             WHERE entry_id='legacy-alias'",
        )
        .unwrap();
        let restored = load_manifest_v2_entries(&conn).unwrap();
        assert_eq!(
            restored[0].inode_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let json = serde_json::to_string(&restored[0]).unwrap();
        assert_eq!(
            serde_json::from_str::<DriveEntryV2Record>(&json)
                .unwrap()
                .inode_id,
            restored[0].inode_id
        );
    }

    #[test]
    fn hardlink_group_validator_distinguishes_content_dedup_from_shared_inodes() {
        let legacy = serde_json::json!({
            "entry_id": "first", "object_id": "same-object",
            "directory_id": null, "display_name": "first.bin",
            "created_at": 10, "updated_at": 10
        });
        let first: DriveEntryV2Record = serde_json::from_value(legacy).unwrap();
        assert_eq!(first.inode_id, None);
        let mut dedup_copy = first.clone();
        dedup_copy.entry_id = "second".into();
        dedup_copy.display_name = "second.bin".into();
        assert!(
            validate_v2_inode_groups(&[first.clone(), dedup_copy.clone()]).is_ok(),
            "sharing immutable bytes alone is not a hardlink"
        );

        let shared_inode = "1234567890abcdef1234567890abcdef";
        let mut linked = first.clone();
        linked.inode_id = Some(shared_inode.into());
        let mut alias = dedup_copy.clone();
        alias.inode_id = Some(shared_inode.into());
        assert!(validate_v2_inode_groups(&[linked.clone(), alias.clone()]).is_ok());

        alias.object_id = "changed-without-sharing".into();
        assert!(
            validate_v2_inode_groups(&[linked.clone(), alias.clone()])
                .unwrap_err()
                .contains("disagree on their content object"),
            "separate COW writes to one alias must never silently split an inode"
        );
        // A deleted historical alias may point at an older snapshot.
        alias.trashed_at = Some(11);
        assert!(validate_v2_inode_groups(&[linked.clone(), alias]).is_ok());

        linked.inode_id = Some("Z".repeat(32));
        assert!(validate_v2_inode_groups(&[linked.clone()]).is_err());
        linked.inode_id = Some("abc".into());
        assert!(validate_v2_inode_groups(&[linked]).is_err());
    }

    #[test]
    fn schema_four_never_publishes_shared_inode_metadata_that_old_clients_would_drop() {
        let conn = memory_db();
        let mut manifest = build_manifest(&conn).unwrap();
        let mut entry: DriveEntryV2Record = serde_json::from_value(serde_json::json!({
            "entry_id": "not-yet-linked",
            "object_id": "0123456789abcdef0123456789abcdef",
            "display_name": "fixture.bin",
            "created_at": 1,
            "updated_at": 1
        }))
        .unwrap();
        assert!(entry.inode_id.is_none());
        entry.inode_id = Some("1234567890abcdef1234567890abcdef".into());
        manifest.v2_entries.push(entry);
        let error = validate_manifest(&manifest).unwrap_err();
        assert!(error.contains("newer Drive manifest schema"), "{error}");
    }

    #[test]
    fn v2_inode_identity_survives_snapshot_direct_targeted_and_child_lookups() {
        let conn = memory_db();
        let entry_id = "qa-inode-lookup-source";
        let object_id = "abcdefabcdefabcdefabcdefabcdefab";
        let inode_id = "1234567890abcdef1234567890abcdef";
        insert_v2_test_file(&conn, entry_id, object_id, None, "linked.bin", 202, 9001);
        conn.execute(format!(
            "UPDATE drive_entries_v2 SET inode_id='{inode_id}' WHERE entry_id='{entry_id}'"
        ))
        .unwrap();

        let direct = drive_file_by_entry(&conn, entry_id).unwrap().unwrap();
        assert_eq!(direct.inode_id.as_deref(), Some(inode_id));
        assert_eq!(direct.file_id, object_id);
        let snapshot = drive_snapshot(&conn).unwrap();
        let snap = snapshot
            .files
            .iter()
            .find(|file| file.entry_id == entry_id)
            .unwrap();
        assert_eq!(snap.inode_id.as_deref(), Some(inode_id));
        let indexed = crate::drive_lookup::file(&conn, entry_id).unwrap().unwrap();
        assert_eq!(indexed.inode_id.as_deref(), Some(inode_id));
        match crate::drive_lookup::regular_file_entry(&conn, entry_id)
            .unwrap()
            .unwrap()
        {
            crate::drive_lookup::RegularFileEntry::File(file) => {
                assert_eq!(file.inode_id.as_deref(), Some(inode_id));
            }
            crate::drive_lookup::RegularFileEntry::Pending(_) => {
                panic!("unexpected pending writer")
            }
        }
        match crate::drive_lookup::child_entry(&conn, None, "linked.bin")
            .unwrap()
            .unwrap()
        {
            crate::drive_lookup::DriveChildEntry::File(file) => {
                assert_eq!(file.inode_id.as_deref(), Some(inode_id));
            }
            _ => panic!("unexpected child kind"),
        }

        // A distinct directory entry pointing at the exact same immutable
        // object is a copy, not automatically a POSIX hardlink.
        conn.execute(format!(
            "INSERT INTO drive_entries_v2
             (entry_id, object_id, display_name, created_at, updated_at)
             VALUES ('qa-inode-independent-copy','{object_id}','copy.bin',10,11)"
        ))
        .unwrap();
        let copied = drive_file_by_entry(&conn, "qa-inode-independent-copy")
            .unwrap()
            .unwrap();
        assert_eq!(copied.inode_id, None);
        assert_eq!(copied.file_id, direct.file_id);
        assert_eq!(
            crate::drive_lookup::child_entry(&conn, None, "copy.bin")
                .unwrap()
                .and_then(|child| match child {
                    crate::drive_lookup::DriveChildEntry::File(file) => Some(file.inode_id),
                    _ => None,
                }),
            Some(None)
        );
    }

    #[test]
    fn corrupt_v2_inode_type_cannot_be_silently_treated_as_independent_file() {
        let conn = memory_db();
        insert_v2_test_file(
            &conn,
            "qa-corrupt-inode-id",
            "abcdefabcdefabcdefabcdefabcdefac",
            None,
            "corrupt-inode.bin",
            202,
            9002,
        );
        conn.execute(
            "UPDATE drive_entries_v2 SET inode_id=X'FF'
             WHERE entry_id='qa-corrupt-inode-id'",
        )
        .unwrap();
        assert!(drive_file_by_entry(&conn, "qa-corrupt-inode-id").is_err());
        assert!(drive_snapshot(&conn).is_err());
        assert!(crate::drive_lookup::file(&conn, "qa-corrupt-inode-id").is_err());
        assert!(crate::drive_lookup::regular_file_entry(&conn, "qa-corrupt-inode-id").is_err());
        assert!(crate::drive_lookup::child_entry(&conn, None, "corrupt-inode.bin").is_err());
    }

    #[test]
    fn shared_inode_guard_blocks_per_entry_chmod_rename_trash_and_restore_without_mutation() {
        let conn = memory_db();
        let id = "future-hardlink-guard";
        insert_v2_test_file(
            &conn,
            id,
            "99999999aaaaaaaa11111111bbbbbbbb",
            None,
            "linked-source.bin",
            202,
            9001,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id='1234567890abcdef1234567890abcdef'
             WHERE entry_id='future-hardlink-guard'",
        )
        .unwrap();
        let before = state_row(&conn).unwrap();
        let error = rename_drive_file(&conn, id, None, "rename-should-fail.bin").unwrap_err();
        assert!(
            error.contains("requires shared-inode POSIX support"),
            "{error}"
        );
        let error = trash_drive_file(&conn, id).unwrap_err();
        assert!(
            error.contains("requires shared-inode POSIX support"),
            "{error}"
        );
        let error = crate::drive_posix::set_permission(&conn, "file", id, 0o600).unwrap_err();
        assert!(
            error.contains("requires shared-inode POSIX support"),
            "{error}"
        );
        assert_eq!(state_row(&conn).unwrap(), before);
        let file = drive_file_by_entry(&conn, id).unwrap().unwrap();
        assert_eq!(file.display_name, "linked-source.bin");
        let mut permission = conn
            .prepare("SELECT COUNT(*) FROM drive_permissions WHERE node_kind='file' AND node_id=?")
            .unwrap();
        permission.bind((1, id)).unwrap();
        assert_eq!(permission.next().unwrap(), sqlite::State::Row);
        assert_eq!(permission.read::<i64, _>(0).unwrap(), 0);
        drop(permission);

        // Tombstone restoration cannot reactivate only one inode alias.
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=123
             WHERE entry_id='future-hardlink-guard'",
        )
        .unwrap();
        let error = restore_drive_trash_item(&conn, "file", id).unwrap_err();
        assert!(
            error.contains("requires shared-inode POSIX support"),
            "{error}"
        );
        assert_eq!(state_row(&conn).unwrap(), before);
        let mut status = conn
            .prepare("SELECT trashed_at FROM drive_entries_v2 WHERE entry_id=?")
            .unwrap();
        status.bind((1, id)).unwrap();
        assert_eq!(status.next().unwrap(), sqlite::State::Row);
        assert_eq!(status.read::<i64, _>(0).unwrap(), 123);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_never_retires_linked_destination_before_unsupported_group_is_rejected() {
        let conn = memory_db();
        let src = "native-unlinked-source";
        let dst = "native-linked-destination";
        insert_v2_test_file(
            &conn,
            src,
            "55555555aaaaaaaabbbbbbbbcccccccc",
            None,
            "source.bin",
            202,
            9002,
        );
        insert_v2_test_file(
            &conn,
            dst,
            "66666666aaaaaaaabbbbbbbbcccccccc",
            None,
            "destination.bin",
            202,
            9003,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id='1234567890abcdef1234567890abcdef'
             WHERE entry_id='native-linked-destination'",
        )
        .unwrap();
        let original_revision = state_row(&conn).unwrap();
        let mut callbacks = 0;
        let err = rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::File,
            src,
            None,
            "destination.bin",
            false,
            |_| false,
            |_, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(err.contains("requires shared-inode POSIX support"), "{err}");
        assert_eq!(
            callbacks, 0,
            "failed rename must not retire the destination reader"
        );
        assert_eq!(state_row(&conn).unwrap(), original_revision);
        assert_eq!(
            drive_file_by_entry(&conn, dst)
                .unwrap()
                .unwrap()
                .display_name,
            "destination.bin"
        );
        assert_eq!(
            drive_file_by_entry(&conn, src)
                .unwrap()
                .unwrap()
                .display_name,
            "source.bin"
        );

        // The source guard also precedes every replacement callback.
        conn.execute(
            "UPDATE drive_entries_v2 SET inode_id=NULL
             WHERE entry_id='native-linked-destination';
             UPDATE drive_entries_v2
             SET inode_id='1234567890abcdef1234567890abcdef'
             WHERE entry_id='native-unlinked-source'",
        )
        .unwrap();
        let err = rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::File,
            src,
            None,
            "destination.bin",
            false,
            |_| false,
            |_, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(err.contains("requires shared-inode POSIX support"), "{err}");
        assert_eq!(callbacks, 0);
        assert_eq!(state_row(&conn).unwrap(), original_revision);

        // Existing unlinked V2 rename-overwrite must remain functional.
        conn.execute(
            "UPDATE drive_entries_v2 SET inode_id=NULL
             WHERE entry_id='native-unlinked-source'",
        )
        .unwrap();
        rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::File,
            src,
            None,
            "destination.bin",
            false,
            |_| false,
            |_, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(callbacks, 1);
        assert_eq!(
            drive_file_by_entry(&conn, src)
                .unwrap()
                .unwrap()
                .display_name,
            "destination.bin"
        );
        assert!(drive_file_by_entry(&conn, dst).unwrap().is_none());
    }

    #[test]
    fn future_hardlink_alias_never_enters_single_entry_copy_on_write() {
        let conn = memory_db();
        insert_v2_test_file(
            &conn,
            "guarded-v2-entry",
            "abababababababababababababababab",
            None,
            "guarded.bin",
            202,
            9001,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id='1234567890abcdef1234567890abcdef'
             WHERE entry_id='guarded-v2-entry'",
        )
        .unwrap();
        let before = state_row(&conn).unwrap();
        let error = create_drive_pending_replacement(
            &conn,
            "12341234123412341234123412341234",
            "guarded-v2-entry",
            "synthetic-only-staging",
        )
        .unwrap_err();
        assert!(error.contains("shared-inode copy-on-write"), "{error}");
        assert!(
            drive_pending_by_id(&conn, "12341234123412341234123412341234")
                .unwrap()
                .is_none()
        );
        assert_eq!(state_row(&conn).unwrap(), before);
    }

    #[test]
    fn mark_drive_dirty_updates_revision_without_reading_state_row_first() {
        let conn = memory_db();
        conn.execute(
            "UPDATE drive_state SET revision = 9000000000000, dirty = 0 WHERE singleton = 1",
        )
        .unwrap();
        DRIVE_STATE_ROW_CALLS.with(|calls| calls.set(0));

        let revision = mark_drive_dirty(&conn).unwrap();

        assert_eq!(
            DRIVE_STATE_ROW_CALLS.with(|calls| calls.get()),
            0,
            "mark_drive_dirty should perform the revision update in one SQL statement"
        );
        assert_eq!(revision, 9000000000001);
        let mut stmt = conn
            .prepare("SELECT revision, dirty FROM drive_state WHERE singleton = 1")
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<i64, _>(0).unwrap(), revision);
        assert_eq!(stmt.read::<i64, _>(1).unwrap(), 1);
    }

    #[test]
    fn mark_drive_dirty_refuses_missing_state_row_without_fabricating_revision() {
        let conn = memory_db();
        conn.execute("DELETE FROM drive_state WHERE singleton = 1")
            .unwrap();
        DRIVE_STATE_ROW_CALLS.with(|calls| calls.set(0));

        let error = mark_drive_dirty(&conn)
            .expect_err("a missing state row must never report a successful sync-dirty revision");
        assert!(error.contains("state row is missing"), "{error}");
        assert_eq!(
            DRIVE_STATE_ROW_CALLS.with(|calls| calls.get()),
            0,
            "missing state row must not trigger a separate SELECT"
        );
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM drive_state WHERE singleton = 1")
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<i64, _>(0).unwrap(), 0);

        // Re-initializing the Drive schema is the approved recovery path.
        init_drive_schema(&conn).unwrap();
        assert!(mark_drive_dirty(&conn).unwrap() > 0);
        assert!(state_row(&conn).unwrap().1);
    }

    #[test]
    fn folder_rename_revision_failure_rolls_back_metadata_and_supports_nested_retry() {
        let conn = memory_db();
        let original = create_drive_directory(&conn, None, "Original").unwrap();
        let destination = create_drive_directory(&conn, None, "Destination").unwrap();
        let initial_revision = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_folder_rename_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa folder rename revision failure'); END;",
        )
        .unwrap();

        let error = rename_drive_directory(&conn, &original.id, Some(&destination.id), "Renamed")
            .expect_err("the rename and revision must be atomic");
        assert!(
            error.contains("qa folder rename revision failure"),
            "{error}"
        );
        assert_eq!(
            load_directories(&conn)
                .unwrap()
                .into_iter()
                .find(|folder| folder.id == original.id)
                .unwrap(),
            original
        );
        assert_eq!(state_row(&conn).unwrap(), initial_revision);

        conn.execute("DROP TRIGGER qa_fail_folder_rename_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_folder_rename").unwrap();
        rename_drive_directory(&conn, &original.id, Some(&destination.id), "Renamed").unwrap();
        conn.execute("ROLLBACK TO qa_outer_folder_rename; RELEASE qa_outer_folder_rename")
            .unwrap();
        assert_eq!(
            load_directories(&conn)
                .unwrap()
                .into_iter()
                .find(|folder| folder.id == original.id)
                .unwrap(),
            original,
            "a nested rename must not commit the outer caller transaction"
        );
        rename_drive_directory(&conn, &original.id, Some(&destination.id), "Renamed").unwrap();
        let renamed = load_directories(&conn)
            .unwrap()
            .into_iter()
            .find(|folder| folder.id == original.id)
            .unwrap();
        assert_eq!(renamed.name, "Renamed");
        assert_eq!(renamed.parent_id.as_deref(), Some(destination.id.as_str()));
        assert!(state_row(&conn).unwrap().0 > initial_revision.0);
    }

    #[test]
    fn folder_trash_revision_failure_rolls_back_stale_entries_and_tombstone() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-stale-channel', 101, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-stale-file', 'qa-stale-channel', 99, 'stale.txt', 8,
                     'text/plain', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let folder = create_drive_directory(&conn, None, "To Trash").unwrap();
        rename_drive_file(&conn, "import-qa-stale-file", Some(&folder.id), "stale.txt").unwrap();
        conn.execute("UPDATE logical_files SET status='missing' WHERE file_id='qa-stale-file'")
            .unwrap();
        let read_entry_trashed = || -> Option<i64> {
            let mut stmt = conn
                .prepare(
                    "SELECT trashed_at FROM drive_file_entries
                     WHERE entry_id='import-qa-stale-file'",
                )
                .unwrap();
            assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
            stmt.read::<Option<i64>, _>(0).unwrap()
        };
        assert_eq!(read_entry_trashed(), None);
        let initial_revision = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_folder_trash_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa folder trash revision failure'); END;",
        )
        .unwrap();

        let error = trash_drive_directory(&conn, &folder.id)
            .expect_err("failed sync revision must undo tombstone and retired file entries");
        assert!(
            error.contains("qa folder trash revision failure"),
            "{error}"
        );
        assert_eq!(
            read_entry_trashed(),
            None,
            "stale file retirement must roll back"
        );
        assert!(
            load_directories(&conn)
                .unwrap()
                .iter()
                .any(|row| row.id == folder.id),
            "the folder must remain active"
        );
        assert_eq!(state_row(&conn).unwrap(), initial_revision);

        conn.execute("DROP TRIGGER qa_fail_folder_trash_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_folder_trash").unwrap();
        trash_drive_directory(&conn, &folder.id).unwrap();
        assert!(read_entry_trashed().is_some());
        conn.execute("ROLLBACK TO qa_outer_folder_trash; RELEASE qa_outer_folder_trash")
            .unwrap();
        assert_eq!(read_entry_trashed(), None);
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .any(|row| row.id == folder.id));

        trash_drive_directory(&conn, &folder.id).unwrap();
        assert!(read_entry_trashed().is_some());
        assert!(!load_directories(&conn)
            .unwrap()
            .iter()
            .any(|row| row.id == folder.id));
        assert!(state_row(&conn).unwrap().0 > initial_revision.0);
    }

    #[test]
    fn legacy_upload_finalize_revision_failure_keeps_pending_and_allows_retry() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-finalize-channel', 971, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-finalize-remote', 'qa-finalize-channel', 41, 'uploaded.txt', 9,
                     'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_pending_files
             (id, display_name, staging_path, backing_channel_id, created_at, updated_at)
             VALUES ('qa-finalize-pending', 'uploaded.txt', 'qa-stage-not-on-disk', 971, 2, 2);",
        )
        .unwrap();
        let original = drive_pending_by_id(&conn, "qa-finalize-pending")
            .unwrap()
            .unwrap();
        let revision = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_legacy_finalize_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa legacy upload finalize revision failed'); END;",
        )
        .unwrap();

        let error =
            finalize_legacy_upload_metadata(&conn, "qa-finalize-pending", "qa-finalize-remote")
                .expect_err("a failed revision must not finalize a Drive upload");
        assert!(
            error.contains("qa legacy upload finalize revision failed"),
            "{error}"
        );
        assert_eq!(
            serde_json::to_value(
                drive_pending_by_id(&conn, "qa-finalize-pending")
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(&original).unwrap(),
            "failed finalization must preserve every serialized pending field"
        );
        assert!(drive_file_by_entry(&conn, "qa-finalize-pending")
            .unwrap()
            .is_none());
        assert_eq!(state_row(&conn).unwrap(), revision);

        conn.execute("DROP TRIGGER qa_fail_legacy_finalize_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_upload_finalize").unwrap();
        assert_eq!(
            finalize_legacy_upload_metadata(&conn, "qa-finalize-pending", "qa-finalize-remote")
                .unwrap(),
            "qa-stage-not-on-disk"
        );
        assert!(drive_pending_by_id(&conn, "qa-finalize-pending")
            .unwrap()
            .is_none());
        assert!(drive_file_by_entry(&conn, "qa-finalize-pending")
            .unwrap()
            .is_some());
        conn.execute("ROLLBACK TO qa_outer_upload_finalize; RELEASE qa_outer_upload_finalize")
            .unwrap();
        assert_eq!(
            serde_json::to_value(
                drive_pending_by_id(&conn, "qa-finalize-pending")
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(&original).unwrap(),
            "the outer caller rollback must fully restore pending upload metadata"
        );
        assert!(drive_file_by_entry(&conn, "qa-finalize-pending")
            .unwrap()
            .is_none());
        assert_eq!(state_row(&conn).unwrap(), revision);

        assert_eq!(
            finalize_legacy_upload_metadata(&conn, "qa-finalize-pending", "qa-finalize-remote")
                .unwrap(),
            "qa-stage-not-on-disk"
        );
        assert!(drive_pending_by_id(&conn, "qa-finalize-pending")
            .unwrap()
            .is_none());
        let finalized = drive_file_by_entry(&conn, "qa-finalize-pending")
            .unwrap()
            .unwrap();
        assert_eq!(finalized.display_name, "uploaded.txt");
        assert_eq!(finalized.file_id, "qa-finalize-remote");
        assert!(state_row(&conn).unwrap().0 > revision.0);
    }

    #[test]
    fn legacy_upload_finalize_refuses_corrupt_pending_parent_without_changes() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-corrupt-stage-channel', 972, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-corrupt-stage-file', 'qa-corrupt-stage-channel', 42, 'upload.txt', 9,
                     'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_pending_files
             (id, directory_id, display_name, staging_path, backing_channel_id, created_at, updated_at)
             VALUES ('qa-corrupt-stage-pending', X'FF', 'upload.txt', 'qa-stage-corrupt', 972, 2, 2);",
        ).unwrap();
        let initial_revision = state_row(&conn).unwrap();
        let err = finalize_legacy_upload_metadata(
            &conn,
            "qa-corrupt-stage-pending",
            "qa-corrupt-stage-file",
        )
        .expect_err("an invalid original folder must not turn into a root-level upload");
        assert!(err.contains("directory_id"), "{err}");
        assert!(drive_file_by_entry(&conn, "qa-corrupt-stage-pending")
            .unwrap()
            .is_none());
        assert_eq!(state_row(&conn).unwrap(), initial_revision);
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM drive_pending_files WHERE id='qa-corrupt-stage-pending'")
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<i64, _>(0).unwrap(), 1);
    }

    #[test]
    fn imported_layout_revision_failure_rolls_back_all_channels_and_files() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-import-a', 551, 'Archive A', 'owner', 1, 1, 1),
                    ('qa-import-b', 552, 'Archive B', 'owner', 1, 2, 2);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-import-file-a', 'qa-import-a', 21, 'a.txt', 10,
                     'text/plain', 1, NULL, 1, 1, 'complete'),
                    ('qa-import-file-b', 'qa-import-b', 22, 'b.txt', 11,
                     'text/plain', 1, NULL, 1, 2, 'complete');",
        )
        .unwrap();
        let before = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_import_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa imported layout revision failure'); END;",
        )
        .unwrap();
        let err = ensure_imported_layout(&conn)
            .expect_err("failed dirty-revision update must roll back all imported entries");
        assert!(err.contains("qa imported layout revision failure"), "{err}");
        assert!(load_directories(&conn).unwrap().is_empty());
        assert!(load_entries(&conn).unwrap().is_empty());
        assert_eq!(state_row(&conn).unwrap(), before);

        conn.execute("DROP TRIGGER qa_fail_import_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_imported_layout").unwrap();
        assert!(ensure_imported_layout(&conn).unwrap());
        conn.execute("ROLLBACK TO qa_outer_imported_layout; RELEASE qa_outer_imported_layout")
            .unwrap();
        assert!(load_directories(&conn).unwrap().is_empty());
        assert!(load_entries(&conn).unwrap().is_empty());
        assert_eq!(state_row(&conn).unwrap(), before);

        assert!(ensure_imported_layout(&conn).unwrap());
        assert_eq!(load_directories(&conn).unwrap().len(), 2);
        assert_eq!(load_entries(&conn).unwrap().len(), 2);
        assert!(state_row(&conn).unwrap().0 > before.0);
        assert!(
            !ensure_imported_layout(&conn).unwrap(),
            "re-import must be idempotent"
        );
    }

    #[test]
    fn legacy_file_restore_revision_failure_preserves_trash_then_retries() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-restore-channel', 553, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-restore-file', 'qa-restore-channel', 21, 'restore.txt', 10,
                     'text/plain', 1, NULL, 1, 1, 'complete');",
        )
        .unwrap();
        assert!(ensure_imported_layout(&conn).unwrap());
        trash_drive_file(&conn, "import-qa-restore-file").unwrap();
        let before = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_legacy_file_restore_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa legacy restore revision failure'); END;",
        )
        .unwrap();
        let err = restore_drive_trash_item(&conn, "file", "import-qa-restore-file")
            .expect_err("failed revision must not revive legacy file");
        assert!(err.contains("qa legacy restore revision failure"), "{err}");
        assert!(drive_file_by_entry(&conn, "import-qa-restore-file")
            .unwrap()
            .is_none());
        assert!(list_drive_trash(&conn)
            .unwrap()
            .iter()
            .any(|item| item.id == "import-qa-restore-file"));
        assert_eq!(state_row(&conn).unwrap(), before);

        conn.execute("DROP TRIGGER qa_fail_legacy_file_restore_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_legacy_file_restore")
            .unwrap();
        assert_eq!(
            restore_drive_trash_item(&conn, "file", "import-qa-restore-file").unwrap(),
            "restore.txt"
        );
        conn.execute(
            "ROLLBACK TO qa_outer_legacy_file_restore; RELEASE qa_outer_legacy_file_restore",
        )
        .unwrap();
        assert!(drive_file_by_entry(&conn, "import-qa-restore-file")
            .unwrap()
            .is_none());
        assert_eq!(state_row(&conn).unwrap(), before);

        assert_eq!(
            restore_drive_trash_item(&conn, "file", "import-qa-restore-file").unwrap(),
            "restore.txt"
        );
        assert!(drive_file_by_entry(&conn, "import-qa-restore-file")
            .unwrap()
            .is_some());
        assert!(state_row(&conn).unwrap().0 > before.0);
    }

    #[test]
    fn folder_restore_revision_failure_preserves_tombstone_then_retries() {
        let conn = memory_db();
        let folder = create_drive_directory(&conn, None, "Restore Me").unwrap();
        trash_drive_directory(&conn, &folder.id).unwrap();
        let before = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_folder_restore_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa folder restore revision failure'); END;",
        )
        .unwrap();
        let err = restore_drive_trash_item(&conn, "folder", &folder.id)
            .expect_err("failed revision must not revive trashed folder");
        assert!(err.contains("qa folder restore revision failure"), "{err}");
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .all(|item| item.id != folder.id));
        assert!(list_drive_trash(&conn)
            .unwrap()
            .iter()
            .any(|item| item.id == folder.id));
        assert_eq!(state_row(&conn).unwrap(), before);

        conn.execute("DROP TRIGGER qa_fail_folder_restore_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_outer_folder_restore").unwrap();
        assert_eq!(
            restore_drive_trash_item(&conn, "folder", &folder.id).unwrap(),
            "Restore Me"
        );
        conn.execute("ROLLBACK TO qa_outer_folder_restore; RELEASE qa_outer_folder_restore")
            .unwrap();
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .all(|item| item.id != folder.id));
        assert_eq!(state_row(&conn).unwrap(), before);

        assert_eq!(
            restore_drive_trash_item(&conn, "folder", &folder.id).unwrap(),
            "Restore Me"
        );
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .any(|item| item.id == folder.id));
        assert!(state_row(&conn).unwrap().0 > before.0);
    }

    #[test]
    fn trash_listing_rejects_corrupt_parent_size_and_backing_status() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "qa-trash-legacy",
            "qa-trash-source",
            "qa-trash-channel",
            "legacy.txt",
            831,
            31,
        );
        trash_drive_file(&conn, "qa-trash-legacy").unwrap();

        for (case, mutation) in [
            (
                "corrupt parent",
                "UPDATE drive_file_entries SET directory_id=X'FF' WHERE entry_id='qa-trash-legacy'",
            ),
            (
                "negative size",
                "UPDATE logical_files SET total_size=-1 WHERE file_id='qa-trash-source'",
            ),
            (
                "corrupt size",
                "UPDATE logical_files SET total_size=X'FF' WHERE file_id='qa-trash-source'",
            ),
            (
                "corrupt status",
                "UPDATE logical_files SET status=X'FF' WHERE file_id='qa-trash-source'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_file_entries SET directory_id=NULL WHERE entry_id='qa-trash-legacy';
                 UPDATE logical_files SET total_size=128, status='complete' WHERE file_id='qa-trash-source'",
            ).unwrap();
            conn.execute(mutation).unwrap();
            assert!(
                list_drive_trash(&conn).is_err(),
                "{case}: Trash must not display fabricated metadata"
            );
        }
        conn.execute(
            "UPDATE drive_file_entries SET directory_id=NULL WHERE entry_id='qa-trash-legacy';
             UPDATE logical_files SET total_size=128, status='complete' WHERE file_id='qa-trash-source'",
        ).unwrap();
        assert_eq!(list_drive_trash(&conn).unwrap()[0].size, Some(128));

        insert_v2_test_file(
            &conn,
            "qa-trash-v2",
            "qa-trash-object",
            None,
            "v2.txt",
            832,
            32,
        );
        trash_drive_file(&conn, "qa-trash-v2").unwrap();
        for (case, mutation) in [
            (
                "V2 parent",
                "UPDATE drive_entries_v2 SET directory_id=X'FF' WHERE entry_id='qa-trash-v2'",
            ),
            (
                "V2 negative plaintext size",
                "UPDATE drive_objects SET plaintext_size=-1 WHERE object_id='qa-trash-object'",
            ),
            (
                "V2 invalid plaintext size",
                "UPDATE drive_objects SET plaintext_size=X'FF' WHERE object_id='qa-trash-object'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_entries_v2 SET directory_id=NULL WHERE entry_id='qa-trash-v2';
                 UPDATE drive_objects SET plaintext_size=1 WHERE object_id='qa-trash-object'",
            )
            .unwrap();
            conn.execute(mutation).unwrap();
            assert!(
                list_drive_trash(&conn).is_err(),
                "{case}: corrupted V2 Trash must fail closed"
            );
        }
        conn.execute(
            "UPDATE drive_entries_v2 SET directory_id=NULL WHERE entry_id='qa-trash-v2';
             UPDATE drive_objects SET plaintext_size=1 WHERE object_id='qa-trash-object'",
        )
        .unwrap();
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 2);
    }

    #[test]
    fn corrupt_original_parents_cannot_restore_files_folders_or_symlinks_at_root() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Parent").unwrap();
        insert_legacy_drive_test_file(
            &conn,
            "qa-parent-legacy",
            "qa-parent-source",
            "qa-parent-channel",
            "legacy.txt",
            833,
            33,
        );
        rename_drive_file(&conn, "qa-parent-legacy", Some(&parent.id), "legacy.txt").unwrap();
        trash_drive_file(&conn, "qa-parent-legacy").unwrap();

        insert_v2_test_file(
            &conn,
            "qa-parent-v2",
            "qa-parent-object",
            Some(&parent.id),
            "v2.txt",
            834,
            34,
        );
        trash_drive_file(&conn, "qa-parent-v2").unwrap();

        let folder = create_drive_directory(&conn, Some(&parent.id), "Child").unwrap();
        trash_drive_directory(&conn, &folder.id).unwrap();
        let link = create_drive_symlink(&conn, Some(&parent.id), "Shortcut", "../target").unwrap();
        trash_drive_symlink(&conn, &link.id).unwrap();

        for (kind, table, id_column, id, parent_column, restored_name) in [
            (
                "file",
                "drive_file_entries",
                "entry_id",
                "qa-parent-legacy",
                "directory_id",
                "legacy.txt",
            ),
            (
                "file",
                "drive_entries_v2",
                "entry_id",
                "qa-parent-v2",
                "directory_id",
                "v2.txt",
            ),
            (
                "folder",
                "drive_directories",
                "id",
                folder.id.as_str(),
                "parent_id",
                "Child",
            ),
            (
                "symlink",
                "drive_symlinks",
                "id",
                link.id.as_str(),
                "directory_id",
                "Shortcut",
            ),
        ] {
            // All table/column names above are hard-coded test metadata.
            let corrupt = format!("UPDATE {table} SET {parent_column}=X'FF' WHERE {id_column}=?");
            let mut stmt = conn.prepare(corrupt).unwrap();
            stmt.bind((1, id)).unwrap();
            stmt.next().unwrap();
            drop(stmt);
            assert!(
                restore_drive_trash_item(&conn, kind, id).is_err(),
                "{kind} {id}: malformed original parent must not become root"
            );

            let repair = format!("UPDATE {table} SET {parent_column}=? WHERE {id_column}=?");
            let mut stmt = conn.prepare(repair).unwrap();
            stmt.bind((1, parent.id.as_str())).unwrap();
            stmt.bind((2, id)).unwrap();
            stmt.next().unwrap();
            drop(stmt);
            assert_eq!(
                restore_drive_trash_item(&conn, kind, id).unwrap(),
                restored_name
            );
        }

        let dirs = load_directories(&conn).unwrap();
        assert_eq!(
            dirs.iter()
                .find(|d| d.id == folder.id)
                .unwrap()
                .parent_id
                .as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(
            drive_file_by_entry(&conn, "qa-parent-legacy")
                .unwrap()
                .unwrap()
                .directory_id
                .as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(
            drive_v2_file_by_entry(&conn, "qa-parent-v2")
                .unwrap()
                .unwrap()
                .directory_id
                .as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &link.id)
                .unwrap()
                .unwrap()
                .directory_id
                .as_deref(),
            Some(parent.id.as_str())
        );
        assert!(list_drive_trash(&conn).unwrap().is_empty());
    }

    #[test]
    fn symlink_rename_revision_failure_rolls_back_metadata_and_allows_retry() {
        let conn = memory_db();
        let source = create_drive_directory(&conn, None, "Source").unwrap();
        let destination = create_drive_directory(&conn, None, "Destination").unwrap();
        let original = create_drive_symlink(&conn, Some(&source.id), "link", "../target").unwrap();
        let revision_before = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_symlink_rename_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa symlink rename revision failed'); END;",
        )
        .unwrap();

        let error = rename_drive_symlink(&conn, &original.id, Some(&destination.id), "new-name")
            .expect_err("a failed revision write must prevent renaming");
        assert!(
            error.contains("qa symlink rename revision failed"),
            "{error}"
        );
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .unwrap(),
            original,
            "the name, parent, target and timestamps must remain unchanged"
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);

        conn.execute("DROP TRIGGER qa_fail_symlink_rename_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_caller_symlink_rename").unwrap();
        rename_drive_symlink(&conn, &original.id, Some(&destination.id), "new-name").unwrap();
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .unwrap()
                .display_name,
            "new-name"
        );
        conn.execute("ROLLBACK TO qa_caller_symlink_rename; RELEASE qa_caller_symlink_rename")
            .unwrap();
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .unwrap(),
            original,
            "a nested rename must not commit the caller's transaction"
        );

        rename_drive_symlink(&conn, &original.id, Some(&destination.id), "new-name").unwrap();
        let renamed = crate::drive_posix::symlink_by_id(&conn, &original.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            renamed.directory_id.as_deref(),
            Some(destination.id.as_str())
        );
        assert_eq!(renamed.display_name, "new-name");
        assert!(state_row(&conn).unwrap().0 > revision_before.0);
    }

    #[test]
    fn symlink_trash_revision_failure_rolls_back_tombstone_and_allows_retry() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Parent").unwrap();
        let original = create_drive_symlink(&conn, Some(&parent.id), "link", "../target").unwrap();
        let revision_before = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_symlink_trash_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa symlink trash revision failed'); END;",
        )
        .unwrap();

        let error = trash_drive_symlink(&conn, &original.id)
            .expect_err("failed revision write must not leave an unsynced tombstone");
        assert!(
            error.contains("qa symlink trash revision failed"),
            "{error}"
        );
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(list_drive_trash(&conn).unwrap().is_empty());

        conn.execute("DROP TRIGGER qa_fail_symlink_trash_revision")
            .unwrap();
        conn.execute("SAVEPOINT qa_caller_symlink_trash").unwrap();
        trash_drive_symlink(&conn, &original.id).unwrap();
        assert_eq!(list_drive_trash(&conn).unwrap().len(), 1);
        conn.execute("ROLLBACK TO qa_caller_symlink_trash; RELEASE qa_caller_symlink_trash")
            .unwrap();
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .unwrap(),
            original,
            "nested trash must preserve the caller's rollback capability"
        );

        trash_drive_symlink(&conn, &original.id).unwrap();
        assert!(
            crate::drive_posix::symlink_by_id(&conn, &original.id)
                .unwrap()
                .is_none(),
            "trashed symlinks must be hidden from active lookup"
        );
        let trash = list_drive_trash(&conn).unwrap();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, original.id);
        assert!(trash[0].deleted_at > 0);
        assert!(state_row(&conn).unwrap().0 > revision_before.0);
    }

    #[test]
    fn project_posix_symlink_trash_is_listed_for_recovery() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "project").unwrap();
        let link = create_drive_symlink(&conn, Some(&parent.id), "tool", "../bin/tool").unwrap();
        assert!(list_drive_trash(&conn).unwrap().is_empty());
        trash_drive_symlink(&conn, &link.id).unwrap();
        let items = list_drive_trash(&conn).unwrap();
        assert_eq!(items.len(), 1, "deleted symlink must remain discoverable");
        let item = &items[0];
        assert_eq!(item.kind, "symlink");
        assert_eq!(item.id, link.id);
        assert_eq!(item.name, "tool");
        assert_eq!(item.original_directory_id, Some(parent.id));
        assert!(item.restorable);
        assert!(item.deleted_at > 0);
    }

    #[test]
    fn project_posix_symlink_trash_restore_preserves_identity_and_target() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "project").unwrap();
        let original =
            create_drive_symlink(&conn, Some(&parent.id), "tool", "../bin/tool").unwrap();
        trash_drive_symlink(&conn, &original.id).unwrap();
        assert_eq!(
            restore_drive_trash_item(&conn, "symlink", &original.id).unwrap(),
            "tool"
        );
        let restored = crate::drive_posix::symlink_by_id(&conn, &original.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.target, original.target);
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.directory_id, Some(parent.id));
        assert!(restored.trashed_at.is_none());
        assert!(restored.updated_at >= original.updated_at);
        assert!(list_drive_trash(&conn).unwrap().is_empty());
        assert!(restore_drive_trash_item(&conn, "symlink", &original.id).is_err());
        assert!(restore_drive_trash_item(&conn, "symlink", "missing").is_err());
    }

    #[test]
    fn project_posix_symlink_trash_restore_handles_deleted_parent_and_name_collision() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "project").unwrap();
        let original =
            create_drive_symlink(&conn, Some(&parent.id), "tool", "/outside/never-follow").unwrap();
        trash_drive_symlink(&conn, &original.id).unwrap();
        trash_drive_directory(&conn, &parent.id).unwrap();
        let occupied = create_drive_directory(&conn, None, "TOOL").unwrap();
        let name = restore_drive_trash_item(&conn, "symlink", &original.id).unwrap();
        assert_ne!(portable_name_key(&name), portable_name_key("tool"));
        let restored = crate::drive_posix::symlink_by_id(&conn, &original.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.directory_id, None);
        assert_eq!(restored.display_name, name);
        assert_eq!(restored.target, original.target);
        assert!(drive_snapshot(&conn)
            .unwrap()
            .directories
            .iter()
            .any(|item| item.id == occupied.id));
    }

    #[test]
    fn project_posix_legacy_v3_storage_manifest_still_restores() {
        let source = memory_db();
        crate::drive_storage::init_drive_storage_schema(&source).unwrap();
        crate::drive_storage::register_storage_channel(
            &source,
            1,
            202,
            "Owned legacy fixture",
            "active",
            None,
        )
        .unwrap();
        insert_v2_test_file(
            &source,
            "v3-project-entry",
            "12121212343434345656565678787878",
            None,
            "legacy.js",
            202,
            9101,
        );
        let mut manifest = build_manifest(&source).unwrap();
        manifest.schema_version = 3;
        assert!(manifest.symlinks.is_empty() && manifest.permissions.is_empty());
        validate_manifest(&manifest).expect("populated schema3 is still supported");
        let target = memory_db();
        apply_manifest(&target, &manifest).unwrap();
        assert_eq!(
            drive_file_by_entry_id(&target, "v3-project-entry")
                .unwrap()
                .unwrap()
                .storage_version,
            2
        );
        assert_eq!(
            merge_drive_manifests(&manifest, &build_manifest(&target).unwrap())
                .unwrap()
                .v2_entries
                .len(),
            1
        );
    }

    #[test]
    fn project_posix_v1_copy_on_write_transfers_mode_to_base_entry() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES('v1-mode-channel',42,'Owned fixture','owner',1,1,1);
          INSERT INTO logical_files VALUES('v1-old','v1-mode-channel',10,'tool.sh',4,NULL,1,NULL,1,1,'complete');
          INSERT INTO logical_files VALUES('v1-new','v1-mode-channel',11,'tool.sh',5,NULL,1,NULL,1,1,'complete');").unwrap();
        ensure_imported_layout(&conn).unwrap();
        let base = drive_snapshot(&conn)
            .unwrap()
            .files
            .iter()
            .find(|file| file.file_id == "v1-old")
            .unwrap()
            .entry_id
            .clone();
        crate::drive_posix::set_permission(&conn, "file", &base, 0o755).unwrap();
        create_drive_pending_replacement(&conn, "v1-mode-pending", &base, "owned-stage").unwrap();
        crate::drive_posix::set_permission(&conn, "pending", "v1-mode-pending", 0o700).unwrap();
        mark_drive_pending_closed(&conn, "v1-mode-pending").unwrap();
        finalize_drive_streamed_pending(&conn, "v1-mode-pending", "v1-new", 5, None, None)
            .expect("COW finalization must retain mode on base");
        assert_eq!(
            crate::drive_posix::permission(&conn, "file", &base).unwrap(),
            Some(0o700)
        );
        assert!(drive_pending_by_id(&conn, "v1-mode-pending")
            .unwrap()
            .is_none());
        assert_eq!(
            drive_file_by_entry_id(&conn, &base)
                .unwrap()
                .unwrap()
                .file_id,
            "v1-new"
        );
    }

    #[test]
    fn v1_stream_finalization_rejects_sqlite_size_overflow_without_losing_pending_upload() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('size-overflow-channel', 44, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('size-overflow-file', 'size-overflow-channel', 10, 'safe.bin', 5,
                     NULL, 1, NULL, 1, 1, 'complete');",
        )
        .unwrap();
        let pending = create_drive_pending_file(
            &conn,
            "size-overflow-pending",
            None,
            "safe.bin",
            "owned-stage",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        let revision_before = state_row(&conn).unwrap();
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "size-overflow-file",
                u64::MAX,
                None,
                None,
            )
            .is_err(),
            "overflow must fail without persisting a negative SQLite plaintext size",
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert!(drive_file_by_entry(&conn, &pending.id).unwrap().is_none());

        finalize_drive_streamed_pending(&conn, &pending.id, "size-overflow-file", 5, None, None)
            .unwrap();
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
        assert_eq!(
            drive_file_by_entry(&conn, &pending.id)
                .unwrap()
                .unwrap()
                .plaintext_size,
            Some(5)
        );
    }

    #[test]
    fn v1_stream_finalization_preserves_pending_until_logical_file_is_complete() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-incomplete-channel', 101, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-incomplete-file', 'qa-incomplete-channel', 202, 'incomplete.bin', 7,
                     NULL, 1, NULL, 1, 1, 'missing');",
        )
        .unwrap();
        let pending = create_drive_pending_file(
            &conn,
            "qa-incomplete-pending",
            None,
            "incomplete.bin",
            "owned-fixture",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        let revision_before = state_row(&conn).unwrap();
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-incomplete-file",
                7,
                None,
                None,
            )
            .is_err(),
            "incomplete remote logical file must not replace durable pending state",
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert!(drive_file_by_entry(&conn, &pending.id).unwrap().is_none());

        conn.execute(
            "UPDATE logical_files SET status='complete' WHERE file_id='qa-incomplete-file'",
        )
        .unwrap();
        finalize_drive_streamed_pending(&conn, &pending.id, "qa-incomplete-file", 7, None, None)
            .unwrap();
        assert_eq!(
            drive_file_by_entry(&conn, &pending.id)
                .unwrap()
                .unwrap()
                .file_id,
            "qa-incomplete-file",
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
    }

    #[test]
    fn v1_stream_cow_does_not_discard_pending_when_original_entry_disappears() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels VALUES(
                 'qa-cow-missing-channel', 101, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files VALUES(
                 'qa-cow-original-file', 'qa-cow-missing-channel', 100, 'old.bin',
                 4, NULL, 1, NULL, 1, 1, 'complete');
             INSERT INTO logical_files VALUES(
                 'qa-cow-uploaded-file', 'qa-cow-missing-channel', 101, 'new.bin',
                 7, NULL, 1, NULL, 1, 1, 'complete');",
        )
        .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let original_id = drive_snapshot(&conn)
            .unwrap()
            .files
            .into_iter()
            .find(|file| file.file_id == "qa-cow-original-file")
            .unwrap()
            .entry_id;
        let pending = create_drive_pending_replacement(
            &conn,
            "qa-cow-missing-pending",
            &original_id,
            "owned-staging",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();

        let mut trash = conn
            .prepare("UPDATE drive_file_entries SET trashed_at=7 WHERE entry_id=?")
            .unwrap();
        trash.bind((1, original_id.as_str())).unwrap();
        trash.next().unwrap();
        drop(trash);
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-cow-uploaded-file",
                7,
                None,
                None
            )
            .is_err(),
            "trashed copy-on-write base must not be silently resurrected"
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        let mut untrash = conn
            .prepare("UPDATE drive_file_entries SET trashed_at=NULL WHERE entry_id=?")
            .unwrap();
        untrash.bind((1, original_id.as_str())).unwrap();
        untrash.next().unwrap();
        drop(untrash);

        let mut remove = conn
            .prepare("DELETE FROM drive_file_entries WHERE entry_id = ?")
            .unwrap();
        remove.bind((1, original_id.as_str())).unwrap();
        remove.next().unwrap();
        drop(remove);
        let revision_before = state_row(&conn).unwrap();
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-cow-uploaded-file",
                7,
                None,
                None
            )
            .is_err(),
            "disappeared copy-on-write base must not consume the only pending upload"
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert!(drive_file_by_entry_id(&conn, &original_id)
            .unwrap()
            .is_none());

        let mut restore = conn
            .prepare(
                "INSERT INTO drive_file_entries
             (entry_id, file_id, directory_id, display_name, created_at, updated_at)
             VALUES (?, 'qa-cow-original-file', NULL, 'old.bin', 1, 1)",
            )
            .unwrap();
        restore.bind((1, original_id.as_str())).unwrap();
        restore.next().unwrap();
        drop(restore);

        // A remote V2 migration can supersede the legacy file while its
        // original copy-on-write editor is still open.
        let v2_object = "10101010202020203030303040404040";
        insert_v2_test_file(
            &conn,
            &original_id,
            v2_object,
            None,
            "migrated.bin",
            202,
            9003,
        );
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-cow-uploaded-file",
                7,
                None,
                None,
            )
            .is_err(),
            "a V2 replacement must not silently consume an obsolete legacy editor"
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());

        let mut trash_v2 = conn
            .prepare("UPDATE drive_entries_v2 SET trashed_at=99 WHERE entry_id=?")
            .unwrap();
        trash_v2.bind((1, original_id.as_str())).unwrap();
        trash_v2.next().unwrap();
        drop(trash_v2);
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-cow-uploaded-file",
                7,
                None,
                None,
            )
            .is_err(),
            "a V2 tombstone must keep the original legacy editor pending"
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());

        let mut delete_v2 = conn
            .prepare("DELETE FROM drive_entries_v2 WHERE entry_id=?")
            .unwrap();
        delete_v2.bind((1, original_id.as_str())).unwrap();
        delete_v2.next().unwrap();
        drop(delete_v2);
        finalize_drive_streamed_pending(&conn, &pending.id, "qa-cow-uploaded-file", 7, None, None)
            .unwrap();
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
        assert_eq!(
            drive_file_by_entry_id(&conn, &original_id)
                .unwrap()
                .unwrap()
                .file_id,
            "qa-cow-uploaded-file"
        );
    }

    #[test]
    fn project_posix_v1_finalization_rolls_back_on_mode_failure() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES('mode-rollback-channel',43,'Owned fixture','owner',1,1,1);
          INSERT INTO logical_files VALUES('mode-old','mode-rollback-channel',10,'old.sh',4,NULL,1,NULL,1,1,'complete');
          INSERT INTO logical_files VALUES('mode-new','mode-rollback-channel',11,'new.sh',5,NULL,1,NULL,1,1,'complete');").unwrap();
        ensure_imported_layout(&conn).unwrap();
        let base = drive_snapshot(&conn)
            .unwrap()
            .files
            .iter()
            .find(|f| f.file_id == "mode-old")
            .unwrap()
            .entry_id
            .clone();
        create_drive_pending_replacement(&conn, "rollback-pending", &base, "owned-stage").unwrap();
        crate::drive_posix::set_permission(&conn, "pending", "rollback-pending", 0o700).unwrap();
        mark_drive_pending_closed(&conn, "rollback-pending").unwrap();
        conn.execute("CREATE TRIGGER fail_qa_mode BEFORE INSERT ON drive_permissions
           WHEN NEW.node_kind='file' BEGIN SELECT RAISE(ABORT,'owned simulated mode failure'); END;").unwrap();
        assert!(finalize_drive_streamed_pending(
            &conn,
            "rollback-pending",
            "mode-new",
            5,
            None,
            None
        )
        .is_err());
        let mut s = conn
            .prepare("SELECT file_id FROM drive_file_entries WHERE entry_id=?")
            .unwrap();
        s.bind((1, base.as_str())).unwrap();
        assert_eq!(s.next().unwrap(), sqlite::State::Row);
        assert_eq!(
            s.read::<String, _>(0).unwrap(),
            "mode-old",
            "failed finalization must retain original content metadata"
        );
        assert!(drive_pending_by_id(&conn, "rollback-pending")
            .unwrap()
            .is_some());
    }

    #[test]
    fn project_posix_unchanged_cow_close_preserves_successful_chmod() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES('chmod-channel',44,'Owned fixture','owner',1,1,1);
          INSERT INTO logical_files VALUES('chmod-file','chmod-channel',10,'tool.sh',4,NULL,1,NULL,1,1,'complete');").unwrap();
        ensure_imported_layout(&conn).unwrap();
        let base = drive_snapshot(&conn).unwrap().files[0].entry_id.clone();
        crate::drive_posix::set_permission(&conn, "file", &base, 0o755).unwrap();
        create_drive_pending_replacement(&conn, "unchanged-cow-mode", &base, "owned-stage")
            .unwrap();
        crate::drive_posix::set_permission(&conn, "pending", "unchanged-cow-mode", 0o700).unwrap();
        mark_drive_pending_closed(&conn, "unchanged-cow-mode").unwrap();
        // Closing an unchanged COW descriptor discards its content journal.
        delete_drive_pending(&conn, "unchanged-cow-mode").unwrap();
        assert_eq!(
            crate::drive_posix::permission(&conn, "file", &base).unwrap(),
            Some(0o700),
            "successful fchmod must survive cancellation of unchanged content"
        );
        assert_eq!(
            drive_file_by_entry_id(&conn, &base)
                .unwrap()
                .unwrap()
                .file_id,
            "chmod-file"
        );
        assert!(drive_pending_by_id(&conn, "unchanged-cow-mode")
            .unwrap()
            .is_none());
    }

    fn project_posix_fixture(conn: &sqlite::Connection) -> DriveManifestV1 {
        let mut value = serde_json::to_value(build_manifest(conn).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(4);
        value["symlinks"] = serde_json::json!([{
            "id":"project-link","directory_id":null,"display_name":"project-link",
            "target":"../source/index.js","created_at":100,"updated_at":100,"trashed_at":null
        }]);
        value["permissions"] = serde_json::json!([]);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn project_posix_manifest_preserves_link_target_and_cross_device_round_trip() {
        let source = memory_db();
        let manifest = project_posix_fixture(&source);
        assert_eq!(
            serde_json::to_value(&manifest).unwrap()["symlinks"][0]["target"],
            "../source/index.js",
            "symlink metadata must not be discarded"
        );
        let restored = memory_db();
        apply_manifest(&restored, &manifest).expect("schema-4 symlinks must apply");
        assert_eq!(
            serde_json::to_value(build_manifest(&restored).unwrap()).unwrap()["symlinks"],
            serde_json::to_value(&manifest).unwrap()["symlinks"]
        );
        assert_eq!(
            serde_json::to_value(drive_snapshot(&restored).unwrap()).unwrap()["symlinks"][0]["id"],
            "project-link"
        );
    }

    #[test]
    fn project_posix_permissions_round_trip_zero_mode_and_executable_mode() {
        for mode in [0u32, 0o755] {
            let source = memory_db();
            let directory = create_drive_directory(&source, None, "project").unwrap();
            let mut value = serde_json::to_value(project_posix_fixture(&source)).unwrap();
            value["permissions"] = serde_json::json!([{
                "node_kind":"directory","node_id":directory.id,"mode":mode,"updated_at":101
            }]);
            let manifest: DriveManifestV1 = serde_json::from_value(value).unwrap();
            let restored = memory_db();
            apply_manifest(&restored, &manifest).expect("ordinary permissions must apply");
            assert_eq!(
                serde_json::to_value(build_manifest(&restored).unwrap()).unwrap()["permissions"][0]
                    ["mode"],
                mode,
                "zero permissions must not become the default"
            );
        }
    }

    #[test]
    fn project_posix_merge_with_legacy_preserves_link_tombstones() {
        let conn = memory_db();
        let original = project_posix_fixture(&conn);
        let mut value = serde_json::to_value(&original).unwrap();
        value["symlinks"][0]["updated_at"] = serde_json::json!(200);
        value["symlinks"][0]["trashed_at"] = serde_json::json!(200);
        let deleted: DriveManifestV1 = serde_json::from_value(value).unwrap();
        let legacy = build_manifest(&conn).unwrap();
        let merged = merge_drive_manifests(
            &merge_drive_manifests(&original, &deleted).unwrap(),
            &legacy,
        )
        .unwrap();
        let reverse = merge_drive_manifests(
            &legacy,
            &merge_drive_manifests(&deleted, &original).unwrap(),
        )
        .unwrap();
        assert_eq!(merged, reverse);
        assert_eq!(
            serde_json::to_value(merged).unwrap()["symlinks"][0]["trashed_at"],
            200
        );

        let mut tied_deletion = original.clone();
        tied_deletion.symlinks[0].trashed_at = Some(100);
        for (left, right) in [(&original, &tied_deletion), (&tied_deletion, &original)] {
            let tied = merge_drive_manifests(left, right).unwrap();
            assert_eq!(
                tied.symlinks[0].trashed_at,
                Some(100),
                "a stale active symlink must not defeat a same-timestamp deletion"
            );
        }
    }

    #[test]
    fn project_posix_invalid_target_rejects_before_changing_database() {
        for target in [
            "".to_string(),
            String::from_utf8(vec![b'a', 0, b'b']).unwrap(),
            "x".repeat(4096),
        ] {
            let conn = memory_db();
            let before = build_manifest(&conn).unwrap();
            let mut value = serde_json::to_value(project_posix_fixture(&conn)).unwrap();
            value["symlinks"][0]["target"] = serde_json::json!(target);
            let manifest: DriveManifestV1 = serde_json::from_value(value).unwrap();
            let error = apply_manifest(&conn, &manifest).unwrap_err();
            assert!(error.contains("symlink target"), "{error}");
            assert_eq!(build_manifest(&conn).unwrap(), before);
        }
    }

    #[test]
    fn project_posix_link_collision_and_missing_parent_reject() {
        let conn = memory_db();
        create_drive_directory(&conn, None, "Project-Link").unwrap();
        let manifest = project_posix_fixture(&conn);
        assert!(validate_manifest(&manifest)
            .unwrap_err()
            .contains("colliding"));
        let conn = memory_db();
        let mut value = serde_json::to_value(project_posix_fixture(&conn)).unwrap();
        value["symlinks"][0]["directory_id"] = serde_json::json!("missing-parent");
        let manifest: DriveManifestV1 = serde_json::from_value(value).unwrap();
        assert!(validate_manifest(&manifest)
            .unwrap_err()
            .contains("missing"));
    }

    #[test]
    fn project_posix_invalid_permission_and_missing_node_reject() {
        let conn = memory_db();
        let dir = create_drive_directory(&conn, None, "mode-project").unwrap();
        for (node, mode, fragment) in [
            (&dir.id[..], 0o4755u32, "permission"),
            ("missing-node", 0o755, "missing"),
        ] {
            let mut value = serde_json::to_value(project_posix_fixture(&conn)).unwrap();
            value["permissions"] = serde_json::json!([{
                "node_kind":"directory","node_id":node,"mode":mode,"updated_at":101
            }]);
            let manifest: DriveManifestV1 = serde_json::from_value(value).unwrap();
            assert!(validate_manifest(&manifest).unwrap_err().contains(fragment));
        }
    }

    #[test]
    fn project_posix_link_blocks_directory_removal_and_name_reuse() {
        let conn = memory_db();
        let dir = create_drive_directory(&conn, None, "project").unwrap();
        let mut value = serde_json::to_value(project_posix_fixture(&conn)).unwrap();
        value["symlinks"][0]["directory_id"] = serde_json::json!(dir.id);
        let manifest: DriveManifestV1 = serde_json::from_value(value).unwrap();
        apply_manifest(&conn, &manifest).unwrap();
        assert!(trash_drive_directory(&conn, &dir.id)
            .unwrap_err()
            .contains("not empty"));
        assert!(create_drive_directory(&conn, Some(&dir.id), "project-link")
            .unwrap_err()
            .contains("already exists"));
    }

    #[test]
    fn targeted_drive_reads_match_legacy_snapshot_and_track_rename_and_trash() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES ('lookup-channel',42,'Lookup','owner',1,100,100);
            INSERT INTO logical_files VALUES ('lookup-file','lookup-channel',1,'日本語.bin',10000000000000,NULL,1,NULL,1,100,'complete');").unwrap();
        ensure_imported_layout(&conn).unwrap();
        let snapshot = drive_snapshot(&conn).unwrap();
        for dir in &snapshot.directories {
            assert_eq!(
                crate::drive_lookup::directory(&conn, &dir.id)
                    .unwrap()
                    .as_ref(),
                Some(dir)
            );
        }
        let old = &snapshot.files[0];
        let direct = crate::drive_lookup::file(&conn, &old.entry_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(direct).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        assert_eq!(
            crate::drive_lookup::child(&conn, old.directory_id.as_deref(), &old.display_name)
                .unwrap(),
            Some(crate::drive_fs_core::DriveNodeId::File(
                old.entry_id.clone()
            ))
        );
        conn.execute(
            "UPDATE drive_file_entries SET display_name='renamed.bin' WHERE file_id='lookup-file'",
        )
        .unwrap();
        assert!(
            crate::drive_lookup::child(&conn, old.directory_id.as_deref(), &old.display_name)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::drive_lookup::file(&conn, &old.entry_id)
                .unwrap()
                .unwrap()
                .display_name,
            "renamed.bin"
        );
        conn.execute("UPDATE drive_file_entries SET trashed_at=123 WHERE file_id='lookup-file'")
            .unwrap();
        assert!(crate::drive_lookup::file(&conn, &old.entry_id)
            .unwrap()
            .is_none());
        assert!(
            crate::drive_lookup::child(&conn, old.directory_id.as_deref(), "renamed.bin")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn targeted_drive_reads_match_v2_metadata_without_backing_parts() {
        let conn = memory_db();
        conn.execute("INSERT INTO drive_objects(object_id,plaintext_size,encryption_version,content_fingerprint,block_count,created_at) VALUES ('lookup-object',10000000000000,0,'fixture-fingerprint',1,123);
            INSERT INTO drive_entries_v2(entry_id,object_id,display_name,created_at,updated_at) VALUES ('lookup-v2','lookup-object','sample.mp4',123,124);").unwrap();
        let old = drive_file_by_entry_id(&conn, "lookup-v2").unwrap().unwrap();
        let direct = crate::drive_lookup::file(&conn, "lookup-v2")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(direct).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        assert_eq!(
            crate::drive_lookup::child(&conn, None, "sample.mp4").unwrap(),
            Some(crate::drive_fs_core::DriveNodeId::File("lookup-v2".into()))
        );
    }

    #[test]
    fn targeted_drive_lookup_prefers_pending_overlay_and_hides_the_old_name() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES ('lookup-channel',42,'Lookup','owner',1,100,100);
            INSERT INTO logical_files VALUES ('lookup-file','lookup-channel',1,'base.bin',128,NULL,1,NULL,1,100,'complete');").unwrap();
        ensure_imported_layout(&conn).unwrap();
        let file = drive_file_by_entry_id(&conn, "import-lookup-file")
            .unwrap()
            .unwrap();
        create_drive_pending_file(
            &conn,
            "lookup-pending",
            file.directory_id.as_deref(),
            "edited.bin",
            "owned-stage",
        )
        .unwrap();
        conn.execute("UPDATE drive_pending_files SET base_entry_id='import-lookup-file' WHERE id='lookup-pending'").unwrap();
        assert_eq!(
            crate::drive_lookup::pending_for_file(&conn, "import-lookup-file")
                .unwrap()
                .unwrap()
                .id,
            "lookup-pending"
        );
        assert!(
            crate::drive_lookup::child(&conn, file.directory_id.as_deref(), "base.bin")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::drive_lookup::child(&conn, file.directory_id.as_deref(), "edited.bin").unwrap(),
            Some(crate::drive_fs_core::DriveNodeId::Pending(
                "lookup-pending".into()
            ))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_unlink_cow_pending_trashes_base_and_removes_overlay_atomically() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES ('unlink-channel',42,'Unlink','owner',1,100,100);\n            INSERT INTO logical_files VALUES ('unlink-file','unlink-channel',1,'target.bin',128,NULL,1,NULL,1,100,'complete');")
            .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let base = drive_file_by_entry_id(&conn, "import-unlink-file")
            .unwrap()
            .unwrap();
        let pending = create_drive_pending_replacement(
            &conn,
            "unlink-pending",
            &base.entry_id,
            "owned-stage/.pending",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();

        let removed = unlink_drive_pending_native(&conn, &pending.id).unwrap();
        assert_eq!(removed.id, pending.id);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
        assert!(drive_file_by_entry_id(&conn, &base.entry_id)
            .unwrap()
            .is_none());
        assert!(crate::drive_lookup::child(
            &conn,
            base.directory_id.as_deref(),
            &base.display_name
        )
        .unwrap()
        .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_unlink_cow_pending_rolls_back_base_and_overlay_then_retries() {
        let conn = memory_db();
        conn.execute("INSERT INTO logical_channels VALUES ('unlink-rollback-channel',42,'Unlink','owner',1,100,100);\n            INSERT INTO logical_files VALUES ('unlink-rollback-file','unlink-rollback-channel',1,'target.bin',128,NULL,1,NULL,1,100,'complete');")
            .unwrap();
        ensure_imported_layout(&conn).unwrap();
        let base = drive_file_by_entry_id(&conn, "import-unlink-rollback-file")
            .unwrap()
            .unwrap();
        let pending = create_drive_pending_replacement(
            &conn,
            "unlink-rollback-pending",
            &base.entry_id,
            "owned-stage/.pending",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_unlink_pending_failure BEFORE DELETE ON drive_pending_files\n             WHEN OLD.id='unlink-rollback-pending'\n             BEGIN SELECT RAISE(ABORT,'qa unlink failure'); END;",
        )
        .unwrap();

        assert!(unlink_drive_pending_native(&conn, &pending.id).is_err());
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert!(drive_file_by_entry_id(&conn, &base.entry_id)
            .unwrap()
            .is_some());
        conn.execute("DROP TRIGGER qa_unlink_pending_failure")
            .unwrap();

        unlink_drive_pending_native(&conn, &pending.id).unwrap();
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
        assert!(drive_file_by_entry_id(&conn, &base.entry_id)
            .unwrap()
            .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_destination_callback_runs_once_for_replacement_and_rolls_back_on_error() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        create_drive_pending_file(
            &conn,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, "destination").unwrap();

        let mut calls = Vec::new();
        let result = rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| false,
            |kind, id| {
                calls.push((kind, id.to_string()));
                Ok(())
            },
        );
        let removed = result.unwrap().unwrap();
        assert_eq!(removed.id, "destination");
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0].0, NativeRenameKind::Pending));
        assert_eq!(calls[0].1, "destination");
        assert!(drive_pending_by_id(&conn, "destination").unwrap().is_none());
        assert_eq!(
            drive_pending_by_id(&conn, "source")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_destination_callback_is_not_called_for_absent_same_or_noreplace() {
        let absent = memory_db();
        create_drive_pending_file(&absent, "source", None, "source.txt", "source-stage").unwrap();
        let mut absent_calls = 0;
        rename_drive_item_native_with_destination(
            &absent,
            NativeRenameKind::Pending,
            "source",
            None,
            "new-name.txt",
            false,
            |_| false,
            |_, _| {
                absent_calls += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(absent_calls, 0);

        let same = memory_db();
        create_drive_pending_file(&same, "source", None, "source.txt", "source-stage").unwrap();
        let mut same_calls = 0;
        assert!(rename_drive_item_native_with_destination(
            &same,
            NativeRenameKind::Pending,
            "source",
            None,
            "source.txt",
            false,
            |_| false,
            |_, _| {
                same_calls += 1;
                Ok(())
            },
        )
        .unwrap()
        .is_none());
        assert_eq!(same_calls, 0);

        let no_replace = memory_db();
        create_drive_pending_file(&no_replace, "source", None, "source.txt", "source-stage")
            .unwrap();
        create_drive_pending_file(
            &no_replace,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        let mut no_replace_calls = 0;
        assert!(rename_drive_item_native_with_destination(
            &no_replace,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            true,
            |_| false,
            |_, _| {
                no_replace_calls += 1;
                Ok(())
            },
        )
        .is_err());
        assert_eq!(no_replace_calls, 0);
        assert!(drive_pending_by_id(&no_replace, "source")
            .unwrap()
            .is_some());
        assert!(drive_pending_by_id(&no_replace, "destination")
            .unwrap()
            .is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_destination_callback_error_rolls_back_both_entries() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        create_drive_pending_file(
            &conn,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, "destination").unwrap();

        let mut calls = 0;
        let result = rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| false,
            |_, _| {
                calls += 1;
                Err("retained view capture failed".to_string())
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert_eq!(
            drive_pending_by_id(&conn, "source")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
        assert_eq!(
            drive_pending_by_id(&conn, "destination")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_destination_callback_skips_busy_pending_destination() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        create_drive_pending_file(
            &conn,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, "destination").unwrap();

        let mut calls = 0;
        let result = rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| true,
            |_, _| {
                calls += 1;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);
        assert_eq!(
            drive_pending_by_id(&conn, "source")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
        assert!(drive_pending_by_id(&conn, "destination").unwrap().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_uses_unified_lookup_for_symlink_destination_replacement() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        conn.execute(
            "INSERT INTO drive_symlinks
                (id, directory_id, display_name, target, created_at, updated_at, trashed_at)
             VALUES ('destination-link', NULL, 'destination.txt', 'target.txt', 1, 1, NULL);",
        )
        .unwrap();

        let mut callback_destination = None;
        rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| false,
            |kind, id| {
                callback_destination =
                    Some((matches!(kind, NativeRenameKind::Symlink), id.to_string()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            callback_destination,
            Some((true, "destination-link".to_string())),
            "the unified destination lookup must preserve symlink identity"
        );
        assert!(crate::drive_posix::symlink_by_id(&conn, "destination-link")
            .unwrap()
            .is_none());
        assert_eq!(
            drive_pending_by_id(&conn, "source")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_hot_path_does_not_build_a_drive_snapshot() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        DRIVE_SNAPSHOT_CALLS.with(|calls| calls.set(0));

        rename_drive_item_native(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "renamed.txt",
            false,
            |_| false,
        )
        .unwrap();
        assert_eq!(
            DRIVE_SNAPSHOT_CALLS.with(|calls| calls.get()),
            0,
            "a single native rename must use targeted namespace lookups, not rebuild the full Drive snapshot"
        );
        let source = drive_pending_by_id(&conn, "source").unwrap().unwrap();
        assert_eq!(source.display_name, "renamed.txt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_rejects_dual_row_destination() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            631,
            16,
        );
        insert_legacy_drive_test_file(
            &conn,
            "destination-entry",
            "legacy-destination-file",
            "legacy-destination-channel",
            "destination.txt",
            632,
            17,
        );
        insert_v2_test_file(
            &conn,
            "destination-entry",
            "destination-object",
            None,
            "destination.txt",
            633,
            18,
        );

        let error = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect_err("a shadowed legacy alias must not become a second visible destination");
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );

        for table in ["drive_file_entries", "drive_entries_v2"] {
            let mut stmt = conn
                .prepare(format!("SELECT trashed_at FROM {table} WHERE entry_id=?1"))
                .unwrap();
            stmt.bind((1, "destination-entry")).unwrap();
            assert!(matches!(stmt.next().unwrap(), sqlite::State::Row));
            assert_eq!(
                stmt.read::<Option<i64>, _>(0).unwrap(),
                None,
                "failed replacement must roll back the {table} alias"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_rejects_dual_row_pending_base() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            641,
            19,
        );
        insert_legacy_drive_test_file(
            &conn,
            "base-entry",
            "legacy-base-file",
            "legacy-base-channel",
            "destination.txt",
            642,
            20,
        );
        insert_v2_test_file(
            &conn,
            "base-entry",
            "base-object",
            None,
            "destination.txt",
            643,
            21,
        );
        create_drive_pending_file(
            &conn,
            "pending-destination",
            None,
            "temporary.txt",
            "pending-stage",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_pending_files
             SET display_name='destination.txt', base_entry_id='base-entry', closed_at=updated_at
             WHERE id='pending-destination';",
        )
        .unwrap();

        let error = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect_err(
            "a shadowed legacy base alias must remain a collision after overlay replacement",
        );
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
        assert!(drive_pending_by_id(&conn, "pending-destination")
            .unwrap()
            .is_some());

        for table in ["drive_file_entries", "drive_entries_v2"] {
            let mut stmt = conn
                .prepare(format!("SELECT trashed_at FROM {table} WHERE entry_id=?1"))
                .unwrap();
            stmt.bind((1, "base-entry")).unwrap();
            assert!(matches!(stmt.next().unwrap(), sqlite::State::Row));
            assert_eq!(
                stmt.read::<Option<i64>, _>(0).unwrap(),
                None,
                "failed pending-base replacement must roll back the {table} alias"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_v2_source_does_not_treat_stale_legacy_alias_as_same_file_destination() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            691,
            31,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            692,
            32,
        );

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "shared-entry",
            None,
            "stale-name.txt",
            false,
            |_| false,
        )
        .expect("renaming to a stale legacy alias must rename the V2-preferred file identity");

        let source = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("the renamed source remains visible");
        assert_eq!(source.storage_version, 2);
        assert_eq!(source.display_name, "stale-name.txt");
        assert!(
            !list_drive_trash(&conn)
                .unwrap()
                .iter()
                .any(|item| item.kind == "file" && item.id == "shared-entry"),
            "a legacy shadow must not appear in Trash while its valid V2 entry is active"
        );
        assert!(
            restore_drive_trash_item(&conn, "file", "shared-entry").is_err(),
            "a retired legacy shadow must not be restorable while its valid V2 entry is active"
        );
        assert_eq!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "stale-name.txt",
            "an attempted shadow restore must not alter the authoritative V2 entry"
        );

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "shared-entry",
            None,
            "third-name.txt",
            false,
            |_| false,
        )
        .expect("the authoritative V2 entry can subsequently move away from the consumed alias");

        let source = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("the live V2 entry remains visible after its second rename");
        assert_eq!(source.storage_version, 2);
        assert_eq!(source.display_name, "third-name.txt");
        assert!(
            crate::drive_lookup::child(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none(),
            "rename-away must not leave the stale legacy name visible in direct child lookup"
        );
        assert!(
            crate::drive_lookup::child_entry(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none(),
            "rename-away must not leave the stale legacy name visible in typed child lookup"
        );

        trash_drive_file(&conn, "shared-entry").unwrap();
        assert!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .is_none(),
            "trashing an authoritative V2 entry must not resurrect the stale legacy alias"
        );
        let mut legacy = conn
            .prepare("SELECT trashed_at FROM drive_file_entries WHERE entry_id=?1")
            .unwrap();
        legacy.bind((1, "shared-entry")).unwrap();
        assert!(matches!(legacy.next().unwrap(), sqlite::State::Row));
        assert!(
            legacy.read::<Option<i64>, _>(0).unwrap().is_some(),
            "the stale shadow alias must be retired in the V2 rename transaction"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_trashing_authoritative_v2_does_not_reveal_legacy_fallback_or_duplicate_trash() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            689,
            29,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            690,
            30,
        );

        trash_drive_file(&conn, "shared-entry").unwrap();

        assert!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .is_none(),
            "trashing a valid authoritative V2 entry must not expose its legacy shadow"
        );
        assert!(
            crate::drive_lookup::child(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none(),
            "the stale legacy path must remain absent after direct V2 trash"
        );
        let trash = list_drive_trash(&conn).unwrap();
        let matching: Vec<_> = trash
            .iter()
            .filter(|item| item.kind == "file" && item.id == "shared-entry")
            .collect();
        assert_eq!(matching.len(), 1, "Trash must show the V2 entry only once");
        assert_eq!(matching[0].name, "current-name.txt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_trashing_orphan_v2_still_allows_legacy_fallback_restore() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "legacy-fallback.txt",
            688,
            28,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('shared-entry', 'missing-authoritative-object', NULL, 'orphan-name.txt', 2, 2, NULL);",
        )
        .unwrap();

        trash_drive_file(&conn, "shared-entry").unwrap();
        assert!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .is_none(),
            "trashing the logical identity should trash the legacy fallback when the V2 object is missing"
        );
        let trash = list_drive_trash(&conn).unwrap();
        assert!(
            trash.iter().any(|item| item.kind == "file"
                && item.id == "shared-entry"
                && item.name == "legacy-fallback.txt"),
            "an orphan V2 row must not suppress the restorable legacy trash entry"
        );

        let restored = restore_drive_trash_item(&conn, "file", "shared-entry")
            .expect("legacy fallback remains restorable when no V2 object exists");
        assert_eq!(restored, "legacy-fallback.txt");
        let restored_file = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("restored legacy fallback remains visible");
        assert_eq!(restored_file.storage_version, 1);
        assert_eq!(restored_file.display_name, "legacy-fallback.txt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fails_closed_when_v2_replacement_would_reveal_stale_legacy_alias() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            693,
            33,
        );
        insert_legacy_drive_test_file(
            &conn,
            "destination-entry",
            "legacy-destination-file",
            "legacy-destination-channel",
            "stale-destination-name.txt",
            694,
            34,
        );
        insert_v2_test_file(
            &conn,
            "destination-entry",
            "authoritative-destination-object",
            None,
            "destination.txt",
            695,
            35,
        );

        let error = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect_err(
            "replacing V2 must not expose its differently named legacy alias after the savepoint",
        );
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
        let destination = crate::drive_lookup::file(&conn, "destination-entry")
            .unwrap()
            .expect("rollback preserves the authoritative destination");
        assert_eq!(destination.storage_version, 2);
        assert_eq!(destination.display_name, "destination.txt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_replaces_legacy_fallback_when_v2_destination_object_is_missing() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            696,
            36,
        );
        insert_legacy_drive_test_file(
            &conn,
            "destination-entry",
            "legacy-destination-file",
            "legacy-destination-channel",
            "destination.txt",
            697,
            37,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('destination-entry', 'missing-destination-object', NULL, 'destination.txt', 2, 2, NULL);",
        )
        .unwrap();

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect("a missing V2 object must not shadow the valid legacy destination");

        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
        assert!(
            crate::drive_lookup::file(&conn, "destination-entry")
                .unwrap()
                .is_none(),
            "the replaced legacy destination is trashed and the orphan V2 row remains invisible"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_replaces_pending_base_using_legacy_fallback_when_v2_object_is_missing() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            698,
            38,
        );
        insert_legacy_drive_test_file(
            &conn,
            "base-entry",
            "legacy-base-file",
            "legacy-base-channel",
            "destination.txt",
            699,
            39,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('base-entry', 'missing-base-object', NULL, 'destination.txt', 2, 2, NULL);",
        )
        .unwrap();
        create_drive_pending_file(
            &conn,
            "pending-destination",
            None,
            "temporary.txt",
            "pending-stage",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_pending_files
             SET display_name='destination.txt', base_entry_id='base-entry', closed_at=updated_at
             WHERE id='pending-destination';",
        )
        .unwrap();

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect("a pending COW base must use legacy fallback when its V2 object is missing");

        assert!(
            drive_pending_by_id(&conn, "pending-destination")
                .unwrap()
                .is_none(),
            "the replaced closed overlay is removed"
        );
        assert!(
            crate::drive_lookup::file(&conn, "base-entry")
                .unwrap()
                .is_none(),
            "the legacy base is trashed and the orphan V2 row remains invisible"
        );
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_ignores_orphan_v2_row_as_a_portable_name_collision() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            700,
            40,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('orphan-entry', 'missing-object', NULL, 'ghost.txt', 2, 2, NULL);",
        )
        .unwrap();

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "ghost.txt",
            false,
            |_| false,
        )
        .expect("an orphan V2 metadata row without an object is not a live namespace entry");

        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "ghost.txt"
        );
        assert!(
            crate::drive_lookup::file(&conn, "orphan-entry")
                .unwrap()
                .is_none(),
            "the orphan V2 row remains invisible"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fails_closed_when_pending_v2_base_replacement_would_reveal_stale_legacy_alias(
    ) {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            701,
            41,
        );
        insert_legacy_drive_test_file(
            &conn,
            "base-entry",
            "legacy-base-file",
            "legacy-base-channel",
            "stale-base-name.txt",
            702,
            42,
        );
        insert_v2_test_file(
            &conn,
            "base-entry",
            "authoritative-base-object",
            None,
            "destination.txt",
            703,
            43,
        );
        create_drive_pending_file(
            &conn,
            "pending-destination",
            None,
            "temporary.txt",
            "pending-stage",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_pending_files
             SET display_name='destination.txt', base_entry_id='base-entry', closed_at=updated_at
             WHERE id='pending-destination';",
        )
        .unwrap();

        let error = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .expect_err(
            "replacing the pending overlay and authoritative V2 base must not reveal the stale legacy name",
        );
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
        assert!(
            drive_pending_by_id(&conn, "pending-destination")
                .unwrap()
                .is_some(),
            "failed replacement must roll back the pending overlay removal"
        );
        let base = crate::drive_lookup::file(&conn, "base-entry")
            .unwrap()
            .expect("the authoritative base survives failed replacement");
        assert_eq!(base.storage_version, 2);
        assert_eq!(base.display_name, "destination.txt");
        for table in ["drive_file_entries", "drive_entries_v2"] {
            let mut stmt = conn
                .prepare(format!("SELECT trashed_at FROM {table} WHERE entry_id=?1"))
                .unwrap();
            stmt.bind((1, "base-entry")).unwrap();
            assert!(matches!(stmt.next().unwrap(), sqlite::State::Row));
            assert_eq!(
                stmt.read::<Option<i64>, _>(0).unwrap(),
                None,
                "failed pending-base replacement must roll back the {table} alias"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_replaces_a_symlink_destination() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            601,
            11,
        );
        conn.execute(
            "INSERT INTO drive_symlinks
                (id, directory_id, display_name, target, created_at, updated_at, trashed_at)
             VALUES ('destination-link', NULL, 'destination.txt', 'target.txt', 1, 1, NULL);",
        )
        .unwrap();

        reset_drive_name_collision_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        let mut callback_destination = None;
        rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
            |kind, id| {
                callback_destination =
                    Some((matches!(kind, NativeRenameKind::Symlink), id.to_string()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            callback_destination,
            Some((true, "destination-link".to_string()))
        );
        assert_eq!(drive_name_collision_query_steps(), 0);
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);
        assert!(crate::drive_posix::symlink_by_id(&conn, "destination-link")
            .unwrap()
            .is_none());
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_replaces_closed_pending_overlay_and_base() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            611,
            12,
        );
        insert_legacy_drive_test_file(
            &conn,
            "base-entry",
            "base-file",
            "base-channel",
            "destination.txt",
            612,
            13,
        );
        create_drive_pending_file(
            &conn,
            "pending-destination",
            None,
            "temporary.txt",
            "pending-stage",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_pending_files
             SET display_name = 'destination.txt', base_entry_id = 'base-entry', closed_at = updated_at
             WHERE id = 'pending-destination';",
        )
        .unwrap();

        reset_drive_name_collision_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        reset_drive_trash_file_version_probe_calls();
        let mut callback_destination = None;
        rename_drive_item_native_with_destination(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
            |kind, id| {
                callback_destination =
                    Some((matches!(kind, NativeRenameKind::Pending), id.to_string()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            callback_destination,
            Some((true, "pending-destination".to_string()))
        );
        assert_eq!(drive_name_collision_query_steps(), 0);
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);
        assert_eq!(
            drive_trash_file_version_probe_calls(),
            0,
            "native pending-base replacement should reuse the base version from the namespace row"
        );
        assert!(drive_pending_by_id(&conn, "pending-destination")
            .unwrap()
            .is_none());
        assert!(crate::drive_lookup::file(&conn, "base-entry")
            .unwrap()
            .is_none());
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_replaces_a_v2_file_destination() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "source-entry",
            "source-file",
            "source-channel",
            "source.txt",
            621,
            14,
        );
        insert_v2_test_file(
            &conn,
            "destination-entry",
            "destination-object",
            None,
            "destination.txt",
            622,
            15,
        );

        reset_drive_name_collision_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        reset_drive_trash_file_version_probe_calls();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .unwrap();

        assert_eq!(drive_name_collision_query_steps(), 0);
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);
        assert_eq!(
            drive_trash_file_version_probe_calls(),
            0,
            "native replacement should reuse the V2-kind bit from the namespace row"
        );
        assert!(crate::drive_lookup::file(&conn, "destination-entry")
            .unwrap()
            .is_none());
        assert_eq!(
            crate::drive_lookup::file(&conn, "source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_query_preserves_parent_error_precedence_and_missing_source() {
        let conn = memory_db();

        crate::drive_lookup::reset_lookup_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        let missing_parent = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "missing-source",
            Some("missing-parent"),
            "new-name.txt",
            false,
            |_| false,
        )
        .expect_err("missing destination parent must be reported before missing source");
        assert_eq!(missing_parent, "Destination folder does not exist");
        assert_eq!(crate::drive_lookup::lookup_query_steps(), 0);
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);

        crate::drive_lookup::reset_lookup_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        let missing_source = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "missing-source",
            None,
            "new-name.txt",
            false,
            |_| false,
        )
        .expect_err("a missing source at root must remain an error");
        assert_eq!(missing_source, "Drive rename source does not exist");
        assert_eq!(crate::drive_lookup::lookup_query_steps(), 0);
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_query_falls_back_to_legacy_for_orphan_v2_source() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "orphan-source-entry",
            "orphan-source-file",
            "orphan-source-channel",
            "source.txt",
            671,
            24,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('orphan-source-entry', 'missing-object', NULL, 'stale-v2-name.txt', 2, 2, NULL);",
        )
        .unwrap();

        crate::drive_lookup::reset_lookup_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "orphan-source-entry",
            None,
            "renamed-source.txt",
            false,
            |_| false,
        )
        .unwrap();

        assert_eq!(
            crate::drive_lookup::lookup_query_steps(),
            0,
            "source version and namespace should be resolved by the fused statement"
        );
        assert_eq!(drive_native_file_rename_namespace_query_steps(), 1);
        let source = crate::drive_lookup::file(&conn, "orphan-source-entry")
            .unwrap()
            .expect("valid legacy source must remain readable");
        assert_eq!(source.storage_version, 1);
        assert_eq!(source.display_name, "renamed-source.txt");

        let mut stmt = conn
            .prepare(
                "SELECT display_name FROM drive_entries_v2
                 WHERE entry_id = 'orphan-source-entry' AND trashed_at IS NULL",
            )
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(
            stmt.read::<String, _>("display_name").unwrap(),
            "stale-v2-name.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fuses_source_parent_and_namespace_query() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "fused-source-entry",
            "fused-source-file",
            "fused-source-channel",
            "source.txt",
            661,
            23,
        );
        conn.execute(
            "INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES ('rename-parent', NULL, 'rename-parent', NULL, 1, 1, NULL);",
        )
        .unwrap();

        crate::drive_lookup::reset_lookup_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "fused-source-entry",
            Some("rename-parent"),
            "renamed.txt",
            false,
            |_| false,
        )
        .unwrap();

        assert_eq!(
            crate::drive_lookup::lookup_query_steps(),
            0,
            "native file rename should validate source and destination parent in the fused namespace statement"
        );
        assert_eq!(
            drive_native_file_rename_namespace_query_steps(),
            1,
            "source, destination-parent, exact destination, and portable collision validation use one query"
        );
        let renamed = crate::drive_lookup::file(&conn, "fused-source-entry")
            .unwrap()
            .unwrap();
        assert_eq!(renamed.directory_id.as_deref(), Some("rename-parent"));
        assert_eq!(renamed.display_name, "renamed.txt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fuses_destination_lookup_and_portable_collision_check() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('fused-rename-channel', 501, 'fused-rename-channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('fused-rename-file', 'fused-rename-channel', 1, 'source.txt',
                     128, 'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('fused-rename-entry', 'fused-rename-file', NULL, 'source.txt', 1, 1,
                     NULL, 0, 128, NULL, NULL);",
        )
        .unwrap();

        reset_drive_name_collision_query_steps();
        reset_drive_native_file_rename_namespace_query_steps();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "fused-rename-entry",
            None,
            "fresh-name.txt",
            false,
            |_| false,
        )
        .unwrap();

        assert_eq!(
            drive_name_collision_query_steps(),
            0,
            "native file rename must not execute a separate portable-name scan"
        );
        assert_eq!(
            drive_native_file_rename_namespace_query_steps(),
            1,
            "destination lookup and portable-name validation must share one namespace statement"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_fused_namespace_preserves_unicode_collision_rejection() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('unicode-rename-channel', 511, 'unicode-rename-channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('unicode-source-file', 'unicode-rename-channel', 1, 'source.txt',
                     128, 'text/plain', 1, NULL, 1, 1, 'complete'),
                    ('unicode-collision-file', 'unicode-rename-channel', 2, 'Résumé.txt',
                     128, 'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('unicode-source-entry', 'unicode-source-file', NULL, 'source.txt', 1, 1,
                     NULL, 0, 128, NULL, NULL),
                    ('unicode-collision-entry', 'unicode-collision-file', NULL, 'Résumé.txt', 1, 1,
                     NULL, 0, 128, NULL, NULL);",
        )
        .unwrap();

        let error = rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "unicode-source-entry",
            None,
            "RÉSUMÉ.txt",
            false,
            |_| false,
        )
        .expect_err("portable NFC/case-fold collisions must still reject native file rename");
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            crate::drive_lookup::file(&conn, "unicode-source-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "source.txt"
        );
    }

    #[test]
    fn public_trash_drive_file_keeps_its_version_probe() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "public-trash-entry",
            "public-trash-file",
            "public-trash-channel",
            "public-trash.txt",
            651,
            22,
        );

        reset_drive_trash_file_version_probe_calls();
        trash_drive_file(&conn, "public-trash-entry").unwrap();
        assert_eq!(
            drive_trash_file_version_probe_calls(),
            1,
            "the standalone public trash helper still discovers its own storage version"
        );
        assert!(crate::drive_lookup::file(&conn, "public-trash-entry")
            .unwrap()
            .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_reuses_the_discovered_source_storage_version() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('legacy-rename-channel', 401, 'legacy-rename-channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('legacy-rename-file', 'legacy-rename-channel', 1, 'legacy-source.txt',
                     128, 'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('legacy-entry', 'legacy-rename-file', NULL, 'legacy-source.txt', 1, 1,
                     NULL, 0, 128, NULL, NULL);",
        )
        .unwrap();
        insert_v2_test_file(
            &conn,
            "v2-entry",
            "v2-object",
            None,
            "v2-source.txt",
            402,
            2,
        );

        reset_drive_rename_file_version_probe_calls();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "legacy-entry",
            None,
            "legacy-renamed.txt",
            false,
            |_| false,
        )
        .unwrap();
        assert_eq!(
            drive_rename_file_version_probe_calls(),
            0,
            "native legacy rename should reuse the source record fetched for validation"
        );

        reset_drive_rename_file_version_probe_calls();
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "v2-entry",
            None,
            "v2-renamed.txt",
            false,
            |_| false,
        )
        .unwrap();
        assert_eq!(
            drive_rename_file_version_probe_calls(),
            0,
            "native V2 rename should reuse the source record fetched for validation"
        );

        reset_drive_rename_file_version_probe_calls();
        rename_drive_file(&conn, "legacy-entry", None, "public-renamed.txt").unwrap();
        assert_eq!(
            drive_rename_file_version_probe_calls(),
            1,
            "the public rename helper must retain its independent storage-version lookup"
        );
    }

    #[test]
    fn trashed_authoritative_v2_suppresses_historical_legacy_shadow_before_restore() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            671,
            11,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            672,
            12,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET trashed_at = 2, updated_at = 2
             WHERE entry_id = 'shared-entry';",
        )
        .unwrap();

        assert!(crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .is_none());
        assert!(crate::drive_lookup::child(&conn, None, "stale-name.txt")
            .unwrap()
            .is_none());
        assert!(
            crate::drive_lookup::child_entry(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none()
        );
        assert!(
            crate::drive_lookup::regular_file_entry(&conn, "shared-entry")
                .unwrap()
                .is_none()
        );
        assert!(drive_file_by_entry(&conn, "shared-entry")
            .unwrap()
            .is_none());
        assert!(drive_file_by_entry_id(&conn, "shared-entry")
            .unwrap()
            .is_none());
        assert!(
            drive_snapshot(&conn)
                .unwrap()
                .files
                .iter()
                .all(|file| file.entry_id != "shared-entry"),
            "the snapshot must not expose the historical legacy shadow while authoritative V2 is trashed"
        );
        assert!(
            ensure_name_available(&conn, None, "stale-name.txt", None, None, None).is_ok(),
            "a hidden historical shadow must not reserve its stale name"
        );
    }

    #[test]
    fn trashed_v2_shadow_does_not_block_removing_its_folder() {
        let conn = memory_db();
        let folder = create_drive_directory(&conn, None, "empty-folder").unwrap();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            667,
            7,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            Some(&folder.id),
            "current-name.txt",
            668,
            8,
        );
        let mut legacy_location = conn
            .prepare("UPDATE drive_file_entries SET directory_id=? WHERE entry_id='shared-entry'")
            .unwrap();
        legacy_location.bind((1, folder.id.as_str())).unwrap();
        legacy_location.next().unwrap();
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=2, updated_at=2 WHERE entry_id='shared-entry';",
        )
        .unwrap();

        trash_drive_directory(&conn, &folder.id)
            .expect("a hidden legacy shadow must not make the folder appear non-empty");
        assert!(crate::drive_lookup::directory(&conn, &folder.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn orphan_v2_child_does_not_block_removing_its_folder() {
        let conn = memory_db();
        let folder = create_drive_directory(&conn, None, "orphan-folder").unwrap();
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('orphan-entry', 'missing-object', NULL, 'orphan.txt', 1, 1, NULL);",
        )
        .unwrap();
        let mut assign_parent = conn
            .prepare("UPDATE drive_entries_v2 SET directory_id=? WHERE entry_id='orphan-entry'")
            .unwrap();
        assign_parent.bind((1, folder.id.as_str())).unwrap();
        assign_parent.next().unwrap();

        trash_drive_directory(&conn, &folder.id).expect(
            "an orphan V2 row that is not rendered must not block removing an empty folder",
        );
        assert!(crate::drive_lookup::directory(&conn, &folder.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn pending_permission_is_not_transferred_to_historical_legacy_shadow_of_trashed_v2() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            665,
            5,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            666,
            6,
        );
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=2, updated_at=2 WHERE entry_id='shared-entry';",
        )
        .unwrap();
        crate::drive_posix::upsert_permission(
            &conn,
            &crate::drive_posix::DrivePermissionRecord {
                node_kind: "pending".into(),
                node_id: "pending-overlay".into(),
                mode: 0o754,
                updated_at: 7,
            },
        )
        .unwrap();

        crate::drive_posix::retain_pending_permission(&conn, "pending-overlay", "shared-entry")
            .expect("a trashed V2 base with only a historical legacy shadow is not active");

        let mut file_mode = conn
            .prepare("SELECT mode FROM drive_permissions WHERE node_kind='file' AND node_id=?1")
            .unwrap();
        file_mode.bind((1, "shared-entry")).unwrap();
        assert!(
            !matches!(file_mode.next().unwrap(), sqlite::State::Row),
            "a pending permission must not be transferred to a hidden legacy shadow"
        );
    }

    #[test]
    fn pending_permission_retains_mode_for_legacy_fallback_when_v2_object_is_missing() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "legacy-fallback",
            "legacy-fallback-file",
            "legacy-fallback-channel",
            "legacy-name.txt",
            663,
            3,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('legacy-fallback', 'missing-object', NULL, 'orphan.txt', 2, 2, 2);",
        )
        .unwrap();
        crate::drive_posix::upsert_permission(
            &conn,
            &crate::drive_posix::DrivePermissionRecord {
                node_kind: "pending".into(),
                node_id: "pending-overlay".into(),
                mode: 0o751,
                updated_at: 4,
            },
        )
        .unwrap();

        crate::drive_posix::retain_pending_permission(&conn, "pending-overlay", "legacy-fallback")
            .expect("an orphan V2 metadata row must not hide a valid legacy base");

        let mut file_mode = conn
            .prepare("SELECT mode FROM drive_permissions WHERE node_kind='file' AND node_id=?1")
            .unwrap();
        file_mode.bind((1, "legacy-fallback")).unwrap();
        assert!(matches!(file_mode.next().unwrap(), sqlite::State::Row));
        assert_eq!(file_mode.read::<i64, _>(0).unwrap(), 0o751);
        let mut pending_mode = conn
            .prepare("SELECT 1 FROM drive_permissions WHERE node_kind='pending' AND node_id=?1")
            .unwrap();
        pending_mode.bind((1, "pending-overlay")).unwrap();
        assert!(!matches!(pending_mode.next().unwrap(), sqlite::State::Row));
    }

    #[test]
    fn orphan_v2_metadata_preserves_legacy_fallback_across_read_paths() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "legacy-fallback",
            "legacy-fallback-file",
            "legacy-fallback-channel",
            "legacy-name.txt",
            669,
            9,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('legacy-fallback', 'missing-fallback-object', NULL, 'orphan-name.txt', 2, 2, 2);",
        )
        .unwrap();

        let file = crate::drive_lookup::file(&conn, "legacy-fallback")
            .unwrap()
            .expect("missing V2 objects must preserve valid legacy metadata");
        assert_eq!(file.storage_version, 1);
        assert_eq!(file.display_name, "legacy-name.txt");
        assert!(matches!(
            crate::drive_lookup::child(&conn, None, "legacy-name.txt").unwrap(),
            Some(crate::drive_fs_core::DriveNodeId::File(ref id)) if id == "legacy-fallback"
        ));
        assert!(matches!(
            crate::drive_lookup::child_entry(&conn, None, "legacy-name.txt").unwrap(),
            Some(crate::drive_lookup::DriveChildEntry::File(ref file))
                if file.storage_version == 1 && file.display_name == "legacy-name.txt"
        ));
        assert!(matches!(
            crate::drive_lookup::regular_file_entry(&conn, "legacy-fallback").unwrap(),
            Some(crate::drive_lookup::RegularFileEntry::File(ref file))
                if file.storage_version == 1 && file.display_name == "legacy-name.txt"
        ));
        assert_eq!(
            drive_file_by_entry(&conn, "legacy-fallback")
                .unwrap()
                .unwrap()
                .storage_version,
            1
        );
        assert_eq!(
            drive_file_by_entry_id(&conn, "legacy-fallback")
                .unwrap()
                .unwrap()
                .storage_version,
            1
        );
        assert!(drive_snapshot(&conn)
            .unwrap()
            .files
            .iter()
            .any(|file| file.entry_id == "legacy-fallback" && file.storage_version == 1));
    }

    #[test]
    fn restore_authoritative_v2_retires_historical_legacy_shadow() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            675,
            15,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            676,
            16,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET trashed_at = 2, updated_at = 2
             WHERE entry_id = 'shared-entry';",
        )
        .unwrap();

        let restored = restore_drive_trash_item(&conn, "file", "shared-entry")
            .expect("the authoritative V2 trash item should restore");
        assert_eq!(restored, "current-name.txt");
        let source = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("the restored V2 entry remains visible");
        assert_eq!(source.storage_version, 2);
        assert_eq!(source.display_name, "current-name.txt");
        assert!(
            crate::drive_lookup::child(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none(),
            "restoring V2 must not revive a historical legacy shadow under its stale name"
        );
        let mut legacy = conn
            .prepare("SELECT trashed_at FROM drive_file_entries WHERE entry_id=?1")
            .unwrap();
        legacy.bind((1, "shared-entry")).unwrap();
        assert!(matches!(legacy.next().unwrap(), sqlite::State::Row));
        assert!(
            legacy.read::<Option<i64>, _>(0).unwrap().is_some(),
            "V2 restore must retire the historical legacy shadow"
        );
    }

    #[test]
    fn restore_authoritative_v2_rolls_back_when_legacy_shadow_retirement_fails() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            673,
            13,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            674,
            14,
        );
        conn.execute(
            "UPDATE drive_entries_v2
             SET trashed_at = 2, updated_at = 2
             WHERE entry_id = 'shared-entry';
             CREATE TRIGGER fail_restore_legacy_shadow_retirement
             BEFORE UPDATE OF trashed_at ON drive_file_entries
             WHEN NEW.entry_id = 'shared-entry'
             BEGIN SELECT RAISE(ABORT, 'simulated restore shadow retirement failure'); END;",
        )
        .unwrap();

        assert!(
            restore_drive_trash_item(&conn, "file", "shared-entry").is_err(),
            "the restore must fail when its legacy shadow cannot be retired"
        );
        let mut v2 = conn
            .prepare("SELECT trashed_at, display_name FROM drive_entries_v2 WHERE entry_id=?1")
            .unwrap();
        v2.bind((1, "shared-entry")).unwrap();
        assert!(matches!(v2.next().unwrap(), sqlite::State::Row));
        assert!(
            v2.read::<Option<i64>, _>("trashed_at").unwrap().is_some(),
            "failed shadow retirement must leave V2 trashed"
        );
        assert_eq!(
            v2.read::<String, _>("display_name").unwrap(),
            "current-name.txt"
        );
        assert!(
            rename_drive_file(&conn, "shared-entry", None, "renamed.txt").is_err(),
            "public rename must not mutate the hidden legacy row when authoritative V2 is trashed"
        );
        assert!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .is_none(),
            "the still-live legacy row must remain hidden while its valid V2 identity is trashed"
        );
        let mut legacy = conn
            .prepare("SELECT display_name, trashed_at FROM drive_file_entries WHERE entry_id=?1")
            .unwrap();
        legacy.bind((1, "shared-entry")).unwrap();
        assert!(matches!(legacy.next().unwrap(), sqlite::State::Row));
        assert_eq!(
            legacy.read::<String, _>("display_name").unwrap(),
            "stale-name.txt"
        );
        assert_eq!(
            legacy.read::<Option<i64>, _>("trashed_at").unwrap(),
            None,
            "failed restore must leave the legacy row intact rather than partially retire it"
        );
    }

    #[test]
    fn public_file_rename_retires_authoritative_v2_legacy_shadow_atomically() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            681,
            21,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            682,
            22,
        );

        rename_drive_file(&conn, "shared-entry", None, "stale-name.txt")
            .expect("the public provider rename must rename the V2 identity onto its legacy alias");
        rename_drive_file(&conn, "shared-entry", None, "third-name.txt")
            .expect("the public provider rename must allow the same V2 identity to move away");

        let source = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("the authoritative V2 entry remains visible");
        assert_eq!(source.storage_version, 2);
        assert_eq!(source.display_name, "third-name.txt");
        assert!(
            crate::drive_lookup::child(&conn, None, "stale-name.txt")
                .unwrap()
                .is_none(),
            "the generic provider path must not leave the stale legacy path visible"
        );

        trash_drive_file(&conn, "shared-entry").unwrap();
        assert!(
            crate::drive_lookup::file(&conn, "shared-entry")
                .unwrap()
                .is_none(),
            "trashing the moved V2 entry must not resurrect its retired legacy shadow"
        );
    }

    #[test]
    fn public_file_rename_rolls_back_when_legacy_shadow_retirement_fails() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "shared-entry",
            "legacy-source-file",
            "legacy-source-channel",
            "stale-name.txt",
            679,
            19,
        );
        insert_v2_test_file(
            &conn,
            "shared-entry",
            "authoritative-source-object",
            None,
            "current-name.txt",
            680,
            20,
        );
        conn.execute(
            "CREATE TRIGGER fail_legacy_shadow_retirement
             BEFORE UPDATE OF trashed_at ON drive_file_entries
             WHEN NEW.entry_id = 'shared-entry'
             BEGIN SELECT RAISE(ABORT, 'simulated shadow retirement failure'); END;",
        )
        .unwrap();

        assert!(
            rename_drive_file(&conn, "shared-entry", None, "renamed.txt").is_err(),
            "failure to retire a legacy shadow must abort the generic rename"
        );
        let source = crate::drive_lookup::file(&conn, "shared-entry")
            .unwrap()
            .expect("the original authoritative V2 entry must survive rollback");
        assert_eq!(source.storage_version, 2);
        assert_eq!(source.display_name, "current-name.txt");
        let mut legacy = conn
            .prepare("SELECT display_name, trashed_at FROM drive_file_entries WHERE entry_id=?1")
            .unwrap();
        legacy.bind((1, "shared-entry")).unwrap();
        assert!(matches!(legacy.next().unwrap(), sqlite::State::Row));
        assert_eq!(
            legacy.read::<String, _>("display_name").unwrap(),
            "stale-name.txt"
        );
        assert_eq!(legacy.read::<Option<i64>, _>("trashed_at").unwrap(), None);
    }

    #[test]
    fn public_file_rename_falls_back_for_orphan_v2_source() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "legacy-fallback",
            "legacy-fallback-file",
            "legacy-fallback-channel",
            "legacy-name.txt",
            683,
            23,
        );
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('legacy-fallback', 'missing-fallback-object', NULL, 'orphan-name.txt', 2, 2, NULL);",
        )
        .unwrap();

        rename_drive_file(&conn, "legacy-fallback", None, "renamed-legacy.txt")
            .expect("a missing V2 object must not shadow the valid legacy source");
        let source = crate::drive_lookup::file(&conn, "legacy-fallback")
            .unwrap()
            .expect("the legacy fallback remains visible");
        assert_eq!(source.storage_version, 1);
        assert_eq!(source.display_name, "renamed-legacy.txt");
        let mut legacy = conn
            .prepare("SELECT trashed_at FROM drive_file_entries WHERE entry_id=?1")
            .unwrap();
        legacy.bind((1, "legacy-fallback")).unwrap();
        assert!(matches!(legacy.next().unwrap(), sqlite::State::Row));
        assert_eq!(legacy.read::<Option<i64>, _>(0).unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_file_rename_does_not_reinitialize_the_drive_schema() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('rename-channel', 301, 'rename-channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('rename-file', 'rename-channel', 1, 'source.txt', 128,
                     'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('rename-entry', 'rename-file', NULL, 'source.txt', 1, 1,
                     NULL, 0, 128, NULL, NULL);",
        )
        .unwrap();
        DRIVE_SCHEMA_INIT_CALLS.with(|calls| calls.set(0));

        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "rename-entry",
            None,
            "renamed.txt",
            false,
            |_| false,
        )
        .unwrap();

        assert_eq!(
            DRIVE_SCHEMA_INIT_CALLS.with(|calls| calls.get()),
            0,
            "the initialized native rename hot path must not rerun schema setup"
        );
        assert_eq!(
            crate::drive_lookup::file(&conn, "rename-entry")
                .unwrap()
                .unwrap()
                .display_name,
            "renamed.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_rename_replaces_closed_local_staging_atomically() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        create_drive_pending_file(
            &conn,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        mark_drive_pending_closed(&conn, "destination").unwrap();
        let removed = rename_drive_item_native(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(removed.id, "destination");
        assert!(drive_pending_by_id(&conn, "destination").unwrap().is_none());
        assert_eq!(
            drive_pending_by_id(&conn, "source")
                .unwrap()
                .unwrap()
                .display_name,
            "destination.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_no_replace_and_busy_staging_preserve_both_files() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        create_drive_pending_file(
            &conn,
            "destination",
            None,
            "destination.txt",
            "destination-stage",
        )
        .unwrap();
        for no_replace in [true, false] {
            assert!(rename_drive_item_native(
                &conn,
                NativeRenameKind::Pending,
                "source",
                None,
                "destination.txt",
                no_replace,
                |_| false
            )
            .is_err());
            assert_eq!(
                drive_pending_by_id(&conn, "source")
                    .unwrap()
                    .unwrap()
                    .display_name,
                "source.txt"
            );
            assert!(drive_pending_by_id(&conn, "destination").unwrap().is_some());
        }
        mark_drive_pending_closed(&conn, "destination").unwrap();
        assert!(rename_drive_item_native(
            &conn,
            NativeRenameKind::Pending,
            "source",
            None,
            "destination.txt",
            false,
            |_| true
        )
        .is_err());
        assert!(drive_pending_by_id(&conn, "destination").unwrap().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_missing_parent_preserves_the_source() {
        let conn = memory_db();
        create_drive_pending_file(&conn, "source", None, "source.txt", "source-stage").unwrap();
        assert!(rename_drive_item_native(
            &conn,
            NativeRenameKind::Pending,
            "source",
            Some("missing"),
            "moved.txt",
            false,
            |_| false
        )
        .is_err());
        let source = drive_pending_by_id(&conn, "source").unwrap().unwrap();
        assert_eq!(source.display_name, "source.txt");
        assert!(source.directory_id.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_failed_directory_move_rolls_back_its_destination() {
        let conn = memory_db();
        let source = create_drive_directory(&conn, None, "source").unwrap();
        let child = create_drive_directory(&conn, Some(&source.id), "child").unwrap();
        let destination = create_drive_directory(&conn, Some(&child.id), "destination").unwrap();
        assert!(rename_drive_item_native(
            &conn,
            NativeRenameKind::Directory,
            &source.id,
            Some(&child.id),
            "destination",
            false,
            |_| false
        )
        .is_err());
        let snapshot = drive_snapshot(&conn).unwrap();
        assert!(snapshot
            .directories
            .iter()
            .any(|item| item.id == destination.id));
        assert!(snapshot
            .directories
            .iter()
            .any(|item| item.id == source.id && item.parent_id.is_none()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_remote_file_replacement_keeps_previous_content_in_trash() {
        let conn = memory_db();
        insert_v2_test_file(
            &conn,
            "source-entry",
            "source-object",
            None,
            "source.txt",
            111,
            1,
        );
        insert_v2_test_file(
            &conn,
            "destination-entry",
            "destination-object",
            None,
            "destination.txt",
            111,
            2,
        );
        rename_drive_item_native(
            &conn,
            NativeRenameKind::File,
            "source-entry",
            None,
            "destination.txt",
            false,
            |_| false,
        )
        .unwrap();
        let snapshot = drive_snapshot(&conn).unwrap();
        assert!(snapshot
            .files
            .iter()
            .any(|item| item.entry_id == "source-entry" && item.display_name == "destination.txt"));
        assert!(!snapshot
            .files
            .iter()
            .any(|item| item.entry_id == "destination-entry"));
        let mut statement = conn
            .prepare("SELECT trashed_at FROM drive_entries_v2 WHERE entry_id='destination-entry'")
            .unwrap();
        assert!(matches!(statement.next().unwrap(), sqlite::State::Row));
        assert!(statement.read::<Option<i64>, _>(0).unwrap().is_some());
    }

    #[test]
    fn ensure_name_available_ignores_orphan_v2_metadata_without_object() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('orphan-entry', 'missing-object', NULL, 'available.txt', 1, 1, NULL);",
        )
        .unwrap();

        assert!(
            ensure_name_available(&conn, None, "available.txt", None, None, None).is_ok(),
            "a V2 metadata row without its backing object is not a visible namespace collision"
        );
    }

    #[test]
    fn ensure_name_available_uses_one_namespace_query() {
        let conn = memory_db();
        reset_drive_name_collision_query_steps();

        assert!(ensure_name_available(&conn, None, "unique-name.txt", None, None, None,).is_ok());

        assert_eq!(
            drive_name_collision_query_steps(),
            1,
            "portable-name collision checking should scan the namespace in one query"
        );
    }

    #[test]
    fn drive_namespace_rejects_unicode_case_collisions() {
        let conn = memory_db();
        create_drive_directory(&conn, None, "Résumé").unwrap();
        assert!(create_drive_directory(&conn, None, "résumé").is_err());

        create_drive_pending_file(
            &conn,
            "11111111222222223333333344444444",
            None,
            "Ångström.txt",
            "staging-a",
        )
        .unwrap();
        assert!(create_drive_pending_file(
            &conn,
            "55555555666666667777777788888888",
            None,
            "ångström.txt",
            "staging-b",
        )
        .is_err());
    }

    #[test]
    fn drive_names_are_portable_to_windows_explorer() {
        for invalid in [
            "CON",
            "nul.txt",
            "COM1.log",
            "LPT9",
            "a:b.txt",
            "what?.txt",
            "star*.txt",
            "pipe|name",
            "quote\"name",
            "back\\slash",
            "angle<name>",
            "trailing.",
            " leading.txt",
            "trailing.txt ",
            "control\u{001f}name",
        ] {
            assert!(
                validate_drive_name(invalid).is_err(),
                "expected Windows-incompatible Drive name to be rejected: {invalid:?}"
            );
        }
        for valid in [
            "report.txt",
            ".env",
            "COM10.txt",
            "project 2026",
            "résumé.pdf",
        ] {
            assert_eq!(validate_drive_name(valid).unwrap(), valid);
        }
    }

    fn insert_legacy_drive_test_file(
        conn: &sqlite::Connection,
        entry_id: &str,
        file_id: &str,
        logical_channel_id: &str,
        display_name: &str,
        backing_channel_id: i64,
        message_id: i64,
    ) {
        conn.execute(format!(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('{logical_channel_id}', {backing_channel_id}, '{logical_channel_id}', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('{file_id}', '{logical_channel_id}', {message_id}, '{display_name}',
                     128, 'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('{entry_id}', '{file_id}', NULL, '{display_name}', 1, 1,
                     NULL, 0, 128, NULL, NULL);"
        ))
        .unwrap();
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
    fn legacy_drive_snapshot_rejects_coerced_file_numeric_fields() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "legacy-entry",
            "legacy-content",
            "legacy-channel",
            "legacy.txt",
            101,
            55,
        );
        assert_eq!(drive_snapshot(&conn).unwrap().files[0].total_size, 128);
        for (table, column, valid) in [
            ("logical_files", "total_size", 128),
            ("logical_files", "first_message_id", 55),
            ("logical_channels", "backing_channel_id", 101),
            ("logical_files", "created_at", 1),
            ("drive_file_entries", "updated_at", 1),
        ] {
            for corrupt in ["12.5", "'not-an-integer'", "x'3132'"] {
                conn.execute(format!("UPDATE {table} SET {column}={corrupt}"))
                    .unwrap();
                assert!(
                    drive_snapshot(&conn).is_err(),
                    "legacy snapshot coerced {table}.{column}={corrupt}"
                );
                conn.execute(format!("UPDATE {table} SET {column}={valid}"))
                    .unwrap();
            }
        }
        let valid = drive_snapshot(&conn).unwrap();
        assert_eq!(valid.files.len(), 1);
        assert_eq!(valid.files[0].total_size, 128);
        assert_eq!(valid.files[0].first_message_id, 55);
    }

    #[test]
    fn corrupt_v2_object_metadata_fails_closed_for_snapshot_and_direct_file() {
        let conn = memory_db();
        let entry_id = "qa-corrupt-v2-entry";
        let object_id = "qa-corrupt-v2-object";
        insert_v2_test_file(&conn, entry_id, object_id, None, "qa-v2.txt", 101, 55);

        assert!(drive_v2_file_by_entry(&conn, entry_id).unwrap().is_some());
        assert_eq!(load_v2_files(&conn).unwrap().len(), 1);
        assert!(drive_snapshot(&conn).is_ok());
        assert_eq!(load_manifest_objects(&conn).unwrap().len(), 1);

        for (field, mutation) in [
            ("negative size", "plaintext_size=-1"),
            ("size type", "plaintext_size=X'FF'"),
            ("encryption type", "encryption_version=X'FF'"),
            ("encryption unsupported", "encryption_version=9"),
            ("crypto identity", "crypto_id=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_objects
                 SET plaintext_size=1, encryption_version=0, crypto_id=NULL
                 WHERE object_id='qa-corrupt-v2-object'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_objects SET {mutation} WHERE object_id='qa-corrupt-v2-object'"
            ))
            .unwrap();
            assert!(
                drive_v2_file_by_entry(&conn, entry_id).is_err(),
                "{field}: direct lookup cannot fabricate object metadata"
            );
            assert!(
                load_v2_files(&conn).is_err(),
                "{field}: V2 snapshot loader must fail closed"
            );
            assert!(
                drive_snapshot(&conn).is_err(),
                "{field}: Drive snapshot must fail closed"
            );
            assert!(
                load_manifest_objects(&conn).is_err(),
                "{field}: sync manifest loader must fail closed"
            );
        }

        conn.execute(
            "UPDATE drive_objects
             SET plaintext_size=1, encryption_version=0, crypto_id=NULL
             WHERE object_id='qa-corrupt-v2-object'",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_entries_v2 SET directory_id=X'FF'
             WHERE entry_id='qa-corrupt-v2-entry'",
        )
        .unwrap();
        assert!(drive_v2_file_by_entry(&conn, entry_id).is_err());
        assert!(load_v2_files(&conn).is_err());
        assert!(drive_snapshot(&conn).is_err());
        conn.execute(
            "UPDATE drive_entries_v2 SET directory_id=NULL
             WHERE entry_id='qa-corrupt-v2-entry'",
        )
        .unwrap();
        assert!(drive_v2_file_by_entry(&conn, entry_id).unwrap().is_some());
        assert!(drive_snapshot(&conn).is_ok());
    }

    #[test]
    fn corrupt_v2_manifest_block_metadata_fails_closed() {
        let conn = memory_db();
        insert_v2_test_file(
            &conn,
            "qa-manifest-entry",
            "qa-manifest-object",
            None,
            "qa-manifest.txt",
            101,
            55,
        );
        assert_eq!(load_manifest_objects(&conn).unwrap().len(), 1);
        for (field, mutation) in [
            ("negative block count", "block_count=-1"),
            ("block count type", "block_count=X'FF'"),
            ("persisted hashes type", "block_hashes_b64=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_objects SET block_count=1, block_hashes_b64=NULL
                 WHERE object_id='qa-manifest-object'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_objects SET {mutation} WHERE object_id='qa-manifest-object'"
            ))
            .unwrap();
            assert!(
                load_manifest_objects(&conn).is_err(),
                "{field}: corrupt block metadata must not be exported"
            );
        }
        // A corrupted SQLite block count must fail as ordinary metadata,
        // never panic while attempting to reserve an impossible hash buffer.
        conn.execute(
            "UPDATE drive_objects SET block_count=9223372036854775807,
                    block_hashes_b64=NULL WHERE object_id='qa-manifest-object'",
        )
        .unwrap();
        let extreme = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            load_manifest_objects(&conn)
        }));
        assert!(
            matches!(extreme, Ok(Err(_))),
            "corrupt huge block count must return an error without panicking or allocating"
        );

        conn.execute(
            "UPDATE drive_objects SET block_count=1, block_hashes_b64=NULL
             WHERE object_id='qa-manifest-object'",
        )
        .unwrap();
        assert_eq!(load_manifest_objects(&conn).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_v2_part_offsets_and_entry_tombstone_fail_closed() {
        let conn = memory_db();
        insert_v2_test_file(
            &conn,
            "qa-part-entry",
            "qa-part-object",
            None,
            "qa-part.txt",
            101,
            55,
        );
        assert_eq!(load_manifest_parts(&conn).unwrap().len(), 1);
        assert_eq!(load_manifest_v2_entries(&conn).unwrap().len(), 1);

        for (field, mutation) in [
            ("negative part index", "part_index=-1"),
            ("chunk offset", "first_chunk_index=-1"),
            ("chunk count", "chunk_count=-1"),
            ("ciphertext size", "ciphertext_size=-1"),
            ("part index type", "part_index=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_object_parts
                 SET part_index=0, first_chunk_index=0, chunk_count=1,
                     ciphertext_size=1
                 WHERE object_id='qa-part-object'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_object_parts SET {mutation}
                 WHERE object_id='qa-part-object'"
            ))
            .unwrap();
            assert!(
                load_manifest_parts(&conn).is_err(),
                "{field}: must not export a corrupt part as a huge unsigned value"
            );
        }
        conn.execute(
            "UPDATE drive_object_parts
             SET part_index=0, first_chunk_index=0, chunk_count=1, ciphertext_size=1
             WHERE object_id='qa-part-object'",
        )
        .unwrap();
        assert_eq!(load_manifest_parts(&conn).unwrap().len(), 1);

        for (field, mutation) in [
            ("tombstone type", "trashed_at=X'FF'"),
            ("directory type", "directory_id=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_entries_v2 SET trashed_at=NULL, directory_id=NULL
                 WHERE entry_id='qa-part-entry'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_entries_v2 SET {mutation}
                 WHERE entry_id='qa-part-entry'"
            ))
            .unwrap();
            assert!(
                load_manifest_v2_entries(&conn).is_err(),
                "{field}: must not silently revive or reparent a corrupted entry"
            );
        }
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=NULL, directory_id=NULL
             WHERE entry_id='qa-part-entry'",
        )
        .unwrap();
        assert_eq!(load_manifest_v2_entries(&conn).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_manifest_stages_keep_exact_bytes_and_independent_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-manifest-stage-qa-{:032x}",
            rand::random::<u128>()
        ));
        let old_fixed_file = root.join(DRIVE_MANIFEST_FILE);
        tokio::fs::create_dir(&root).await.unwrap();
        tokio::fs::write(&old_fixed_file, b"pre-existing staging content")
            .await
            .unwrap();

        let first_bytes = b"qa-manifest-first-publication";
        let second_bytes = b"qa-manifest-second-publication-with-other-length";
        let (first, second) = tokio::join!(
            stage_manifest_bytes(&root, first_bytes),
            stage_manifest_bytes(&root, second_bytes)
        );
        let first = first.unwrap();
        let second = second.unwrap();

        assert_ne!(
            first, second,
            "concurrent uploads must not share a staging file"
        );
        assert_eq!(first.file_name().unwrap(), DRIVE_MANIFEST_FILE);
        assert_eq!(second.file_name().unwrap(), DRIVE_MANIFEST_FILE);
        assert_ne!(first.parent(), second.parent());
        assert_eq!(tokio::fs::read(&first).await.unwrap(), first_bytes);
        assert_eq!(tokio::fs::read(&second).await.unwrap(), second_bytes);
        assert_eq!(
            tokio::fs::read(&old_fixed_file).await.unwrap(),
            b"pre-existing staging content",
            "a legacy shared staging file must never be overwritten"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&first, &second] {
                assert_eq!(
                    tokio::fs::metadata(path.parent().unwrap())
                        .await
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700,
                    "staged manifest can include sensitive Drive crypto metadata"
                );
            }
        }

        tokio::fs::remove_file(&first).await.unwrap();
        tokio::fs::remove_dir(first.parent().unwrap())
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(&second).await.unwrap(),
            second_bytes,
            "one completed upload cannot delete another upload's staged bytes"
        );

        tokio::fs::remove_file(&second).await.unwrap();
        tokio::fs::remove_dir(second.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::remove_file(&old_fixed_file).await.unwrap();
        tokio::fs::remove_dir(&root).await.unwrap();
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn staging_failure_preserves_unrelated_existing_output() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-manifest-stage-blocked-qa-{:032x}",
            rand::random::<u128>()
        ));
        tokio::fs::write(&root, b"unrelated existing file")
            .await
            .unwrap();
        let result = stage_manifest_bytes(&root, b"do not overwrite").await;
        assert!(result.is_err());
        assert_eq!(
            tokio::fs::read(&root).await.unwrap(),
            b"unrelated existing file"
        );
        tokio::fs::remove_file(&root).await.unwrap();
    }

    #[test]
    fn applying_manifest_preserves_known_remote_message_id() {
        let conn = memory_db();
        let manifest = build_manifest(&conn).unwrap();
        set_drive_clean(&conn, manifest.revision, Some(77)).unwrap();

        apply_manifest(&conn, &manifest).unwrap();

        let mut state = conn
            .prepare("SELECT last_remote_message_id FROM drive_state WHERE singleton = 1")
            .unwrap();
        assert!(matches!(state.next().unwrap(), sqlite::State::Row));
        assert_eq!(
            state.read::<Option<i64>, _>(0).unwrap(),
            Some(77),
            "Re-applying an equal remote manifest must not forget which Telegram message supplied it"
        );
        drop(state);

        let mut malformed = manifest.clone();
        malformed.revision = -1;
        assert!(
            validate_manifest(&malformed).is_err(),
            "a remote manifest cannot advertise a negative sync revision"
        );
        assert!(
            apply_manifest(&conn, &malformed).is_err(),
            "a negative remote revision must not overwrite local sync state"
        );
        assert_eq!(state_row(&conn).unwrap().0, manifest.revision);
        let mut state = conn
            .prepare("SELECT last_remote_message_id FROM drive_state WHERE singleton = 1")
            .unwrap();
        assert!(matches!(state.next().unwrap(), sqlite::State::Row));
        assert_eq!(state.read::<Option<i64>, _>(0).unwrap(), Some(77));
    }

    #[test]
    fn sync_revision_fence_preserves_local_edits_made_after_snapshot() {
        let conn = memory_db();
        let original = create_drive_directory(&conn, None, "Existing before sync").unwrap();
        let (revision, dirty) = state_row(&conn).unwrap();
        assert!(dirty);
        let stale_manifest = build_manifest(&conn).unwrap();
        assert!(ensure_drive_sync_snapshot_current(&conn, revision, dirty).is_ok());

        // Represents a local edit while upload_stream/send_message awaits Telegram.
        let later = create_drive_directory(&conn, None, "Edited during sync").unwrap();
        let (new_revision, new_dirty) = state_row(&conn).unwrap();
        assert!(new_revision > revision);
        assert!(new_dirty);
        assert!(
            ensure_drive_sync_snapshot_current(&conn, revision, dirty).is_err(),
            "a published stale manifest must not overwrite local changes"
        );
        assert!(
            load_directories(&conn)
                .unwrap()
                .iter()
                .any(|row| row.id == later.id),
            "the newer local directory must survive a refused sync"
        );
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .any(|row| row.id == original.id),);
        assert!(
            !stale_manifest
                .directories
                .iter()
                .any(|row| row.id == later.id),
            "the upload snapshot really predates the local edit"
        );
        assert!(
            ensure_drive_sync_snapshot_current(&conn, new_revision, new_dirty).is_ok(),
            "an updated snapshot is eligible for retry"
        );
    }

    #[test]
    fn sync_revision_fence_detects_dirty_flag_and_clean_state_changes() {
        let conn = memory_db();
        let (revision, dirty) = state_row(&conn).unwrap();
        assert!(!dirty);
        assert!(ensure_drive_sync_snapshot_current(&conn, revision, dirty).is_ok());

        // Even if a corrupt/imported state reuses a revision number, a dirty
        // flag transition must not be silently declared a clean sync.
        conn.execute("UPDATE drive_state SET dirty=1 WHERE singleton=1")
            .unwrap();
        assert!(ensure_drive_sync_snapshot_current(&conn, revision, dirty).is_err());

        let (updated_revision, updated_dirty) = state_row(&conn).unwrap();
        assert!(updated_dirty);
        set_drive_clean(&conn, updated_revision + 1, Some(77)).unwrap();
        assert!(
            ensure_drive_sync_snapshot_current(&conn, updated_revision, updated_dirty).is_err(),
            "another overlapping sync committing a newer revision must invalidate this snapshot"
        );
        assert_eq!(state_row(&conn).unwrap(), (updated_revision + 1, false));
        assert!(ensure_drive_sync_snapshot_current(&conn, updated_revision + 1, false).is_ok());
    }

    #[test]
    fn stale_manifest_cleanup_selects_only_prior_positive_unique_ids() {
        assert_eq!(
            stale_manifest_ids(&[13, 25, 25, -7, 0, 34, 13], 34),
            vec![13, 25],
            "do not delete the newly published manifest or duplicate invalid IDs"
        );
        assert!(stale_manifest_ids(&[34, 34, 0], 34).is_empty());
        assert!(stale_manifest_ids(&[], 55).is_empty());
    }

    #[test]
    fn applying_manifest_refuses_corrupt_remote_cursor_without_replacing_local_filesystem() {
        let conn = memory_db();
        create_drive_directory(&conn, None, "Manifest version").unwrap();
        let manifest = build_manifest(&conn).unwrap();
        let local_only = create_drive_directory(&conn, None, "Keep local folder").unwrap();

        for (case, value) in [
            ("blob cursor", "X'FF'"),
            ("text cursor", "'not-a-message'"),
            ("fractional cursor", "3.5"),
            ("zero cursor", "0"),
            ("negative cursor", "-7"),
        ] {
            conn.execute(format!(
                "UPDATE drive_state SET last_remote_message_id={value} WHERE singleton=1"
            ))
            .unwrap();
            assert!(
                apply_manifest(&conn, &manifest).is_err(),
                "{case}: corrupt sync cursor must stop manifest application"
            );
            assert!(
                load_directories(&conn)
                    .unwrap()
                    .iter()
                    .any(|dir| dir.id == local_only.id),
                "{case}: rejected manifest must leave local Drive directory intact"
            );
        }

        // A missing state record also cannot silently become a NULL cursor.
        conn.execute("DELETE FROM drive_state WHERE singleton=1")
            .unwrap();
        assert!(apply_manifest(&conn, &manifest).is_err());
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .any(|dir| dir.id == local_only.id));

        // Genuine legacy NULL and a valid remote message ID still work.
        conn.execute(
            "INSERT INTO drive_state(singleton,revision,dirty,last_remote_message_id)
             VALUES(1,0,1,NULL)",
        )
        .unwrap();
        apply_manifest(&conn, &manifest).unwrap();
        assert!(!load_directories(&conn)
            .unwrap()
            .iter()
            .any(|dir| dir.id == local_only.id));
        set_drive_clean(&conn, manifest.revision, Some(77)).unwrap();
        apply_manifest(&conn, &manifest).unwrap();
        let mut stmt = conn
            .prepare("SELECT last_remote_message_id FROM drive_state WHERE singleton=1")
            .unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<Option<i64>, _>(0).unwrap(), Some(77));
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
    fn corrupt_directory_metadata_cannot_reparent_or_revive_a_sync_tombstone() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES
                ('qa-dir-parent', NULL, 'Parent', NULL, 1, 1, NULL),
                ('qa-dir-child', 'qa-dir-parent', 'Child', NULL, 1, 1, NULL);",
        )
        .unwrap();
        assert_eq!(load_directories(&conn).unwrap().len(), 2);
        assert_eq!(build_manifest(&conn).unwrap().directories.len(), 2);

        for (field, invalid) in [
            (
                "parent_id",
                "UPDATE drive_directories SET parent_id=X'FF' WHERE id='qa-dir-child'",
            ),
            (
                "backing_logical_channel_id",
                "UPDATE drive_directories SET backing_logical_channel_id=X'FF'
                 WHERE id='qa-dir-child'",
            ),
            (
                "trashed_at",
                "UPDATE drive_directories SET trashed_at=X'FF' WHERE id='qa-dir-child'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_directories
                 SET parent_id='qa-dir-parent', backing_logical_channel_id=NULL, trashed_at=NULL
                 WHERE id='qa-dir-child'",
            )
            .unwrap();
            conn.execute(invalid).unwrap();
            assert!(
                build_manifest(&conn).is_err(),
                "{field}: corrupt namespace or tombstone must never enter the sync manifest"
            );
            if field != "trashed_at" {
                assert!(
                    drive_snapshot(&conn).is_err(),
                    "{field}: corrupt folder must not appear in the Drive view"
                );
            }
        }
        conn.execute(
            "UPDATE drive_directories
             SET parent_id='qa-dir-parent', backing_logical_channel_id=NULL, trashed_at=NULL
             WHERE id='qa-dir-child'",
        )
        .unwrap();
        assert!(drive_snapshot(&conn).is_ok());
        assert!(build_manifest(&conn).is_ok());
    }

    #[test]
    fn corrupt_legacy_drive_entry_metadata_does_not_drop_crypto_or_trash_state() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-channel', 200, 'Test channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-file', 'qa-channel', 1, 'fixture.bin',
                     42, 'application/octet-stream', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('qa-entry', 'qa-file', NULL, 'fixture.bin', 1, 1,
                     NULL, 0, NULL, NULL, NULL);",
        )
        .unwrap();
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 1);
        assert_eq!(build_manifest(&conn).unwrap().entries.len(), 1);
        for (field, invalid) in [
            (
                "directory_id",
                "UPDATE drive_file_entries SET directory_id=X'FF' WHERE entry_id='qa-entry'",
            ),
            (
                "encryption_version",
                "UPDATE drive_file_entries SET encryption_version=X'FF' WHERE entry_id='qa-entry'",
            ),
            (
                "encryption_version_range",
                "UPDATE drive_file_entries SET encryption_version=2 WHERE entry_id='qa-entry'",
            ),
            (
                "plaintext_size",
                "UPDATE drive_file_entries SET plaintext_size=X'FF' WHERE entry_id='qa-entry'",
            ),
            (
                "negative_plaintext_size",
                "UPDATE drive_file_entries SET plaintext_size=-1 WHERE entry_id='qa-entry'",
            ),
            (
                "crypto_id",
                "UPDATE drive_file_entries SET crypto_id=X'FF' WHERE entry_id='qa-entry'",
            ),
            (
                "content_fingerprint",
                "UPDATE drive_file_entries SET content_fingerprint=X'FF'
                 WHERE entry_id='qa-entry'",
            ),
            (
                "trashed_at",
                "UPDATE drive_file_entries SET trashed_at=X'FF' WHERE entry_id='qa-entry'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_file_entries
                 SET directory_id=NULL, encryption_version=0, plaintext_size=NULL,
                     crypto_id=NULL, content_fingerprint=NULL, trashed_at=NULL
                 WHERE entry_id='qa-entry'",
            )
            .unwrap();
            conn.execute(invalid).unwrap();
            assert!(
                build_manifest(&conn).is_err(),
                "{field}: invalid crypto/namespace/tombstone metadata must block sync"
            );
            if field != "trashed_at" {
                assert!(
                    drive_snapshot(&conn).is_err(),
                    "{field}: invalid active entry must not appear decrypted or relocated"
                );
                assert!(
                    drive_file_by_entry(&conn, "qa-entry").is_err(),
                    "{field}: targeted file lookup must reject corrupt persisted metadata"
                );
                assert!(
                    drive_file_by_entry_id(&conn, "qa-entry").is_err(),
                    "{field}: direct ID lookup must reject corrupt persisted metadata"
                );
            }
        }
        conn.execute(
            "UPDATE drive_file_entries
             SET directory_id=NULL, encryption_version=0, plaintext_size=NULL,
                 crypto_id=NULL, content_fingerprint=NULL, trashed_at=NULL
             WHERE entry_id='qa-entry'",
        )
        .unwrap();
        assert!(drive_snapshot(&conn).is_ok());
        assert!(build_manifest(&conn).is_ok());
        for (field, mutation) in [
            (
                "negative_total_size",
                "UPDATE logical_files SET total_size=-1 WHERE file_id='qa-file'",
            ),
            (
                "blob_total_size",
                "UPDATE logical_files SET total_size=X'FF' WHERE file_id='qa-file'",
            ),
            (
                "blob_mime_type",
                "UPDATE logical_files SET mime_type=X'FF' WHERE file_id='qa-file'",
            ),
        ] {
            conn.execute("UPDATE logical_files SET total_size=42, mime_type='application/octet-stream' WHERE file_id='qa-file'").unwrap();
            conn.execute(mutation).unwrap();
            assert!(
                drive_file_by_entry(&conn, "qa-entry").is_err(),
                "{field}: corrupt logical file metadata must not be returned"
            );
            assert!(
                drive_file_by_entry_id(&conn, "qa-entry").is_err(),
                "{field}: ID lookup must reject corrupt logical file metadata"
            );
        }
        conn.execute("UPDATE logical_files SET total_size=42, mime_type='application/octet-stream' WHERE file_id='qa-file'").unwrap();
        assert!(drive_file_by_entry(&conn, "qa-entry").unwrap().is_some());
        assert!(drive_file_by_entry_id(&conn, "qa-entry").unwrap().is_some());
    }

    #[test]
    fn corrupt_pending_metadata_must_fail_in_snapshot_direct_and_targeted_lookups() {
        let conn = memory_db();
        let pending_id = "qa-corrupt-pending-metadata";
        let valid = create_drive_pending_file(
            &conn,
            pending_id,
            None,
            "safe-pending.txt",
            "/tmp/qa-only-corrupt-pending-metadata",
        )
        .unwrap();
        let baseline = drive_pending_by_id(&conn, pending_id).unwrap().unwrap();
        assert_eq!(baseline.format_version, valid.format_version);
        assert_eq!(baseline.encryption_version, valid.encryption_version);
        assert!(drive_snapshot(&conn).is_ok());
        assert!(crate::drive_lookup::pending_for_file(&conn, pending_id)
            .unwrap()
            .is_some());

        // SQLite INTEGER/TEXT affinities can store incompatible BLOB values.
        // The old .ok().flatten() / unwrap_or() decoder silently converted
        // failed reads to missing metadata or unencrypted legacy versions.
        for (field, mutation) in [
            (
                "format_version",
                "UPDATE drive_pending_files SET format_version=X'FF'",
            ),
            (
                "format_version_range",
                "UPDATE drive_pending_files SET format_version=3",
            ),
            (
                "encryption_version",
                "UPDATE drive_pending_files SET encryption_version=X'FF'",
            ),
            (
                "encryption_version_range",
                "UPDATE drive_pending_files SET encryption_version=5",
            ),
            (
                "crypto_id",
                "UPDATE drive_pending_files SET crypto_id=X'FF'",
            ),
            (
                "directory_id",
                "UPDATE drive_pending_files SET directory_id=X'FF'",
            ),
            (
                "base_entry_id",
                "UPDATE drive_pending_files SET base_entry_id=X'FF'",
            ),
            (
                "closed_at",
                "UPDATE drive_pending_files SET closed_at=X'FF'",
            ),
            (
                "backing_channel_id",
                "UPDATE drive_pending_files SET backing_channel_id=X'FF'",
            ),
            ("size", "UPDATE drive_pending_files SET size=-1"),
        ] {
            conn.execute(
                "UPDATE drive_pending_files
                 SET directory_id=NULL, size=0, format_version=2,
                     backing_channel_id=0, base_entry_id=NULL,
                     encryption_version=0, crypto_id=NULL, closed_at=NULL
                 WHERE id='qa-corrupt-pending-metadata'",
            )
            .unwrap();
            conn.execute(mutation).unwrap();
            assert!(
                drive_pending_by_id(&conn, pending_id).is_err(),
                "{field}: direct pending lookup must fail closed"
            );
            assert!(
                drive_snapshot(&conn).is_err(),
                "{field}: snapshot must not expose a fabricated pending record"
            );
            assert!(
                crate::drive_lookup::pending_for_file(&conn, pending_id).is_err(),
                "{field}: native filesystem lookup must fail closed"
            );
        }
        conn.execute(
            "UPDATE drive_pending_files
             SET directory_id=NULL, size=0, format_version=2,
                 backing_channel_id=0, base_entry_id=NULL,
                 encryption_version=0, crypto_id=NULL, closed_at=NULL
             WHERE id='qa-corrupt-pending-metadata'",
        )
        .unwrap();
        assert!(drive_pending_by_id(&conn, pending_id).unwrap().is_some());
        assert!(drive_snapshot(&conn).is_ok());
    }

    #[test]
    fn pending_range_mutations_can_join_one_caller_savepoint_and_rollback() {
        let conn = memory_db();
        let pending_id = "range-transaction-rollback";
        create_drive_pending_file(
            &conn,
            pending_id,
            None,
            "safe-rollback.txt",
            "/tmp/qa-pending-range-marker",
        )
        .unwrap();
        record_drive_pending_range(&conn, pending_id, 0, 16).unwrap();
        update_drive_pending_size(&conn, pending_id, 16).unwrap();

        conn.execute("SAVEPOINT qa_resize_atomic").unwrap();
        record_drive_pending_range_in_transaction(&conn, pending_id, 16, 24).unwrap();
        clear_drive_pending_ranges_for_span_in_transaction(&conn, pending_id, 4, 20).unwrap();
        update_drive_pending_size(&conn, pending_id, 24).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 4), (20, 24)]
        );
        conn.execute("ROLLBACK TO qa_resize_atomic; RELEASE qa_resize_atomic")
            .unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 16)]
        );
        assert_eq!(
            drive_pending_by_id(&conn, pending_id)
                .unwrap()
                .unwrap()
                .size,
            16
        );

        conn.execute("SAVEPOINT qa_resize_commit").unwrap();
        record_drive_pending_range_in_transaction(&conn, pending_id, 16, 24).unwrap();
        clear_drive_pending_ranges_for_span_in_transaction(&conn, pending_id, 4, 20).unwrap();
        update_drive_pending_size(&conn, pending_id, 24).unwrap();
        conn.execute("RELEASE qa_resize_commit").unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 4), (20, 24)]
        );
        assert_eq!(
            drive_pending_by_id(&conn, pending_id)
                .unwrap()
                .unwrap()
                .size,
            24
        );

        // Invalid clear bounds must never turn a valid dirty extent into
        // overlapping database records or mutate its persisted coverage.
        let unchanged = drive_pending_ranges(&conn, pending_id).unwrap();
        assert!(
            clear_drive_pending_ranges_for_span(&conn, pending_id, 22, 8).is_err(),
            "reversed clear bounds must not generate overlapping dirty ranges"
        );
        assert_eq!(drive_pending_ranges(&conn, pending_id).unwrap(), unchanged);
        conn.execute("SAVEPOINT qa_reversed_clear").unwrap();
        assert!(
            clear_drive_pending_ranges_for_span_in_transaction(&conn, pending_id, 22, 8).is_err()
        );
        assert_eq!(drive_pending_ranges(&conn, pending_id).unwrap(), unchanged);
        conn.execute("ROLLBACK TO qa_reversed_clear; RELEASE qa_reversed_clear")
            .unwrap();
        clear_drive_pending_ranges_for_span(&conn, pending_id, 12, 12).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            unchanged,
            "zero-length clears must not split the persisted dirty ranges"
        );

        // Reversed write bounds are an invalid caller operation, not a
        // successful empty write: silently ignoring one loses dirty bytes.
        assert!(
            record_drive_pending_range(&conn, pending_id, 23, 8).is_err(),
            "reversed writes must not report a successful dirty-range update"
        );
        assert_eq!(drive_pending_ranges(&conn, pending_id).unwrap(), unchanged);
        conn.execute("SAVEPOINT qa_reversed_record").unwrap();
        assert!(record_drive_pending_range_in_transaction(&conn, pending_id, 23, 8).is_err());
        assert_eq!(drive_pending_ranges(&conn, pending_id).unwrap(), unchanged);
        conn.execute("ROLLBACK TO qa_reversed_record; RELEASE qa_reversed_record")
            .unwrap();
        record_drive_pending_range(&conn, pending_id, 12, 12).unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            unchanged,
            "zero-length writes must not alter the dirty-range journal"
        );

        // A stale native writer must not recreate an orphan journal after the
        // pending upload was removed. This table has no foreign key.
        let missing_id = "qa-disappeared-pending-write";
        assert!(drive_pending_by_id(&conn, missing_id).unwrap().is_none());
        assert!(
            record_drive_pending_range(&conn, missing_id, 0, 8).is_err(),
            "recording dirty bytes for a deleted pending upload must fail"
        );
        conn.execute("SAVEPOINT qa_missing_pending_range").unwrap();
        assert!(record_drive_pending_range_in_transaction(&conn, missing_id, 0, 8).is_err());
        conn.execute("ROLLBACK TO qa_missing_pending_range; RELEASE qa_missing_pending_range")
            .unwrap();
        assert!(
            drive_pending_ranges(&conn, missing_id).unwrap().is_empty(),
            "an absent pending upload must not gain orphan dirty-range records"
        );
        assert!(
            update_drive_pending_size(&conn, missing_id, 8).is_err(),
            "resizing a deleted pending write must not silently report success"
        );
        assert!(
            mark_drive_pending_closed(&conn, missing_id).is_err(),
            "closing a deleted pending upload must not report successful completion"
        );
        assert!(drive_pending_by_id(&conn, missing_id).unwrap().is_none());
        assert_eq!(drive_pending_ranges(&conn, pending_id).unwrap(), unchanged);

        for (case, mutation) in [
            (
                "negative start",
                "UPDATE drive_pending_ranges SET range_start=-1",
            ),
            (
                "negative end",
                "UPDATE drive_pending_ranges SET range_end=-1",
            ),
            (
                "reversed span",
                "UPDATE drive_pending_ranges SET range_start=22, range_end=5",
            ),
            (
                "blob start",
                "UPDATE drive_pending_ranges SET range_start=X'FF'",
            ),
        ] {
            conn.execute(
                "DELETE FROM drive_pending_ranges WHERE pending_id='range-transaction-rollback';
                 INSERT INTO drive_pending_ranges (pending_id, range_start, range_end)
                 VALUES ('range-transaction-rollback', 0, 16);",
            )
            .unwrap();
            conn.execute(format!(
                "{mutation} WHERE pending_id='range-transaction-rollback'"
            ))
            .unwrap();
            assert!(
                drive_pending_ranges(&conn, pending_id).is_err(),
                "{case}: corrupt persisted range must not be exposed"
            );
            assert!(
                record_drive_pending_range(&conn, pending_id, 16, 24).is_err(),
                "{case}: corrupt stored range must not be silently merged or overwritten"
            );
            assert!(
                clear_drive_pending_ranges_for_span(&conn, pending_id, 4, 8).is_err(),
                "{case}: corrupt stored range must not be silently erased"
            );
        }
        conn.execute(
            "DELETE FROM drive_pending_ranges WHERE pending_id='range-transaction-rollback';
             INSERT INTO drive_pending_ranges (pending_id, range_start, range_end)
             VALUES ('range-transaction-rollback', 0, 16);",
        )
        .unwrap();
        assert_eq!(
            drive_pending_ranges(&conn, pending_id).unwrap(),
            vec![(0, 16)]
        );
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
    fn corrupt_pending_upload_chunk_and_part_offsets_fail_closed() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_pending_chunks
             (pending_id, chunk_index, message_id, chunk_size, sha256,
              plaintext_sha256, uploaded_at)
             VALUES ('qa-pending-corrupt', 0, 501, 128, 'cipher-hash', 'plain-hash', 1);
             INSERT INTO drive_pending_parts
             (pending_id, part_index, first_chunk_index, chunk_count,
              backing_channel_id, message_id, part_size, sha256, uploaded_at)
             VALUES ('qa-pending-corrupt', 0, 0, 1, 201, 502, 128, 'part-hash', 1);",
        )
        .unwrap();
        assert_eq!(
            drive_pending_chunks(&conn, "qa-pending-corrupt")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            drive_pending_parts(&conn, "qa-pending-corrupt")
                .unwrap()
                .len(),
            1
        );

        for (description, mutation) in [
            ("negative chunk index", "chunk_index=-1"),
            ("negative chunk size", "chunk_size=-1"),
            ("wrong chunk index type", "chunk_index=X'FF'"),
            ("wrong chunk size type", "chunk_size=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_pending_chunks
                 SET chunk_index=0, chunk_size=128
                 WHERE pending_id='qa-pending-corrupt'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_pending_chunks SET {mutation}
                 WHERE pending_id='qa-pending-corrupt'"
            ))
            .unwrap();
            assert!(
                drive_pending_chunks(&conn, "qa-pending-corrupt").is_err(),
                "{description}: corrupted upload offsets must not become unsigned"
            );
        }
        conn.execute(
            "UPDATE drive_pending_chunks SET chunk_index=0, chunk_size=128
             WHERE pending_id='qa-pending-corrupt'",
        )
        .unwrap();

        for (description, mutation) in [
            ("negative part index", "part_index=-1"),
            ("negative first chunk", "first_chunk_index=-1"),
            ("negative chunk count", "chunk_count=-1"),
            ("negative part size", "part_size=-1"),
            ("wrong part index type", "part_index=X'FF'"),
            ("wrong first chunk type", "first_chunk_index=X'FF'"),
            ("wrong count type", "chunk_count=X'FF'"),
            ("wrong part size type", "part_size=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_pending_parts
                 SET part_index=0, first_chunk_index=0, chunk_count=1, part_size=128
                 WHERE pending_id='qa-pending-corrupt'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_pending_parts SET {mutation}
                 WHERE pending_id='qa-pending-corrupt'"
            ))
            .unwrap();
            assert!(
                drive_pending_parts(&conn, "qa-pending-corrupt").is_err(),
                "{description}: corrupted pending part must not be exposed"
            );
        }
        conn.execute(
            "UPDATE drive_pending_parts
             SET part_index=0, first_chunk_index=0, chunk_count=1, part_size=128
             WHERE pending_id='qa-pending-corrupt'",
        )
        .unwrap();
        assert_eq!(
            drive_pending_chunks(&conn, "qa-pending-corrupt").unwrap()[0].chunk_size,
            128
        );
        assert_eq!(
            drive_pending_parts(&conn, "qa-pending-corrupt").unwrap()[0].part_size,
            128
        );
    }

    #[test]
    fn corrupt_v2_object_part_and_block_offsets_fail_closed() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_objects
             (object_id, plaintext_size, encryption_version,
              content_fingerprint, block_count, created_at)
             VALUES ('qa-object-corrupt', 128, 0, 'qa-fingerprint', 1, 1);
             INSERT INTO drive_object_parts
             (object_id, part_index, first_chunk_index, chunk_count,
              backing_channel_id, message_id, ciphertext_size, sha256)
             VALUES ('qa-object-corrupt', 0, 0, 1, 201, 503, 128, 'part-hash');
             INSERT INTO drive_object_blocks
             (object_id, chunk_index, part_index, part_offset, ciphertext_size,
              sha256, plaintext_sha256)
             VALUES ('qa-object-corrupt', 0, 0, 0, 128, 'block-hash', 'plain-hash');",
        )
        .unwrap();
        assert_eq!(
            drive_object_parts(&conn, "qa-object-corrupt")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            drive_object_blocks(&conn, "qa-object-corrupt")
                .unwrap()
                .len(),
            1
        );

        for (description, mutation) in [
            ("negative part index", "part_index=-1"),
            ("negative first chunk", "first_chunk_index=-1"),
            ("negative part chunk count", "chunk_count=-1"),
            ("negative ciphertext size", "ciphertext_size=-1"),
            ("wrong part index type", "part_index=X'FF'"),
            ("wrong chunk count type", "chunk_count=X'FF'"),
            ("wrong ciphertext size type", "ciphertext_size=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_object_parts
                 SET part_index=0, first_chunk_index=0, chunk_count=1, ciphertext_size=128
                 WHERE object_id='qa-object-corrupt'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_object_parts SET {mutation}
                 WHERE object_id='qa-object-corrupt'"
            ))
            .unwrap();
            assert!(
                drive_object_parts(&conn, "qa-object-corrupt").is_err(),
                "{description}: corrupt remote part must not get a huge unsigned value"
            );
        }
        conn.execute(
            "UPDATE drive_object_parts
             SET part_index=0, first_chunk_index=0, chunk_count=1, ciphertext_size=128
             WHERE object_id='qa-object-corrupt'",
        )
        .unwrap();

        for (description, mutation) in [
            ("negative chunk index", "chunk_index=-1"),
            ("negative part index", "part_index=-1"),
            ("negative ciphertext offset", "part_offset=-1"),
            ("negative ciphertext size", "ciphertext_size=-1"),
            ("wrong chunk index type", "chunk_index=X'FF'"),
            ("wrong part offset type", "part_offset=X'FF'"),
            ("wrong ciphertext size type", "ciphertext_size=X'FF'"),
        ] {
            conn.execute(
                "UPDATE drive_object_blocks
                 SET chunk_index=0, part_index=0, part_offset=0, ciphertext_size=128
                 WHERE object_id='qa-object-corrupt'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_object_blocks SET {mutation}
                 WHERE object_id='qa-object-corrupt'"
            ))
            .unwrap();
            assert!(
                drive_object_blocks(&conn, "qa-object-corrupt").is_err(),
                "{description}: corrupt encrypted block must not get a huge offset"
            );
        }
        conn.execute(
            "UPDATE drive_object_blocks
             SET chunk_index=0, part_index=0, part_offset=0, ciphertext_size=128
             WHERE object_id='qa-object-corrupt'",
        )
        .unwrap();
        assert_eq!(
            drive_object_parts(&conn, "qa-object-corrupt").unwrap()[0].ciphertext_size,
            128
        );
        assert_eq!(
            drive_object_blocks(&conn, "qa-object-corrupt").unwrap()[0].part_offset,
            0
        );
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
    fn targeted_v2_block_location_rejects_corrupt_offsets_and_sizes() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_objects
             (object_id, plaintext_size, encryption_version, content_fingerprint,
              block_count, created_at)
             VALUES ('qa-targeted-object', 128, 0, 'qa-targeted-fingerprint', 1, 1);
             INSERT INTO drive_object_parts
             (object_id, part_index, first_chunk_index, chunk_count,
              backing_channel_id, message_id, ciphertext_size, sha256)
             VALUES ('qa-targeted-object', 0, 0, 1, 55, 501, 128, 'part-sha');
             INSERT INTO drive_object_blocks
             (object_id, chunk_index, part_index, part_offset,
              ciphertext_size, sha256, plaintext_sha256)
             VALUES ('qa-targeted-object', 0, 0, 0, 128, 'cipher-sha', 'plain-sha');",
        )
        .unwrap();
        let (block, part) = drive_object_block_location(&conn, "qa-targeted-object", 0)
            .unwrap()
            .unwrap();
        assert_eq!(block.ciphertext_size, 128);
        assert_eq!(part.ciphertext_size, 128);

        for (label, mutation) in [
            (
                "negative block offset",
                "UPDATE drive_object_blocks SET part_offset=-1",
            ),
            (
                "blob block offset",
                "UPDATE drive_object_blocks SET part_offset=X'FF'",
            ),
            (
                "negative block size",
                "UPDATE drive_object_blocks SET ciphertext_size=-1",
            ),
            (
                "blob block size",
                "UPDATE drive_object_blocks SET ciphertext_size=X'FF'",
            ),
            (
                "negative part first chunk",
                "UPDATE drive_object_parts SET first_chunk_index=-1",
            ),
            (
                "blob part first chunk",
                "UPDATE drive_object_parts SET first_chunk_index=X'FF'",
            ),
            (
                "negative part chunk count",
                "UPDATE drive_object_parts SET chunk_count=-1",
            ),
            (
                "blob part chunk count",
                "UPDATE drive_object_parts SET chunk_count=X'FF'",
            ),
            (
                "negative part size",
                "UPDATE drive_object_parts SET ciphertext_size=-1",
            ),
            (
                "blob part size",
                "UPDATE drive_object_parts SET ciphertext_size=X'FF'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_object_blocks SET part_offset=0, ciphertext_size=128
                 WHERE object_id='qa-targeted-object';
                 UPDATE drive_object_parts
                 SET first_chunk_index=0, chunk_count=1, ciphertext_size=128
                 WHERE object_id='qa-targeted-object'",
            )
            .unwrap();
            conn.execute(format!("{mutation} WHERE object_id='qa-targeted-object'"))
                .unwrap();
            reset_drive_object_block_location_query_steps();
            assert!(
                drive_object_block_location(&conn, "qa-targeted-object", 0).is_err(),
                "{label}: targeted read must reject corrupt persisted location"
            );
            assert_eq!(drive_object_block_location_query_steps(), 1);
        }

        conn.execute(
            "UPDATE drive_object_blocks SET part_offset=0, ciphertext_size=128
             WHERE object_id='qa-targeted-object';
             UPDATE drive_object_parts
             SET first_chunk_index=0, chunk_count=1, ciphertext_size=128
             WHERE object_id='qa-targeted-object'",
        )
        .unwrap();
        let (block, part) = drive_object_block_location(&conn, "qa-targeted-object", 0)
            .unwrap()
            .unwrap();
        assert_eq!(block.part_offset, 0);
        assert_eq!(part.chunk_count, 1);
    }

    #[test]
    fn pending_size_write_rejects_unsigned_overflow_without_mutating_record() {
        let conn = memory_db();
        let original = create_drive_pending_file(
            &conn,
            "qa-pending-overflow",
            None,
            "large.bin",
            "qa-nonexistent-staging",
        )
        .unwrap();

        let error = update_drive_pending_size(&conn, &original.id, u64::MAX)
            .expect_err("SQLite cannot persist u64::MAX without signed overflow");
        assert!(error.contains("SQLite integer limits"), "{error}");
        let after = drive_pending_by_id(&conn, &original.id).unwrap().unwrap();
        assert_eq!(after.size, original.size);
        assert_eq!(after.updated_at, original.updated_at);

        update_drive_pending_size(&conn, &original.id, 128).unwrap();
        assert_eq!(
            drive_pending_by_id(&conn, &original.id)
                .unwrap()
                .unwrap()
                .size,
            128
        );
    }

    #[test]
    fn copy_on_write_replacement_rejects_corrupt_legacy_parent_and_size() {
        let conn = memory_db();
        insert_legacy_drive_test_file(
            &conn,
            "qa-replace-source",
            "qa-replace-file",
            "qa-replace-channel",
            "edit.txt",
            901,
            902,
        );
        for (case, corruption) in [
            (
                "corrupt original parent",
                "UPDATE drive_file_entries SET directory_id=X'FF'
                 WHERE entry_id='qa-replace-source'",
            ),
            (
                "negative visible size",
                "UPDATE drive_file_entries SET plaintext_size=-1
                 WHERE entry_id='qa-replace-source'",
            ),
            (
                "negative backing size",
                "UPDATE drive_file_entries SET plaintext_size=NULL
                 WHERE entry_id='qa-replace-source';
                 UPDATE logical_files SET total_size=-1
                 WHERE file_id='qa-replace-file'",
            ),
        ] {
            conn.execute(
                "UPDATE drive_file_entries
                 SET directory_id=NULL, plaintext_size=128
                 WHERE entry_id='qa-replace-source';
                 UPDATE logical_files SET total_size=128
                 WHERE file_id='qa-replace-file'",
            )
            .unwrap();
            conn.execute(corruption).unwrap();
            let err = create_drive_pending_replacement(
                &conn,
                "qa-replace-pending",
                "qa-replace-source",
                "qa-no-real-staging",
            )
            .expect_err("invalid source metadata must never create a pending replacement");
            assert!(
                err.contains("directory_id") || err.contains("visible_size"),
                "{case}: {err}"
            );
            assert!(drive_pending_by_id(&conn, "qa-replace-pending")
                .unwrap()
                .is_none());
        }

        conn.execute(
            "UPDATE drive_file_entries SET directory_id=NULL, plaintext_size=128
             WHERE entry_id='qa-replace-source';
             UPDATE logical_files SET total_size=128 WHERE file_id='qa-replace-file'",
        )
        .unwrap();
        let pending = create_drive_pending_replacement(
            &conn,
            "qa-replace-pending",
            "qa-replace-source",
            "qa-no-real-staging",
        )
        .unwrap();
        assert_eq!(pending.size, 128);
        assert!(pending.directory_id.is_none());
        assert_eq!(pending.base_entry_id.as_deref(), Some("qa-replace-source"));
    }

    #[test]
    fn drive_object_block_location_reads_one_block_and_part_in_one_query() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_objects
                (object_id, plaintext_size, encryption_version, crypto_id, content_fingerprint,
                 block_count, block_hashes_b64, created_at)
             VALUES ('location-object', 4096, 2, 'location-crypto', 'location-fingerprint',
                     2, NULL, 1);
             INSERT INTO drive_object_parts
                (object_id, part_index, first_chunk_index, chunk_count, backing_channel_id,
                 message_id, ciphertext_size, sha256)
             VALUES ('location-object', 0, 0, 1, 42, 5001, 170, 'part-zero-sha'),
                    ('location-object', 1, 1, 1, 73, 5002, 190, 'part-one-sha');
             INSERT INTO drive_object_blocks
                (object_id, chunk_index, part_index, part_offset, ciphertext_size, sha256,
                 plaintext_sha256)
             VALUES ('location-object', 0, 0, 10, 170, 'block-zero-sha', 'plain-zero-sha'),
                    ('location-object', 1, 1, 0, 190, 'block-one-sha', 'plain-one-sha');",
        )
        .unwrap();

        reset_drive_object_block_location_query_steps();
        let (block0, part0) = drive_object_block_location(&conn, "location-object", 0)
            .unwrap()
            .unwrap();
        assert_eq!(drive_object_block_location_query_steps(), 1);
        assert_eq!(block0.object_id, "location-object");
        assert_eq!(block0.chunk_index, 0);
        assert_eq!(block0.part_index, 0);
        assert_eq!(block0.part_offset, 10);
        assert_eq!(block0.ciphertext_size, 170);
        assert_eq!(block0.sha256, "block-zero-sha");
        assert_eq!(block0.plaintext_sha256, "plain-zero-sha");
        assert_eq!(part0.object_id, "location-object");
        assert_eq!(part0.part_index, 0);
        assert_eq!(part0.first_chunk_index, 0);
        assert_eq!(part0.chunk_count, 1);
        assert_eq!(part0.backing_channel_id, 42);
        assert_eq!(part0.message_id, 5001);
        assert_eq!(part0.ciphertext_size, 170);
        assert_eq!(part0.sha256, "part-zero-sha");

        reset_drive_object_block_location_query_steps();
        let (block1, part1) = drive_object_block_location(&conn, "location-object", 1)
            .unwrap()
            .unwrap();
        assert_eq!(drive_object_block_location_query_steps(), 1);
        assert_eq!(block1.part_index, 1);
        assert_eq!(block1.part_offset, 0);
        assert_eq!(block1.ciphertext_size, 190);
        assert_eq!(part1.part_index, 1);
        assert_eq!(part1.backing_channel_id, 73);
        assert_eq!(part1.message_id, 5002);
        assert_eq!(part1.sha256, "part-one-sha");

        reset_drive_object_block_location_query_steps();
        assert!(drive_object_block_location(&conn, "location-object", 9)
            .unwrap()
            .is_none());
        assert_eq!(
            drive_object_block_location_query_steps(),
            1,
            "missing chunks should use the same single indexed lookup"
        );

        conn.execute(
            "DELETE FROM drive_object_parts
             WHERE object_id='location-object' AND part_index=1",
        )
        .unwrap();
        reset_drive_object_block_location_query_steps();
        assert_eq!(
            drive_object_block_location(&conn, "location-object", 1).unwrap_err(),
            "Drive V2 block references a missing remote part"
        );
        assert_eq!(drive_object_block_location_query_steps(), 1);
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
            (
                2u64,
                "80808080ababababcdcdcdcd12121212",
                "23232323343434344545454556565656",
                8101i64,
            ),
            (
                3u64,
                "66666666777777778888888899999999",
                "99999999eeeeeeeeddddddddcccccccc",
                9101i64,
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
            if ordinal == 2 {
                conn.execute(format!(
                    "DELETE FROM drive_object_blocks WHERE object_id='{finalized_object}'"
                ))
                .unwrap();
            } else if ordinal == 3 {
                conn.execute(format!(
                    "INSERT INTO drive_object_blocks
                     (object_id, chunk_index, part_index, part_offset, ciphertext_size,
                      sha256, plaintext_sha256)
                     VALUES ('{finalized_object}', 0, 0, 0, 1,
                             'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                             'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
                     DELETE FROM drive_object_parts WHERE object_id='{finalized_object}'"
                ))
                .unwrap();
            }
            if ordinal >= 2 {
                let before = state_row(&conn).unwrap();
                assert!(
                    finalize_drive_v2_metadata(&conn, &pending.id, proposed_object, fingerprint)
                        .is_err(),
                    "V2 dedup must not discard uploaded parts for a corrupt stored remote object (case {ordinal})"
                );
                assert_eq!(state_row(&conn).unwrap(), before);
                assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
                continue;
            }
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
    fn new_v2_upload_cannot_overwrite_existing_trashed_entry_with_colliding_id() {
        let conn = memory_db();
        let collided_id = "qa-v2-colliding-pending";
        let old_object = "22222222333333334444444455555555";
        insert_v2_test_file(&conn, collided_id, old_object, None, "old.bin", 202, 9001);
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=123 WHERE entry_id='qa-v2-colliding-pending'",
        )
        .unwrap();
        let pending =
            create_drive_pending_file(&conn, collided_id, None, "new.bin", "private-staging")
                .unwrap();
        update_drive_pending_size(&conn, &pending.id, 1).unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
        upsert_drive_pending_chunk(
            &conn,
            &pending.id,
            0,
            9002,
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
            9002,
            1,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        let before = state_row(&conn).unwrap();
        assert!(
            finalize_drive_v2_metadata(
                &conn,
                &pending.id,
                "66666666777777778888888899999999",
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            )
            .is_err(),
            "a new upload must never revive or replace someone else's trashed V2 entry"
        );
        assert_eq!(state_row(&conn).unwrap(), before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        let mut stmt = conn
            .prepare("SELECT object_id, trashed_at FROM drive_entries_v2 WHERE entry_id=?")
            .unwrap();
        stmt.bind((1, collided_id)).unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<String, _>(0).unwrap(), old_object);
        assert_eq!(stmt.read::<i64, _>(1).unwrap(), 123);
        drop(stmt);

        // V2 and legacy namespaces are separate tables but share entry IDs.
        // A fresh V2 upload must not mask an existing legacy Trash record.
        conn.execute(
            "DELETE FROM drive_entries_v2 WHERE entry_id='qa-v2-colliding-pending';
             INSERT INTO logical_channels VALUES (
                 'qa-other-namespace', 101, 'Synthetic', 'owner', 1, 1, 1
             );
             INSERT INTO logical_files VALUES (
                 'qa-other-file', 'qa-other-namespace', 202, 'old.bin',
                 1, NULL, 1, NULL, 1, 1, 'complete'
             );
             INSERT INTO drive_file_entries
                 (entry_id, file_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('qa-v2-colliding-pending','qa-other-file','old.bin',1,1,123);",
        )
        .unwrap();
        assert!(
            finalize_drive_v2_metadata(
                &conn,
                &pending.id,
                "66666666777777778888888899999999",
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            )
            .is_err(),
            "fresh V2 upload must not shadow an existing legacy Trash entry"
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert_eq!(state_row(&conn).unwrap(), before);
        let mut old = conn
            .prepare("SELECT file_id,trashed_at FROM drive_file_entries WHERE entry_id=?")
            .unwrap();
        old.bind((1, collided_id)).unwrap();
        assert_eq!(old.next().unwrap(), sqlite::State::Row);
        assert_eq!(old.read::<String, _>(0).unwrap(), "qa-other-file");
        assert_eq!(old.read::<i64, _>(1).unwrap(), 123);
    }

    #[test]
    fn new_legacy_upload_cannot_overwrite_existing_trashed_entry_with_colliding_id() {
        let conn = memory_db();
        let collided_id = "qa-legacy-colliding-pending";
        conn.execute(
            "INSERT INTO logical_channels VALUES(
                 'qa-collision-channel', 101, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files VALUES(
                 'qa-old-collision-file', 'qa-collision-channel', 101, 'old.bin',
                 4, NULL, 1, NULL, 1, 1, 'complete');
             INSERT INTO logical_files VALUES(
                 'qa-new-collision-file', 'qa-collision-channel', 102, 'new.bin',
                 7, NULL, 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('qa-legacy-colliding-pending', 'qa-old-collision-file',
                     NULL, 'old.bin', 1, 1, 123);",
        )
        .unwrap();
        let pending =
            create_drive_pending_file(&conn, collided_id, None, "new.bin", "private-staging")
                .unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        let before = state_row(&conn).unwrap();
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-new-collision-file",
                7,
                None,
                None,
            )
            .is_err(),
            "streamed upload must not overwrite an existing trashed file"
        );
        assert_eq!(state_row(&conn).unwrap(), before);
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert!(
            finalize_legacy_upload_metadata(&conn, &pending.id, "qa-new-collision-file").is_err(),
            "legacy one-shot upload must also preserve the trashed file"
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        let mut stmt = conn
            .prepare("SELECT file_id, trashed_at FROM drive_file_entries WHERE entry_id=?")
            .unwrap();
        stmt.bind((1, collided_id)).unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<String, _>(0).unwrap(), "qa-old-collision-file");
        assert_eq!(stmt.read::<i64, _>(1).unwrap(), 123);
        drop(stmt);

        // Neither legacy finalizer may mask a V2 tombstone with the same ID.
        conn.execute("DELETE FROM drive_file_entries WHERE entry_id='qa-legacy-colliding-pending'")
            .unwrap();
        insert_v2_test_file(
            &conn,
            collided_id,
            "12121212343434345656565678787878",
            None,
            "old.bin",
            202,
            9003,
        );
        conn.execute("UPDATE drive_entries_v2 SET trashed_at=123 WHERE entry_id='qa-legacy-colliding-pending'").unwrap();
        assert!(
            finalize_drive_streamed_pending(
                &conn,
                &pending.id,
                "qa-new-collision-file",
                7,
                None,
                None,
            )
            .is_err(),
            "fresh streamed legacy upload must not shadow a V2 Trash entry"
        );
        assert!(
            finalize_legacy_upload_metadata(&conn, &pending.id, "qa-new-collision-file").is_err(),
            "fresh one-shot legacy upload must not shadow a V2 Trash entry"
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        assert_eq!(state_row(&conn).unwrap(), before);
        let mut v2 = conn
            .prepare("SELECT object_id,trashed_at FROM drive_entries_v2 WHERE entry_id=?")
            .unwrap();
        v2.bind((1, collided_id)).unwrap();
        assert_eq!(v2.next().unwrap(), sqlite::State::Row);
        assert_eq!(
            v2.read::<String, _>(0).unwrap(),
            "12121212343434345656565678787878"
        );
        assert_eq!(v2.read::<i64, _>(1).unwrap(), 123);
    }

    #[test]
    fn v2_finalization_preserves_cow_pending_when_original_is_trashed_or_missing() {
        let conn = memory_db();
        let base_entry_id = "qa-v2-cow-original";
        let original_object_id = "12121212343434345656565678787878";
        let pending_id = "00112233445566778899aabbccddeeff";
        let replacement_object_id = "abcdef0123456789abcdef0123456789";
        let fingerprint = "abababababababababababababababababababababababababababababababab";
        insert_v2_test_file(
            &conn,
            base_entry_id,
            original_object_id,
            None,
            "old.bin",
            202,
            9001,
        );
        create_drive_pending_replacement(&conn, pending_id, base_entry_id, "owned-staging")
            .unwrap();
        mark_drive_pending_closed(&conn, pending_id).unwrap();
        assign_drive_pending_backing_channel(&conn, pending_id, 202).unwrap();
        upsert_drive_pending_chunk(
            &conn,
            pending_id,
            0,
            9002,
            1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        upsert_drive_pending_part(
            &conn,
            pending_id,
            0,
            0,
            1,
            202,
            9002,
            1,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        let revision_before = state_row(&conn).unwrap();
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=7 WHERE entry_id='qa-v2-cow-original'",
        )
        .unwrap();
        assert!(
            finalize_drive_v2_metadata(&conn, pending_id, replacement_object_id, fingerprint)
                .is_err(),
            "a trashed V2 base must not be resurrected by pending finalization",
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, pending_id).unwrap().is_some());
        assert!(drive_file_by_entry_id(&conn, base_entry_id)
            .unwrap()
            .is_none());
        conn.execute("DELETE FROM drive_entries_v2 WHERE entry_id='qa-v2-cow-original'")
            .unwrap();
        assert!(
            finalize_drive_v2_metadata(&conn, pending_id, replacement_object_id, fingerprint)
                .is_err(),
            "a deleted V2 base must not reappear from a stale pending editor",
        );
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, pending_id).unwrap().is_some());

        conn.execute(
            "INSERT INTO drive_entries_v2
             (entry_id, object_id, directory_id, display_name, created_at, updated_at)
             VALUES ('qa-v2-cow-original',
                     '12121212343434345656565678787878', NULL, 'old.bin', 1, 1)",
        )
        .unwrap();
        // Reintroduce a logically linked source while the earlier COW editor
        // is open. Finalization must recheck inside its transaction, or it
        // would publish just one alias with different bytes.
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id='1234567890abcdef1234567890abcdef'
             WHERE entry_id='qa-v2-cow-original'",
        )
        .unwrap();
        let error =
            finalize_drive_v2_metadata(&conn, pending_id, replacement_object_id, fingerprint)
                .unwrap_err();
        assert!(error.contains("shared-inode copy-on-write"), "{error}");
        assert_eq!(state_row(&conn).unwrap(), revision_before);
        assert!(drive_pending_by_id(&conn, pending_id).unwrap().is_some());
        conn.execute(
            "UPDATE drive_entries_v2
             SET inode_id=NULL WHERE entry_id='qa-v2-cow-original'",
        )
        .unwrap();
        let result =
            finalize_drive_v2_metadata(&conn, pending_id, replacement_object_id, fingerprint)
                .unwrap();
        assert_eq!(result.entry_id, base_entry_id);
        assert_eq!(result.object_id, replacement_object_id);
        assert!(drive_pending_by_id(&conn, pending_id).unwrap().is_none());
        assert_eq!(
            drive_file_by_entry_id(&conn, base_entry_id)
                .unwrap()
                .unwrap()
                .file_id,
            replacement_object_id,
        );
    }

    #[test]
    fn v2_finalization_does_not_reuse_a_different_crypto_identity() {
        let conn = memory_db();
        let existing_object = "99999999aaaaaaaa11111111bbbbbbbb";
        let pending_id = "11223344556677889900aabbccddeeff";
        let proposed_object = "abcdef0123456789abcdef0123456789";
        let fingerprint = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        insert_v2_test_file(
            &conn,
            "old-crypto-entry",
            existing_object,
            None,
            "old.enc",
            202,
            8001,
        );
        conn.execute(
            "UPDATE drive_objects
             SET encryption_version=1, crypto_id='old-key'
             WHERE object_id='99999999aaaaaaaa11111111bbbbbbbb'",
        )
        .unwrap();

        let pending = create_drive_pending_file(
            &conn,
            pending_id,
            None,
            "new.enc",
            "/tmp/terarelay-crypto-identity-fixture",
        )
        .unwrap();
        conn.execute(
            "UPDATE drive_pending_files
             SET encryption_version=1, crypto_id='new-key'
             WHERE id='11223344556677889900aabbccddeeff'",
        )
        .unwrap();
        update_drive_pending_size(&conn, &pending.id, 1).unwrap();
        mark_drive_pending_closed(&conn, &pending.id).unwrap();
        assign_drive_pending_backing_channel(&conn, &pending.id, 202).unwrap();
        upsert_drive_pending_chunk(
            &conn,
            &pending.id,
            0,
            8002,
            1,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .unwrap();
        upsert_drive_pending_part(
            &conn,
            &pending.id,
            0,
            0,
            1,
            202,
            8002,
            1,
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        )
        .unwrap();

        conn.execute(
            "UPDATE drive_objects SET crypto_id=X'FF'
             WHERE object_id='99999999aaaaaaaa11111111bbbbbbbb'",
        )
        .unwrap();
        assert!(
            finalize_drive_v2_metadata(&conn, &pending.id, proposed_object, fingerprint).is_err(),
            "malformed stored crypto identity must not be treated as an absent identity",
        );
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_some());
        conn.execute(
            "UPDATE drive_objects SET crypto_id='old-key'
             WHERE object_id='99999999aaaaaaaa11111111bbbbbbbb'",
        )
        .unwrap();

        let result =
            finalize_drive_v2_metadata(&conn, &pending.id, proposed_object, fingerprint).unwrap();
        assert!(
            !result.reused_existing_object,
            "ciphertext from a different encryption identity cannot be reused"
        );
        assert_eq!(result.object_id, proposed_object);
        assert!(result.cleanup_parts.is_empty());
        assert!(drive_pending_by_id(&conn, &pending.id).unwrap().is_none());
        let stored = drive_v2_file_by_entry(&conn, pending_id).unwrap().unwrap();
        assert_eq!(stored.crypto_id.as_deref(), Some("new-key"));
        assert_eq!(stored.file_id, proposed_object);
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
    fn imported_layout_skips_large_existing_namespace_without_rewriting_metadata() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('already-mapped', 404, 'Mapped', 'owner', 1, 1, 1);",
        )
        .unwrap();
        // A modest fixture exercises the production importer without using
        // timing assertions that would vary wildly across CI machines.
        for index in 0..256u32 {
            conn.execute(format!(
                "INSERT INTO logical_files
                 (file_id,logical_channel_id,first_message_id,display_name,total_size,
                  mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
                 VALUES ('id-{index:04}','already-mapped',{index},'f{index:04}.bin',
                         1,NULL,1,NULL,1,1,'complete');"
            ))
            .unwrap();
        }
        assert!(ensure_imported_layout(&conn).unwrap());
        let revision = state_row(&conn).unwrap();
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 256);
        assert!(!ensure_imported_layout(&conn).unwrap());
        assert_eq!(state_row(&conn).unwrap(), revision);
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 256);
        // A later upload still gets imported without revisiting or
        // rewriting existing entries.
        conn.execute(
            "INSERT INTO logical_files
             (file_id,logical_channel_id,first_message_id,display_name,total_size,
              mime_type,chunk_count,whole_sha256,storage_version,created_at,status)
             VALUES ('new-id','already-mapped',900,'new.bin',3,NULL,1,NULL,1,2,'complete');",
        )
        .unwrap();
        assert!(ensure_imported_layout(&conn).unwrap());
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 257);
    }

    #[test]
    fn imported_channel_folders_skip_pending_files_and_occupied_fallback_names() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('mychannelz', 401, 'Media', 'owner', 1, 1, 1);
             INSERT INTO drive_pending_files
             (id,directory_id,display_name,staging_path,size,backing_channel_id,created_at,updated_at)
             VALUES ('occupied-root',NULL,'media','/tmp/not-read',0,401,1,1),
                    ('occupied-fallback',NULL,'Media mychanne','/tmp/not-read',0,401,1,1);",
        ).unwrap();
        assert!(ensure_imported_layout(&conn).unwrap());
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.directories.len(), 1);
        assert_eq!(snapshot.directories[0].name, "Media mychanne-2");
        assert_eq!(snapshot.directories[0].id, "channel-mychannelz");
        assert!(!ensure_imported_layout(&conn).unwrap());
    }

    #[test]
    fn imported_file_names_avoid_directory_pending_and_portable_collisions() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('filechannel', 402, 'Files', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('abcdef123456abcdef123456abcdef12', 'filechannel', 81, 'Report.txt', 5,
                     'text/plain', 1, NULL, 1, 2, 'complete');
             INSERT INTO drive_directories
             (id,parent_id,name,backing_logical_channel_id,created_at,updated_at,trashed_at)
             VALUES ('channel-filechannel',NULL,'Files','filechannel',1,1,NULL),
                    ('occupied-directory','channel-filechannel','REPORT.TXT',NULL,1,1,NULL);
             INSERT INTO drive_pending_files
             (id,directory_id,display_name,staging_path,size,backing_channel_id,created_at,updated_at)
             VALUES ('occupied-file-suffix','channel-filechannel','Report (abcdef).txt',
                     '/tmp/not-read',0,402,1,1);",
        ).unwrap();
        assert!(ensure_imported_layout(&conn).unwrap());
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].display_name, "Report (abcdef-2).txt");
        assert!(!ensure_imported_layout(&conn).unwrap());
    }

    #[test]
    fn imported_collision_suffix_never_breaks_utf8_or_name_length_limit() {
        let conn = memory_db();
        let requested = format!("{}🌠.txt", "a".repeat(247));
        let chosen = available_imported_name(&conn, None, &requested, "abc00012", true).unwrap();
        assert_eq!(chosen, requested);
        conn.execute(
            "INSERT INTO drive_directories
             (id,parent_id,name,created_at,updated_at,trashed_at)
             VALUES ('already',NULL,'fallback',1,1,NULL);",
        )
        .unwrap();
        let fallback = available_imported_name(&conn, None, "fallback", "abc00012", false).unwrap();
        assert_eq!(fallback, "fallback abc00012");
        // Force a collision on a valid 254-byte Unicode name. Added suffix
        // must truncate on a character boundary rather than slicing UTF-8.
        let long_name = format!("{}🌠", "x".repeat(250));
        let mut insert = conn
            .prepare("INSERT INTO drive_directories (id,parent_id,name,created_at,updated_at,trashed_at) VALUES (?1,NULL,?2,1,1,NULL)")
            .unwrap();
        insert.bind((1, "long-existing")).unwrap();
        insert.bind((2, long_name.as_str())).unwrap();
        insert.next().unwrap();
        let next = available_imported_name(&conn, None, &long_name, "abc00012", false).unwrap();
        assert!(next.len() <= 255);
        assert!(next.ends_with(" abc00012"));
        assert!(validate_drive_name(&next).is_ok());
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
    fn scoped_upload_import_notifies_new_channel_folder_without_scanning_unrelated_files() {
        use crate::commands::logical_files::{LogicalFileManifestV1, ManifestChunkV1};
        let conn = memory_db();
        conn.execute(
            "CREATE TABLE logical_file_chunks (
                file_id TEXT NOT NULL, chunk_index INTEGER NOT NULL,
                message_id INTEGER NOT NULL, chunk_size INTEGER NOT NULL, sha256 TEXT,
                PRIMARY KEY(file_id, chunk_index)
             );",
        )
        .unwrap();
        // The mount already exists, before either channel is added.
        ensure_imported_layout(&conn).unwrap();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('new-channel', 301, 'New Channel', 'owner', 1, 10, 10),
                    ('unrelated-channel', 302, 'Unrelated', 'owner', 1, 11, 11);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('unrelated-old-file', 'unrelated-channel', 5, 'unrelated.txt', 1,
                     'text/plain', 1, NULL, 1, 12, 'complete');",
        )
        .unwrap();
        let upload = LogicalFileManifestV1 {
            schema_version: crate::commands::logical_files::MANIFEST_SCHEMA_VERSION,
            file_id: "1234567890abcdef1234567890abcdef".into(),
            logical_channel_id: "new-channel".into(),
            original_name: "uploaded.txt".into(),
            total_size: 7,
            mime_type: Some("text/plain".into()),
            created_at: 13,
            whole_sha256: None,
            chunks: vec![ManifestChunkV1 {
                index: 1,
                message_id: 3012,
                size: 7,
                sha256: None,
            }],
        };
        assert_eq!(
            publish_completed_upload_to_drive(&conn, &upload).unwrap(),
            Some((
                "channel-new-channel".into(),
                "uploaded.txt".into(),
                Some("New Channel".into()),
                "import-1234567890abcdef1234567890abcdef".into()
            ))
        );
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.directories.len(), 1);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].display_name, "uploaded.txt");
        assert!(!snapshot
            .files
            .iter()
            .any(|file| file.file_id == "unrelated-old-file"));
        assert_eq!(
            publish_completed_upload_to_drive(&conn, &upload).unwrap(),
            Some((
                "channel-new-channel".into(),
                "uploaded.txt".into(),
                None,
                "import-1234567890abcdef1234567890abcdef".into()
            ))
        );
        // Recovery's full importer still discovers legacy/unmapped files.
        assert!(ensure_imported_layout(&conn).unwrap());
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.directories.len(), 2);
        assert_eq!(snapshot.files.len(), 2);
    }

    #[test]
    fn finished_upload_appears_in_existing_drive_namespace_without_remount() {
        use crate::commands::logical_files::{LogicalFileManifestV1, ManifestChunkV1};

        let conn = memory_db();
        conn.execute(
            "CREATE TABLE logical_file_chunks (
                file_id TEXT NOT NULL, chunk_index INTEGER NOT NULL, message_id INTEGER NOT NULL,
                chunk_size INTEGER NOT NULL, sha256 TEXT,
                PRIMARY KEY(file_id, chunk_index)
             );
             INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('live-upload-channel', 812, 'Live Uploads', 'owner', 1, 1, 1);",
        )
        .unwrap();

        // Represents the mounted Drive before the upload, with the folder open.
        assert!(ensure_imported_layout(&conn).unwrap());
        assert!(drive_snapshot(&conn).unwrap().files.is_empty());
        let before = state_row(&conn).unwrap();
        let manifest = LogicalFileManifestV1 {
            schema_version: crate::commands::logical_files::MANIFEST_SCHEMA_VERSION,
            file_id: "0123456789abcdef0123456789abcdef".into(),
            logical_channel_id: "live-upload-channel".into(),
            original_name: "completed.txt".into(),
            total_size: 5,
            mime_type: Some("text/plain".into()),
            created_at: 1_790_000_000,
            whole_sha256: None,
            chunks: vec![ManifestChunkV1 {
                index: 1,
                message_id: 8123,
                size: 5,
                sha256: None,
            }],
        };
        let changed = publish_completed_upload_to_drive(&conn, &manifest).unwrap();
        assert_eq!(
            changed,
            Some((
                "channel-live-upload-channel".into(),
                "completed.txt".into(),
                None,
                "import-0123456789abcdef0123456789abcdef".into()
            ))
        );
        let snapshot = drive_snapshot(&conn).unwrap();
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].display_name, "completed.txt");
        assert_eq!(snapshot.files[0].total_size, 5);
        assert!(state_row(&conn).unwrap().0 > before.0);
        let revision = state_row(&conn).unwrap();
        assert_eq!(
            publish_completed_upload_to_drive(&conn, &manifest).unwrap(),
            changed
        );
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 1);
        assert_eq!(
            state_row(&conn).unwrap(),
            revision,
            "retry cannot duplicate the entry"
        );

        // Replacing the remote content under the SAME logical file identity
        // must keep its inode/entry identity while exposing the new attributes.
        // The upload dispatcher uses the returned entry ID for kernel
        // inval_inode in addition to invalidating its parent dentry.
        let mut replacement = manifest.clone();
        replacement.total_size = 6;
        replacement.chunks[0].size = 6;
        replacement.chunks[0].message_id = 8124;
        assert_eq!(
            publish_completed_upload_to_drive(&conn, &replacement).unwrap(),
            changed
        );
        let replaced = drive_snapshot(&conn).unwrap();
        assert_eq!(replaced.files.len(), 1);
        assert_eq!(replaced.files[0].total_size, 6);
        assert_eq!(
            replaced.files[0].entry_id,
            "import-0123456789abcdef0123456789abcdef"
        );

        // The manifest and Drive entry must both roll back if publication of
        // the namespace revision fails, leaving a retryable upload.
        let mut second = manifest;
        second.file_id = "fedcba9876543210fedcba9876543210".into();
        second.original_name = "second.txt".into();
        conn.execute(
            "CREATE TRIGGER qa_reject_live_import_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa revision unavailable'); END;",
        )
        .unwrap();
        let error = publish_completed_upload_to_drive(&conn, &second).unwrap_err();
        assert!(error.contains("qa revision unavailable"), "{error}");
        assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 1);
        let mut query = conn
            .prepare("SELECT COUNT(*) FROM logical_files WHERE file_id=?")
            .unwrap();
        query.bind((1, second.file_id.as_str())).unwrap();
        assert_eq!(query.next().unwrap(), sqlite::State::Row);
        assert_eq!(query.read::<i64, _>(0).unwrap(), 0);
        drop(query);

        // A channel directory that was trashed concurrently must not cause
        // a successful upload to be falsely acknowledged as visible.
        conn.execute(
            "DROP TRIGGER qa_reject_live_import_revision;
             UPDATE drive_directories SET trashed_at=42
             WHERE id='channel-live-upload-channel';",
        )
        .unwrap();
        let error = publish_completed_upload_to_drive(&conn, &second).unwrap_err();
        assert!(
            error.contains("missing or trashed Drive directory"),
            "{error}"
        );
        let mut missing = conn
            .prepare("SELECT COUNT(*) FROM logical_files WHERE file_id=?1")
            .unwrap();
        missing.bind((1, second.file_id.as_str())).unwrap();
        assert_eq!(missing.next().unwrap(), sqlite::State::Row);
        assert_eq!(missing.read::<i64, _>(0).unwrap(), 0);
    }

    #[test]
    fn legacy_import_does_not_reserve_names_for_orphan_v2_metadata() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('import-target', 'abc', 99, 'Dune.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete');
             INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('orphan-entry', 'missing-object', 'channel-abc', 'Dune.mkv', 2, 2, NULL);",
        )
        .unwrap();

        assert!(ensure_imported_layout(&conn).unwrap());

        let snapshot = drive_snapshot(&conn).unwrap();
        let imported = snapshot
            .files
            .iter()
            .find(|file| file.file_id == "import-target")
            .expect("unmapped legacy file must be imported");
        assert_eq!(
            imported.display_name, "Dune.mkv",
            "orphan V2 metadata is not a visible namespace collision during legacy import"
        );
        assert_eq!(imported.storage_version, 1);
    }

    #[test]
    fn legacy_import_does_not_reserve_names_for_trashed_v2_legacy_shadows() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('old-shadow-file', 'abc', 98, 'Dune.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 1, 'complete'),
                    ('import-target', 'abc', 99, 'Dune.mkv', 124,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('import-old-shadow-file', 'old-shadow-file', 'channel-abc',
                     'Dune.mkv', 1, 1, NULL, 0, 123, NULL, NULL);",
        )
        .unwrap();
        insert_v2_test_file(
            &conn,
            "import-old-shadow-file",
            "trashed-shadow-object",
            Some("channel-abc"),
            "Dune.mkv",
            101,
            98,
        );
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=3, updated_at=3
             WHERE entry_id='import-old-shadow-file';",
        )
        .unwrap();

        assert!(ensure_imported_layout(&conn).unwrap());

        let snapshot = drive_snapshot(&conn).unwrap();
        let imported = snapshot
            .files
            .iter()
            .find(|file| file.file_id == "import-target")
            .expect("unmapped legacy file must be imported");
        assert_eq!(
            imported.display_name, "Dune.mkv",
            "a real V2 tombstone must keep its historical legacy alias from reserving a visible name"
        );
        assert_eq!(imported.storage_version, 1);
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| file.entry_id != "import-old-shadow-file"),
            "the historical legacy shadow itself must stay hidden"
        );
    }

    #[test]
    fn legacy_import_preserves_name_reservation_for_active_v2_legacy_aliases() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('abc', 101, 'Movies', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('old-shadow-file', 'abc', 98, 'Dune.mkv', 123,
                     'video/x-matroska', 1, NULL, 1, 1, 'complete'),
                    ('import-target', 'abc', 99, 'Dune.mkv', 124,
                     'video/x-matroska', 1, NULL, 1, 2, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('import-old-shadow-file', 'old-shadow-file', 'channel-abc',
                     'Dune.mkv', 1, 1, NULL, 0, 123, NULL, NULL);",
        )
        .unwrap();
        insert_v2_test_file(
            &conn,
            "import-old-shadow-file",
            "active-shadow-object",
            Some("channel-abc"),
            "Latest.mkv",
            101,
            98,
        );

        assert!(ensure_imported_layout(&conn).unwrap());

        let snapshot = drive_snapshot(&conn).unwrap();
        let imported = snapshot
            .files
            .iter()
            .find(|file| file.file_id == "import-target")
            .expect("unmapped legacy file must be imported");
        assert_eq!(
            imported.display_name, "Dune (import).mkv",
            "active dual-row legacy aliases intentionally remain namespace-visible for compatibility"
        );
        let legacy_lookup =
            crate::drive_lookup::child_entry(&conn, Some("channel-abc"), "Dune.mkv")
                .unwrap()
                .expect("the active dual-row legacy alias remains available");
        let crate::drive_lookup::DriveChildEntry::File(file) = legacy_lookup else {
            panic!("the active legacy alias must resolve as a file");
        };
        assert_eq!(file.storage_version, 2);
        assert_eq!(file.display_name, "Latest.mkv");
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
    fn deduplication_size_cannot_wrap_into_negative_sqlite_plaintext_size() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-dedup-channel', 981, 'QA', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-dedup-file', 'qa-dedup-channel', 71, 'qa-dedup.bin', 1,
                     'application/octet-stream', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
             (entry_id, file_id, display_name, created_at, updated_at,
              encryption_version, plaintext_size, content_fingerprint)
             VALUES ('qa-dedup-entry', 'qa-dedup-file', 'qa-dedup.bin', 1, 2,
                     0, -1, 'qa-dedup-fingerprint');",
        )
        .unwrap();
        assert!(
            find_drive_content_match(&conn, "qa-dedup-fingerprint", u64::MAX, 0)
                .expect_err("too-large unsigned sizes must not match negative stored values")
                .contains("SQLite limits")
        );
        assert!(
            find_drive_content_match(&conn, "qa-dedup-fingerprint", (i64::MAX as u64) + 1, 0)
                .is_err()
        );
        assert!(
            find_drive_content_match(&conn, "qa-dedup-fingerprint", 1, 0)
                .unwrap()
                .is_none()
        );
        conn.execute(
            "UPDATE drive_file_entries SET plaintext_size=11
             WHERE entry_id='qa-dedup-entry'",
        )
        .unwrap();
        assert_eq!(
            find_drive_content_match(&conn, "qa-dedup-fingerprint", 11, 0)
                .unwrap()
                .unwrap()
                .file_id,
            "qa-dedup-file"
        );
        assert!(
            find_drive_content_match(&conn, "qa-dedup-fingerprint", 11, 2)
                .expect_err("unknown encryption modes cannot be accepted for deduplication")
                .contains("encryption version")
        );
    }

    #[test]
    fn deduplication_rejects_corrupt_stored_crypto_identity_without_silent_fallback() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('qa-crypto-channel', 982, 'QA', 'owner', 1, 1, 1);
             INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('qa-crypto-file', 'qa-crypto-channel', 72, 'qa-crypto.bin', 10,
                     'application/octet-stream', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
             (entry_id, file_id, display_name, created_at, updated_at,
              encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('qa-crypto-entry', 'qa-crypto-file', 'qa-crypto.bin', 1, 2,
                     1, 10, X'FF', 'qa-crypto-fingerprint');",
        )
        .unwrap();
        let err = find_drive_content_match(&conn, "qa-crypto-fingerprint", 10, 1)
            .expect_err("a BLOB crypto identity must not be converted into NULL");
        assert!(err.contains("crypto_id"), "{err}");
        conn.execute(
            "UPDATE drive_file_entries SET crypto_id='qa-valid-crypto'
             WHERE entry_id='qa-crypto-entry'",
        )
        .unwrap();
        let recovered = find_drive_content_match(&conn, "qa-crypto-fingerprint", 10, 1)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.file_id, "qa-crypto-file");
        assert_eq!(recovered.crypto_id.as_deref(), Some("qa-valid-crypto"));

        // Genuine SQL NULL is distinct from malformed metadata; downstream
        // encrypted deduplication explicitly rejects a missing crypto identity.
        conn.execute(
            "UPDATE drive_file_entries SET crypto_id=NULL
             WHERE entry_id='qa-crypto-entry'",
        )
        .unwrap();
        assert!(
            find_drive_content_match(&conn, "qa-crypto-fingerprint", 10, 1)
                .unwrap()
                .unwrap()
                .crypto_id
                .is_none()
        );
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
    fn remote_manifest_rejects_negative_record_timestamps_before_import() {
        let conn = memory_db();
        crate::drive_storage::init_drive_storage_schema(&conn).unwrap();
        crate::drive_storage::register_storage_channel(
            &conn,
            1,
            202,
            "TeraRelay Drive Storage",
            "active",
            None,
        )
        .unwrap();
        insert_v2_test_file(
            &conn,
            "timestamp-v2-entry",
            "88888888777777776666666655555555",
            None,
            "v2-timestamp.txt",
            202,
            9101,
        );
        let original = build_manifest(&conn).unwrap();
        let mut valid = original.clone();
        valid.directories.push(DriveDirectoryRecord {
            id: "timestamp-dir".into(),
            parent_id: None,
            name: "timestamp-dir".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        valid.entries.push(DriveEntryRecord {
            entry_id: "timestamp-v1-entry".into(),
            file_id: "timestamp-legacy-file".into(),
            directory_id: None,
            display_name: "legacy-timestamp.txt".into(),
            created_at: 1,
            updated_at: 1,
            encryption_version: 0,
            plaintext_size: Some(1),
            crypto_id: None,
            content_fingerprint: None,
            trashed_at: None,
        });
        validate_manifest(&valid).unwrap();
        type TimestampMutation = fn(&mut DriveManifestV1);
        let cases: [(&str, TimestampMutation); 12] = [
            ("folder creation", |m| m.directories[0].created_at = -1),
            ("folder update", |m| m.directories[0].updated_at = -1),
            ("folder deletion", |m| {
                m.directories[0].trashed_at = Some(-1)
            }),
            ("legacy creation", |m| m.entries[0].created_at = -1),
            ("legacy update", |m| m.entries[0].updated_at = -1),
            ("legacy deletion", |m| m.entries[0].trashed_at = Some(-1)),
            ("storage creation", |m| {
                m.storage_channels[0].created_at = -1
            }),
            ("storage update", |m| m.storage_channels[0].updated_at = -1),
            ("object creation", |m| m.objects[0].created_at = -1),
            ("V2 creation", |m| m.v2_entries[0].created_at = -1),
            ("V2 update", |m| m.v2_entries[0].updated_at = -1),
            ("V2 deletion", |m| m.v2_entries[0].trashed_at = Some(-1)),
        ];
        for (field, mutate) in cases {
            let mut inbound = valid.clone();
            mutate(&mut inbound);
            assert!(
                validate_manifest(&inbound).is_err(),
                "{field}: negative remote timestamp must not pass validation"
            );
            assert!(
                apply_manifest(&conn, &inbound).is_err(),
                "{field}: corrupt remote timestamps must not replace local metadata"
            );
            assert_eq!(
                build_manifest(&conn).unwrap(),
                original,
                "{field}: failed import must leave local Drive unchanged"
            );
        }
    }

    #[test]
    fn remote_encryption_metadata_rejects_excessive_unlock_resources() {
        let conn = memory_db();
        let mut manifest = build_manifest(&conn).unwrap();
        manifest.crypto = Some(crate::drive_crypto::DriveCryptoEnvelope {
            version: crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            updated_at: 10,
            salt_b64: B64.encode([1u8; 16]),
            nonce_b64: B64.encode([2u8; 24]),
            wrapped_master_key_b64: B64.encode([3u8; 48]),
            key_check_b64: Some(B64.encode([4u8; 32])),
            memory_kib: 64 * 1024,
            iterations: 3,
            parallelism: 1,
        });
        validate_manifest(&manifest).unwrap();

        for (field, memory, iterations, parallelism) in [
            ("memory", 1024 * 1024, 3, 1),
            ("iterations", 64 * 1024, 500, 1),
            ("parallelism", 64 * 1024, 3, 128),
            ("zero iterations", 64 * 1024, 0, 1),
        ] {
            let envelope = manifest.crypto.as_mut().unwrap();
            envelope.memory_kib = memory;
            envelope.iterations = iterations;
            envelope.parallelism = parallelism;
            assert!(
                validate_manifest(&manifest).is_err(),
                "{field}: remote encryption metadata must not force excessive KDF resources"
            );
        }

        let valid_envelope = manifest.crypto.as_ref().unwrap().clone();
        let mut valid_envelope = valid_envelope;
        valid_envelope.memory_kib = 64 * 1024;
        valid_envelope.iterations = 3;
        valid_envelope.parallelism = 1;
        manifest.crypto = Some(valid_envelope.clone());
        for (field, change) in [
            ("salt", "salt"),
            ("nonce", "nonce"),
            ("wrapped key", "wrapped"),
            ("verifier", "check"),
            ("timestamp", "timestamp"),
        ] {
            let envelope = manifest.crypto.as_mut().unwrap();
            *envelope = valid_envelope.clone();
            match change {
                "salt" => envelope.salt_b64 = B64.encode([1u8; 15]),
                "nonce" => envelope.nonce_b64 = "not-base64!".into(),
                "wrapped" => envelope.wrapped_master_key_b64 = B64.encode([3u8; 40]),
                "check" => envelope.key_check_b64 = Some(B64.encode([4u8; 31])),
                "timestamp" => envelope.updated_at = -1,
                _ => unreachable!(),
            }
            assert!(
                validate_manifest(&manifest).is_err(),
                "{field}: remote encryption envelope must be structurally valid"
            );
            assert!(
                apply_manifest(&conn, &manifest).is_err(),
                "{field}: invalid remote key metadata must not replace local state"
            );
        }
        manifest.crypto = Some(valid_envelope.clone());
        validate_manifest(&manifest).unwrap();
        manifest.crypto.as_mut().unwrap().key_check_b64 = None;
        validate_manifest(&manifest).unwrap();
    }

    #[test]
    fn concurrent_encryption_keys_never_silently_replace_one_another() {
        let envelope = crate::drive_crypto::DriveCryptoEnvelope {
            version: crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            updated_at: 10,
            salt_b64: B64.encode([1u8; 16]),
            nonce_b64: B64.encode([2u8; 24]),
            wrapped_master_key_b64: B64.encode([3u8; 48]),
            key_check_b64: Some(B64.encode([4u8; 32])),
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        };
        let first = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
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
        second.crypto.as_mut().unwrap().key_check_b64 = Some(B64.encode([5u8; 32]));
        assert!(merge_drive_manifests(&first, &second)
            .unwrap_err()
            .contains("Conflicting TeraRelay Drive encryption keys"));

        // Changing the passphrase only rotates the wrapper, not the
        // underlying master key. This should still merge normally.
        second.crypto.as_mut().unwrap().key_check_b64 = Some(B64.encode([4u8; 32]));
        second.crypto.as_mut().unwrap().wrapped_master_key_b64 = B64.encode([6u8; 48]);
        let merged = merge_drive_manifests(&first, &second).unwrap();
        assert_eq!(
            merged.crypto.unwrap().wrapped_master_key_b64,
            B64.encode([6u8; 48])
        );

        // Older metadata without a verifier cannot prove two independently
        // wrapped keys represent the same master key. Reject ambiguity.
        let mut legacy = first.clone();
        legacy.crypto.as_mut().unwrap().key_check_b64 = None;
        assert!(merge_drive_manifests(&legacy, &second).is_err());

        // A pre-verifier envelope can be compared only if its entire
        // passphrase wrapper is byte-for-byte compatible. The ciphertext
        // alone is insufficient: a different salt, nonce or Argon2 setting
        // would make the same wrapped key impossible to decrypt.
        for change in ["salt", "nonce", "memory", "iterations", "parallelism"] {
            let mut conflicting = legacy.clone();
            let wrapper = conflicting.crypto.as_mut().unwrap();
            wrapper.updated_at += 1;
            match change {
                "salt" => wrapper.salt_b64 = B64.encode([8u8; 16]),
                "nonce" => wrapper.nonce_b64 = B64.encode([9u8; 24]),
                "memory" => wrapper.memory_kib = 32 * 1024,
                "iterations" => wrapper.iterations = 4,
                "parallelism" => wrapper.parallelism = 2,
                _ => unreachable!(),
            }
            validate_manifest(&conflicting).unwrap();
            assert!(
                merge_drive_manifests(&legacy, &conflicting).is_err(),
                "{change}: incompatible legacy wrappers cannot silently replace encryption"
            );
            assert!(
                merge_drive_manifests(&conflicting, &legacy).is_err(),
                "{change}: merge order must not bypass wrapper compatibility"
            );
        }
        let mut same_wrapper = legacy.clone();
        same_wrapper.crypto.as_mut().unwrap().updated_at += 1;
        assert!(merge_drive_manifests(&legacy, &same_wrapper).is_ok());
    }

    #[test]
    fn remote_manifest_cannot_erase_or_replace_existing_encryption_key() {
        let conn = memory_db();
        let local = crate::drive_crypto::DriveCryptoEnvelope {
            version: crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            updated_at: 10,
            salt_b64: B64.encode([1u8; 16]),
            nonce_b64: B64.encode([2u8; 24]),
            wrapped_master_key_b64: B64.encode([3u8; 48]),
            key_check_b64: Some(B64.encode([4u8; 32])),
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        };
        crate::drive_crypto::save_crypto_envelope(&conn, Some(&local)).unwrap();
        let mut incoming = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
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
        incompatible.key_check_b64 = Some(B64.encode([5u8; 32]));
        incoming.crypto = Some(incompatible);
        assert!(apply_manifest(&conn, &incoming)
            .unwrap_err()
            .contains("does not match"));
        assert_eq!(
            crate::drive_crypto::load_crypto_envelope(&conn).unwrap(),
            Some(local.clone())
        );

        let mut rotated_passphrase = local.clone();
        rotated_passphrase.wrapped_master_key_b64 = B64.encode([6u8; 48]);
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

        // A remotely supplied part size must be valid before it is allowed
        // through the validator or a cross-device manifest merge. Otherwise
        // a malformed merge can only fail after a later database import.
        for invalid_size in [manifest.parts[0].ciphertext_size + 1, u64::MAX, 0] {
            let mut invalid = manifest.clone();
            invalid.parts[0].ciphertext_size = invalid_size;
            assert!(
                validate_manifest(&invalid).is_err(),
                "malformed remote part size {invalid_size} passed manifest validation"
            );
            assert!(apply_manifest(&target, &invalid).is_err());
            assert!(
                drive_file_by_entry_id(&target, "synced-v2-entry")
                    .unwrap()
                    .is_some(),
                "invalid remote metadata must not erase a previously synced file"
            );
        }
    }

    #[test]
    fn malformed_storage_retirement_reason_must_not_be_lost_during_manifest_export() {
        let conn = memory_db();
        crate::drive_storage::init_drive_storage_schema(&conn).unwrap();
        crate::drive_storage::register_storage_channel(
            &conn,
            1,
            202,
            "TeraRelay Drive Storage",
            "retired",
            Some("channel retired"),
        )
        .unwrap();
        let original = build_manifest(&conn).unwrap();
        assert_eq!(original.storage_channels.len(), 1);
        assert_eq!(
            original.storage_channels[0].retirement_reason.as_deref(),
            Some("channel retired")
        );

        conn.execute("UPDATE drive_storage_channels SET retirement_reason=X'FF' WHERE backing_channel_id=202").unwrap();
        assert!(
            build_manifest(&conn).is_err(),
            "invalid BLOB must not silently erase storage channel retirement history"
        );

        conn.execute("UPDATE drive_storage_channels SET retirement_reason='channel retired' WHERE backing_channel_id=202").unwrap();
        assert_eq!(
            build_manifest(&conn).unwrap().storage_channels[0]
                .retirement_reason
                .as_deref(),
            Some("channel retired")
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
    fn manifest_merge_resolves_legacy_to_v2_migration_without_losing_tombstones() {
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
            "migration-shared-entry",
            "aabbccddaabbccddaabbccddaabbccdd",
            None,
            "migrated.bin",
            202,
            9301,
        );
        let mut newer = build_manifest(&source).unwrap();
        newer.v2_entries[0].updated_at = 30;

        let mut older = newer.clone();
        older.objects.clear();
        older.parts.clear();
        older.v2_entries.clear();
        older.entries.push(DriveEntryRecord {
            entry_id: "migration-shared-entry".into(),
            file_id: "original-logical-file".into(),
            directory_id: None,
            display_name: "migrated.bin".into(),
            created_at: 1,
            updated_at: 10,
            encryption_version: 0,
            plaintext_size: Some(1),
            crypto_id: None,
            content_fingerprint: None,
            trashed_at: None,
        });
        validate_manifest(&older).unwrap();
        validate_manifest(&newer).unwrap();

        for (left, right) in [(&older, &newer), (&newer, &older)] {
            let merged = merge_drive_manifests(left, right)
                .expect("a legitimate V2 migration must merge with a stale V1 device");
            assert!(merged.entries.is_empty());
            assert_eq!(merged.v2_entries.len(), 1);
            assert_eq!(
                merged.v2_entries[0].object_id,
                newer.v2_entries[0].object_id
            );
            assert_eq!(merged.objects.len(), 1);
            assert_eq!(merged.parts.len(), 1);
        }

        let mut deleted = older.clone();
        deleted.entries[0].updated_at = 40;
        deleted.entries[0].trashed_at = Some(40);
        let merged = merge_drive_manifests(&deleted, &newer)
            .expect("a newer legacy deletion must not be discarded by V2 migration");
        assert!(merged.v2_entries.is_empty());
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(merged.entries[0].trashed_at, Some(40));

        deleted.entries[0].updated_at = 30;
        deleted.entries[0].trashed_at = Some(30);
        let merged = merge_drive_manifests(&newer, &deleted)
            .expect("a tied deletion must not resurrect a V2 file");
        assert!(merged.v2_entries.is_empty());
        assert_eq!(merged.entries[0].trashed_at, Some(30));
    }

    #[test]
    fn equal_timestamp_deletions_win_over_active_directory_and_file_versions() {
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
            "tie-v2-entry",
            "12345678123456781234567812345678",
            None,
            "v2.bin",
            202,
            9401,
        );
        let mut active = build_manifest(&source).unwrap();
        active.directories.push(DriveDirectoryRecord {
            id: "tie-folder".into(),
            parent_id: None,
            name: "folder".into(),
            backing_logical_channel_id: None,
            created_at: 10,
            updated_at: 10,
            trashed_at: None,
        });
        active.entries.push(DriveEntryRecord {
            entry_id: "tie-v1-entry".into(),
            file_id: "old-logical-file".into(),
            directory_id: None,
            display_name: "v1.bin".into(),
            created_at: 10,
            updated_at: 10,
            encryption_version: 0,
            plaintext_size: Some(1),
            crypto_id: None,
            content_fingerprint: None,
            trashed_at: None,
        });
        active.v2_entries[0].updated_at = 10;

        let mut deleted = active.clone();
        deleted.revision += 1;
        deleted.directories[0].trashed_at = Some(10);
        deleted.entries[0].trashed_at = Some(10);
        deleted.v2_entries[0].trashed_at = Some(10);
        for (left, right) in [(&active, &deleted), (&deleted, &active)] {
            let merged = merge_drive_manifests(left, right)
                .expect("equal-timestamp edits and deletions must converge");
            assert_eq!(
                merged.directories[0].trashed_at,
                Some(10),
                "a stale directory version must not restore a deleted folder"
            );
            assert_eq!(
                merged.entries[0].trashed_at,
                Some(10),
                "a stale legacy version must not restore a deleted file"
            );
            assert_eq!(
                merged.v2_entries[0].trashed_at,
                Some(10),
                "a stale V2 version must not restore a deleted file"
            );
        }
    }

    #[test]
    fn provider_projection_checkpoint_round_trips_atomically_without_dirtying_drive_state() {
        use crate::drive_fs_core::{DriveNodeId, DriveNodeKind, DriveProjectionRecord};

        let conn = memory_db();
        let records = vec![DriveProjectionRecord {
            id: DriveNodeId::File("entry-a".to_string()),
            path: vec!["Folder".to_string(), "File.txt".to_string()],
            kind: DriveNodeKind::File,
            content_version: "object-a".to_string(),
            size: 42,
            updated_at_ms: 123,
        }];
        assert!(load_drive_provider_projection(&conn, "windows-cfapi", 1)
            .unwrap()
            .is_none());
        save_drive_provider_projection(&conn, "windows-cfapi", 1, &records).unwrap();
        assert_eq!(
            load_drive_provider_projection(&conn, "windows-cfapi", 1)
                .unwrap()
                .unwrap(),
            records
        );
        assert!(load_drive_provider_projection(&conn, "windows-cfapi", 2).is_err());

        let state = state_row(&conn).unwrap();
        assert_eq!(state.0, 0);
        assert!(
            !state.1,
            "local provider checkpoint must not enter Device Continuity"
        );
    }

    // Manual diagnostic: exercise the real Rust helpers with normal SQLite
    // durability, without a desktop app, Telegram session, or timing assertion.
    #[test]
    #[ignore = "manual bounded metadata-cost profile; not a performance gate"]
    fn profile_directory_metadata_cost() {
        use std::time::Instant;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.repotunnel-tmp")
            .join(format!("directory-cost-{}", new_drive_id()));
        std::fs::create_dir_all(&root).unwrap();
        let seed = memory_db();
        let mut schema = seed
            .prepare(
                "SELECT sql FROM sqlite_master
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, name",
            )
            .unwrap();
        let mut schema_sql = Vec::new();
        while schema.next().unwrap() == sqlite::State::Row {
            schema_sql.push(schema.read::<String, _>(0).unwrap());
        }
        drop(schema);
        drop(seed);

        let mut results = Vec::new();
        for kind in ["memory", "file-delete-full", "file-wal-full"] {
            let db_path = root.join(format!("{kind}.db"));
            let conn = if kind == "memory" {
                memory_db()
            } else {
                let conn = sqlite::open(&db_path).unwrap();
                for sql in &schema_sql {
                    conn.execute(sql).unwrap();
                }
                init_drive_schema(&conn).unwrap();
                match kind {
                    "file-delete-full" => conn
                        .execute("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                        .unwrap(),
                    "file-wal-full" => conn
                        .execute("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
                        .unwrap(),
                    _ => unreachable!(),
                }
                conn
            };
            let mut journal = conn.prepare("PRAGMA journal_mode").unwrap();
            journal.next().unwrap();
            let journal_mode = journal.read::<String, _>(0).unwrap();
            drop(journal);
            let mut sync = conn.prepare("PRAGMA synchronous").unwrap();
            sync.next().unwrap();
            let synchronous = sync.read::<i64, _>(0).unwrap();
            drop(sync);

            let parent = create_drive_directory(&conn, None, "Profile parent").unwrap();
            for i in 0..128 {
                create_drive_directory(&conn, Some(&parent.id), &format!("Seed {i}")).unwrap();
            }
            let load_before = std::fs::read_to_string("/proc/loadavg").ok();
            let mut phases = Vec::new();
            let started = Instant::now();
            for _ in 0..128 {
                init_drive_schema(&conn).unwrap();
            }
            phases.push(("schema_checks", started.elapsed().as_secs_f64()));

            let started = Instant::now();
            for i in 0..128 {
                validate_prospective_directory_parent(
                    &conn,
                    &format!("unassigned-profile-{i}"),
                    Some(&parent.id),
                    true,
                )
                .unwrap();
            }
            phases.push(("ancestor_checks", started.elapsed().as_secs_f64()));

            let started = Instant::now();
            for i in 0..128 {
                ensure_name_available(
                    &conn,
                    Some(&parent.id),
                    &format!("New {i}"),
                    None,
                    None,
                    None,
                )
                .unwrap();
            }
            phases.push(("sibling_name_checks", started.elapsed().as_secs_f64()));

            let started = Instant::now();
            for _ in 0..128 {
                mark_drive_dirty(&conn).unwrap();
            }
            phases.push(("revision_writes", started.elapsed().as_secs_f64()));

            let started = Instant::now();
            for i in 0..128 {
                create_drive_directory(&conn, Some(&parent.id), &format!("New {i}")).unwrap();
            }
            phases.push(("full_creation", started.elapsed().as_secs_f64()));
            assert_eq!(load_directories(&conn).unwrap().len(), 257);
            assert!(state_row(&conn).unwrap().1);
            results.push(serde_json::json!({
                "kind": kind,
                "iterations_per_phase": 128,
                "initial_siblings": 128,
                "journal_mode": journal_mode,
                "synchronous": synchronous,
                "load_before": load_before,
                "load_after": std::fs::read_to_string("/proc/loadavg").ok(),
                "phases": phases,
            }));
            drop(conn);
            if kind != "memory" {
                std::fs::remove_file(&db_path).unwrap();
                let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
                let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
            }
        }
        let report = serde_json::json!({
            "scope": "real Rust helpers in sandbox; not a native FUSE throughput result",
            "results": results,
        });
        let path = root.join("result.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        println!("DIRECTORY_COST_RESULT {} {}", path.display(), report);
    }

    #[test]
    fn native_directory_mode_failure_leaves_no_tombstone_and_allows_retry() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Parent").unwrap();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_mkdir_mode BEFORE INSERT ON drive_permissions
             BEGIN SELECT RAISE(ABORT, 'qa mode write failed'); END;",
        )
        .unwrap();
        let error = create_drive_directory_with_mode(&conn, Some(&parent.id), "Retry mode", 0o750)
            .expect_err("mode failure must fail mkdir");
        assert!(error.contains("qa mode write failed"), "{error}");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before
        );
        assert_eq!(state_row(&conn).unwrap(), state);
        assert!(list_drive_trash(&conn).unwrap().is_empty());
        conn.execute("DROP TRIGGER qa_fail_mkdir_mode").unwrap();
        let retry =
            create_drive_directory_with_mode(&conn, Some(&parent.id), "Retry mode", 0o750).unwrap();
        assert_eq!(
            crate::drive_posix::permission(&conn, "directory", &retry.id).unwrap(),
            Some(0o750)
        );
    }

    #[test]
    fn native_directory_mode_revision_failure_rolls_back_permission_and_folder() {
        let conn = memory_db();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        // The first directory revision succeeds. Fail the revision after mode insertion.
        conn.execute(
            "CREATE TRIGGER qa_fail_mode_revision BEFORE UPDATE OF revision ON drive_state
             WHEN EXISTS (SELECT 1 FROM drive_permissions WHERE node_kind='directory')
             BEGIN SELECT RAISE(ABORT, 'qa mode revision failed'); END;",
        )
        .unwrap();
        let error = create_drive_directory_with_mode(&conn, None, "Retry revision", 0o640)
            .expect_err("revision failure after mode insertion must roll back mkdir");
        assert!(error.contains("qa mode revision failed"), "{error}");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before
        );
        assert_eq!(state_row(&conn).unwrap(), state);
        conn.execute("DROP TRIGGER qa_fail_mode_revision").unwrap();
        let retry = create_drive_directory_with_mode(&conn, None, "Retry revision", 0o640).unwrap();
        assert_eq!(
            crate::drive_posix::permission(&conn, "directory", &retry.id).unwrap(),
            Some(0o640)
        );
    }

    #[test]
    fn native_directory_mode_preserves_caller_writes_and_outer_rollback() {
        let conn = memory_db();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        conn.execute("SAVEPOINT caller_native_mkdir").unwrap();
        create_drive_directory_with_mode(&conn, None, "Earlier caller write", 0o700).unwrap();
        let earlier = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let earlier_state = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_nested_mode BEFORE INSERT ON drive_permissions
             BEGIN SELECT RAISE(ABORT, 'qa nested mode failed'); END;",
        )
        .unwrap();
        let error =
            create_drive_directory_with_mode(&conn, None, "Nested retry", 0o750).unwrap_err();
        assert!(error.contains("qa nested mode failed"), "{error}");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            earlier
        );
        assert_eq!(state_row(&conn).unwrap(), earlier_state);
        conn.execute("DROP TRIGGER qa_fail_nested_mode").unwrap();
        create_drive_directory_with_mode(&conn, None, "Nested retry", 0o750).unwrap();
        conn.execute("ROLLBACK TO caller_native_mkdir; RELEASE caller_native_mkdir")
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before
        );
        assert_eq!(state_row(&conn).unwrap(), state);
    }

    #[test]
    fn native_directory_mode_validates_inputs_without_mutation() {
        let conn = memory_db();
        for (name, mode) in [("Zero mode", 0), ("Full mode", 0o777)] {
            let folder = create_drive_directory_with_mode(&conn, None, name, mode).unwrap();
            assert_eq!(
                crate::drive_posix::permission(&conn, "directory", &folder.id).unwrap(),
                Some(mode as u16)
            );
        }
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        for (parent, name, mode) in [
            (None, "Invalid mode", 0o1000),
            (Some("missing-parent"), "Missing parent", 0o750),
            (None, "full mode", 0o750),
            (None, "bad/name", 0o750),
        ] {
            assert!(create_drive_directory_with_mode(&conn, parent, name, mode).is_err());
            assert_eq!(
                serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
                before
            );
            assert_eq!(state_row(&conn).unwrap(), state);
        }
        create_drive_directory_with_mode(&conn, None, "Invalid mode", 0o750).unwrap();
    }

    #[test]
    fn new_directory_revision_failure_rolls_back_and_allows_retry() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Parent").unwrap();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_directory_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa revision write failed'); END;",
        )
        .unwrap();

        let error = create_drive_directory(&conn, Some(&parent.id), "Retry me")
            .expect_err("revision failure must be reported");
        assert!(error.contains("qa revision write failed"), "{error}");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before,
            "failed mkdir must not leave a folder behind"
        );
        assert_eq!(state_row(&conn).unwrap(), state);
        conn.execute("DROP TRIGGER qa_fail_directory_revision")
            .unwrap();

        let retry = create_drive_directory(&conn, Some(&parent.id), "Retry me")
            .expect("the failed mkdir must not reserve the original name");
        assert_eq!(retry.parent_id.as_deref(), Some(parent.id.as_str()));
        assert!(state_row(&conn).unwrap().0 > state.0);
    }

    #[test]
    fn new_directory_creation_preserves_the_callers_outer_savepoint() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Parent").unwrap();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();
        conn.execute("SAVEPOINT caller_directory_batch").unwrap();
        create_drive_directory(&conn, Some(&parent.id), "Earlier caller write").unwrap();
        let earlier = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        conn.execute(
            "CREATE TRIGGER qa_fail_nested_directory_revision
             BEFORE UPDATE OF revision ON drive_state
             BEGIN SELECT RAISE(ABORT, 'qa nested revision write failed'); END;",
        )
        .unwrap();
        let error = create_drive_directory(&conn, Some(&parent.id), "Nested")
            .expect_err("nested revision failure must be reported");
        assert!(error.contains("qa nested revision write failed"), "{error}");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            earlier,
            "failed mkdir must preserve earlier caller writes"
        );
        conn.execute("DROP TRIGGER qa_fail_nested_directory_revision")
            .unwrap();
        let child = create_drive_directory(&conn, Some(&parent.id), "Nested")
            .expect("same-name retry must succeed inside the caller's savepoint");
        assert!(load_directories(&conn)
            .unwrap()
            .iter()
            .any(|row| row.id == child.id));
        assert!(state_row(&conn).unwrap().0 > state.0);

        conn.execute("ROLLBACK TO caller_directory_batch; RELEASE caller_directory_batch")
            .expect("mkdir must not commit or destroy the caller's transaction");
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before
        );
        assert_eq!(state_row(&conn).unwrap(), state);
    }

    #[test]
    fn new_directory_rejects_duplicate_ids_including_tombstones() {
        for trashed_at in [None, Some(7i64)] {
            let conn = memory_db();
            let mut seed = conn
                .prepare(
                    "INSERT INTO drive_directories
                     (id, parent_id, name, created_at, updated_at, trashed_at)
                     VALUES ('duplicate-id', NULL, 'Original', 1, 2, ?)",
                )
                .unwrap();
            seed.bind((1, trashed_at)).unwrap();
            seed.next().unwrap();
            drop(seed);
            let before = serde_json::to_vec(&load_manifest_directories(&conn).unwrap()).unwrap();
            let state = state_row(&conn).unwrap();

            let error = validate_prospective_directory_parent(&conn, "duplicate-id", None, true)
                .expect_err("neither active nor tombstoned identities may be reused");
            assert!(error.contains("already exists"), "{error}");
            assert_eq!(
                serde_json::to_vec(&load_manifest_directories(&conn).unwrap()).unwrap(),
                before
            );
            assert_eq!(state_row(&conn).unwrap(), state);
        }
    }

    #[test]
    fn new_directory_rejects_an_indirect_missing_ancestor_without_mutation() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('parent','missing-grandparent','Parent',1,2)",
        )
        .unwrap();
        let before = serde_json::to_vec(&load_manifest_directories(&conn).unwrap()).unwrap();
        let state = state_row(&conn).unwrap();

        let error = create_drive_directory(&conn, Some("parent"), "Unreachable child")
            .expect_err("an existing immediate parent does not make missing ancestry valid");
        assert!(error.contains("does not exist"), "{error}");
        assert_eq!(
            serde_json::to_vec(&load_manifest_directories(&conn).unwrap()).unwrap(),
            before
        );
        assert_eq!(state_row(&conn).unwrap(), state);
    }

    #[test]
    fn new_directory_creation_preserves_disconnected_corruption_without_validating_it() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Healthy").unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('cycle-a','cycle-b','A',1,2),('cycle-b','cycle-a','B',1,2)",
        )
        .unwrap();
        let damaged_before: Vec<_> = load_manifest_directories(&conn)
            .unwrap()
            .into_iter()
            .filter(|directory| directory.id.starts_with("cycle-"))
            .collect();
        let state = state_row(&conn).unwrap();

        let child = create_drive_directory(&conn, Some(&parent.id), "Reachable child")
            .expect("a new leaf must inspect its own ancestry rather than another subtree");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.backing_logical_channel_id, None);
        let after = load_manifest_directories(&conn).unwrap();
        assert!(after.iter().any(|directory| directory.id == child.id));
        let damaged_after: Vec<_> = after
            .iter()
            .filter(|directory| directory.id.starts_with("cycle-"))
            .cloned()
            .collect();
        assert_eq!(
            serde_json::to_vec(&damaged_after).unwrap(),
            serde_json::to_vec(&damaged_before).unwrap(),
            "local creation must not rewrite unrelated corrupt metadata"
        );
        assert!(state_row(&conn).unwrap().0 > state.0);
        assert!(
            validate_directory_topology(&after).is_err(),
            "full graph validation must still reject the disconnected cycle"
        );
    }

    #[test]
    fn new_directory_rejects_a_trashed_parent_without_mutation() {
        let conn = memory_db();
        let parent = create_drive_directory(&conn, None, "Deleted").unwrap();
        trash_drive_directory(&conn, &parent.id).unwrap();
        let before = serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap();
        let result = create_drive_directory(&conn, Some(&parent.id), "Hidden child");
        assert!(
            result.is_err(),
            "must not create an unreachable child in trash"
        );
        assert_eq!(
            serde_json::to_vec(&build_manifest(&conn).unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn new_directory_rejects_a_trashed_ancestor_without_mutation() {
        let conn = memory_db();
        let root = create_drive_directory(&conn, None, "Root").unwrap();
        let parent = create_drive_directory(&conn, Some(&root.id), "Parent").unwrap();
        let mut stmt = conn
            .prepare("UPDATE drive_directories SET trashed_at=1 WHERE id=?")
            .unwrap();
        stmt.bind((1, root.id.as_str())).unwrap();
        stmt.next().unwrap();
        drop(stmt);
        let before = state_row(&conn).unwrap();
        let result = create_drive_directory(&conn, Some(&parent.id), "Hidden child");
        assert!(result.is_err(), "every ancestor must still be active");
        assert_eq!(state_row(&conn).unwrap(), before);
        let mut stmt = conn
            .prepare("SELECT count(*) FROM drive_directories")
            .unwrap();
        stmt.next().unwrap();
        assert_eq!(stmt.read::<i64, _>(0).unwrap(), 2);
    }

    #[test]
    fn new_directory_rejects_missing_or_cyclic_ancestry_without_mutation() {
        let conn = memory_db();
        assert!(create_drive_directory(&conn, Some("missing"), "Child").is_err());
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
                      VALUES('a','b','A',0,0),('b','a','B',0,0)",
        )
        .unwrap();
        let before = state_row(&conn).unwrap();
        assert!(create_drive_directory(&conn, Some("a"), "Child").is_err());
        assert_eq!(state_row(&conn).unwrap(), before);
    }

    #[test]
    fn local_directory_creation_enforces_native_provider_depth_limit() {
        let conn = memory_db();
        let mut parent: Option<String> = None;
        for depth in 0..MAX_DRIVE_DIRECTORY_DEPTH {
            let directory =
                create_drive_directory(&conn, parent.as_deref(), &format!("Folder {depth}"))
                    .unwrap();
            parent = Some(directory.id);
        }
        assert!(create_drive_directory(&conn, parent.as_deref(), "Too Deep").is_err());
    }

    #[test]
    fn local_directory_move_cannot_push_descendants_beyond_provider_depth_limit() {
        let conn = memory_db();
        let mut parent: Option<String> = None;
        for depth in 0..(MAX_DRIVE_DIRECTORY_DEPTH - 1) {
            let directory =
                create_drive_directory(&conn, parent.as_deref(), &format!("Chain {depth}"))
                    .unwrap();
            parent = Some(directory.id);
        }
        let moving = create_drive_directory(&conn, None, "Moving").unwrap();
        let leaf = create_drive_directory(&conn, Some(&moving.id), "Leaf").unwrap();
        assert!(rename_drive_directory(&conn, &moving.id, parent.as_deref(), "Moving").is_err());
        assert_eq!(
            load_directories(&conn)
                .unwrap()
                .into_iter()
                .find(|directory| directory.id == leaf.id)
                .unwrap()
                .parent_id
                .as_deref(),
            Some(moving.id.as_str())
        );
    }

    #[test]
    fn remote_manifest_rejects_portable_sibling_namespace_collisions() {
        let mut manifest = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 1,
            crypto: None,
            directories: vec![
                DriveDirectoryRecord {
                    id: "dir-a".into(),
                    parent_id: None,
                    name: "Résumé".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
                DriveDirectoryRecord {
                    id: "dir-b".into(),
                    parent_id: None,
                    name: "résumé".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
            ],
            entries: Vec::new(),
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        assert!(validate_manifest(&manifest).is_err());

        manifest.directories.truncate(1);
        manifest.directories[0].name = "Café.txt".into();
        manifest.entries.push(DriveEntryRecord {
            entry_id: "legacy-entry".into(),
            file_id: "legacy-file".into(),
            directory_id: None,
            display_name: "Cafe\u{301}.txt".into(),
            created_at: 1,
            updated_at: 1,
            encryption_version: 0,
            plaintext_size: None,
            crypto_id: None,
            content_fingerprint: None,
            trashed_at: None,
        });
        assert!(validate_manifest(&manifest).is_err());

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
            "v2-entry",
            "12121212343434345656565678787878",
            None,
            "Ångström.txt",
            202,
            8101,
        );
        let mut v2_manifest = build_manifest(&source).unwrap();
        v2_manifest.directories.push(DriveDirectoryRecord {
            id: "v2-collision-dir".into(),
            parent_id: None,
            name: "ångström.txt".into(),
            backing_logical_channel_id: None,
            created_at: 1,
            updated_at: 1,
            trashed_at: None,
        });
        assert!(validate_manifest(&v2_manifest).is_err());
    }

    #[test]
    fn manifest_rejects_directory_cycles() {
        let manifest = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 1,
            crypto: None,
            directories: vec![
                DriveDirectoryRecord {
                    id: "a".into(),
                    parent_id: Some("b".into()),
                    name: "A".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
                DriveDirectoryRecord {
                    id: "b".into(),
                    parent_id: Some("a".into()),
                    name: "B".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
            ],
            entries: Vec::new(),
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn manifest_rejects_directory_hierarchy_deeper_than_native_provider_limit() {
        let mut directories = Vec::new();
        for depth in 0..=512usize {
            directories.push(DriveDirectoryRecord {
                id: format!("dir-{depth}"),
                parent_id: if depth == 0 {
                    None
                } else {
                    Some(format!("dir-{}", depth - 1))
                },
                name: format!("Folder {depth}"),
                backing_logical_channel_id: None,
                created_at: 1,
                updated_at: 1,
                trashed_at: None,
            });
        }
        let manifest = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
            schema_version: DRIVE_SCHEMA_VERSION,
            revision: 1,
            crypto: None,
            directories,
            entries: Vec::new(),
            storage_channels: vec![],
            objects: vec![],
            parts: vec![],
            v2_entries: vec![],
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn manifest_rejects_unsigned_legacy_plaintext_size_overflow_before_import() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO logical_channels VALUES(
                 'qa-import-channel', 101, 'Synthetic', 'owner', 1, 1, 1);
             INSERT INTO logical_files VALUES(
                 'qa-import-file', 'qa-import-channel', 102, 'stable.bin',
                 4, NULL, 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                 (entry_id, file_id, directory_id, display_name, created_at,
                  updated_at, encryption_version, plaintext_size)
             VALUES ('qa-import-entry', 'qa-import-file', NULL, 'stable.bin',
                     1, 1, 0, 4);",
        )
        .unwrap();
        let mut manifest = build_manifest(&conn).unwrap();
        let original_revision = state_row(&conn).unwrap();
        assert_eq!(manifest.entries.len(), 1);

        manifest.entries[0].plaintext_size = Some(u64::MAX);
        assert!(
            validate_manifest(&manifest).is_err(),
            "manifest must reject unsigned sizes that cannot fit in SQLite"
        );
        assert!(
            apply_manifest(&conn, &manifest).is_err(),
            "manifest replacement cannot persist a negative cast from u64::MAX"
        );
        assert_eq!(state_row(&conn).unwrap(), original_revision);
        assert_eq!(
            drive_file_by_entry(&conn, "qa-import-entry")
                .unwrap()
                .unwrap()
                .plaintext_size,
            Some(4),
        );

        manifest.entries[0].plaintext_size = Some(i64::MAX as u64);
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn manifest_rejects_missing_parent_references() {
        let manifest = DriveManifestV1 {
            symlinks: Vec::new(),
            permissions: Vec::new(),
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
