use crate::models::FileMetadata;
use grammers_client::types::Media;
use serde::{Deserialize, Serialize};
use sqlite;
use std::collections::HashSet;
use std::path::PathBuf;
use tauri::Manager;

pub const MANIFEST_SCHEMA_VERSION: u32 = 2;
const LEGACY_MANIFEST_SCHEMA_VERSION: u32 = 1;
const TDLIB_MESSAGE_ID_SHIFT: u32 = 20;
const TDLIB_MESSAGE_ID_LOW_MASK: i64 = (1_i64 << TDLIB_MESSAGE_ID_SHIFT) - 1;
pub const MANIFEST_FILE_PREFIX: &str = ".terarelay-manifest-";
pub const MANIFEST_FILE_SUFFIX: &str = ".json";
pub const MANIFEST_CAPTION_PREFIX: &str = "TRM1:";
pub const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestChunkV1 {
    pub index: u32,
    pub message_id: i64,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogicalFileManifestV1 {
    pub schema_version: u32,
    pub file_id: String,
    pub logical_channel_id: String,
    pub original_name: String,
    pub total_size: u64,
    pub mime_type: Option<String>,
    pub created_at: i64,
    pub whole_sha256: Option<String>,
    pub chunks: Vec<ManifestChunkV1>,
}

pub fn new_file_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn manifest_file_name(file_id: &str) -> String {
    format!("{MANIFEST_FILE_PREFIX}{file_id}{MANIFEST_FILE_SUFFIX}")
}

pub fn manifest_caption(file_id: &str) -> String {
    format!("{MANIFEST_CAPTION_PREFIX}{file_id}")
}

fn valid_file_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn manifest_file_id(name: &str, caption: &str) -> Option<String> {
    let from_name = name
        .strip_prefix(MANIFEST_FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(MANIFEST_FILE_SUFFIX))
        .filter(|id| valid_file_id(id));

    let from_caption = caption
        .strip_prefix(MANIFEST_CAPTION_PREFIX)
        .filter(|id| valid_file_id(id));

    match (from_name, from_caption) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn normalize_legacy_manifest_message_ids(
    manifest: &mut LogicalFileManifestV1,
) -> Result<(), String> {
    if manifest.schema_version != LEGACY_MANIFEST_SCHEMA_VERSION || manifest.chunks.is_empty() {
        return Ok(());
    }

    // The first manifest implementation accidentally persisted TDLib's
    // internal MessageId values for native uploads. TDLib encodes ordinary
    // server message IDs as server_id << 20. Grammers uses server_id directly.
    //
    // Legacy Grammers manifests already contain raw IDs. We only migrate when
    // EVERY chunk has the TDLib server-ID bit layout; this avoids rewriting a
    // mixed or ordinary V1 manifest.
    let looks_tdlib_encoded = manifest.chunks.iter().all(|chunk| {
        chunk.message_id > 0 && chunk.message_id & TDLIB_MESSAGE_ID_LOW_MASK == 0 && {
            let server_id = chunk.message_id >> TDLIB_MESSAGE_ID_SHIFT;
            server_id > 0 && server_id <= i32::MAX as i64
        }
    });

    if looks_tdlib_encoded {
        for chunk in &mut manifest.chunks {
            chunk.message_id =
                crate::commands::utils::tdlib_message_id_to_server_id(chunk.message_id)?;
        }
    }
    Ok(())
}

pub fn validate_manifest(
    manifest: &LogicalFileManifestV1,
    expected_logical_channel_id: Option<&str>,
) -> Result<(), String> {
    if manifest.schema_version != LEGACY_MANIFEST_SCHEMA_VERSION
        && manifest.schema_version != MANIFEST_SCHEMA_VERSION
    {
        return Err(format!(
            "Unsupported TeraRelay manifest version {}",
            manifest.schema_version
        ));
    }
    if !valid_file_id(&manifest.file_id) {
        return Err("Invalid TeraRelay manifest file ID".to_string());
    }
    if manifest.logical_channel_id.is_empty() {
        return Err("Manifest has no logical channel ID".to_string());
    }
    if let Some(expected) = expected_logical_channel_id {
        if manifest.logical_channel_id != expected {
            return Err("Manifest belongs to a different TeraRelay channel".to_string());
        }
    }
    if manifest.original_name.trim().is_empty() {
        return Err("Manifest has no original filename".to_string());
    }
    if manifest.chunks.is_empty() {
        return Err("Manifest has no storage chunks".to_string());
    }

    let mut seen_messages = HashSet::new();
    let mut total = 0u64;
    for (zero_index, chunk) in manifest.chunks.iter().enumerate() {
        let expected_index = u32::try_from(zero_index)
            .ok()
            .and_then(|i| i.checked_add(1))
            .ok_or_else(|| "Manifest part index overflow".to_string())?;
        if chunk.index != expected_index {
            return Err(format!(
                "Manifest chunk order is invalid at index {}",
                expected_index
            ));
        }
        if chunk.message_id <= 0 || chunk.message_id > i32::MAX as i64 {
            return Err(format!(
                "Manifest chunk {} has an invalid canonical Telegram message ID",
                chunk.index
            ));
        }
        if chunk.size == 0 {
            return Err(format!("Manifest chunk {} has zero size", chunk.index));
        }
        if !seen_messages.insert(chunk.message_id) {
            return Err("Manifest contains a duplicate Telegram message ID".to_string());
        }
        if let Some(hash) = chunk.sha256.as_deref() {
            if !valid_sha256(hash) {
                return Err(format!(
                    "Manifest chunk {} has an invalid SHA-256",
                    chunk.index
                ));
            }
        }
        total = total
            .checked_add(chunk.size)
            .ok_or_else(|| "Manifest total size overflow".to_string())?;
    }

    if total != manifest.total_size {
        return Err(format!(
            "Manifest size mismatch: chunks total {} bytes but file says {} bytes",
            total, manifest.total_size
        ));
    }
    if let Some(hash) = manifest.whole_sha256.as_deref() {
        if !valid_sha256(hash) {
            return Err("Manifest whole-file SHA-256 is invalid".to_string());
        }
    }
    Ok(())
}

/// Bound metadata before uploading any payload. The estimate uses maximum
/// numeric widths and a checksum on every part, while preserving the bounded
/// manifest parser shared with existing clients.
pub fn ensure_upload_manifest_capacity(
    channel_id: &str,
    original_name: &str,
    total_size: u64,
    parts: u32,
) -> Result<(), String> {
    let header = LogicalFileManifestV1 {
        schema_version: MANIFEST_SCHEMA_VERSION,
        file_id: "0".repeat(32),
        logical_channel_id: channel_id.to_string(),
        original_name: original_name.to_string(),
        total_size,
        mime_type: mime_guess::from_path(original_name)
            .first()
            .map(|m| m.essence_str().to_string()),
        created_at: i64::MAX,
        whole_sha256: None,
        chunks: Vec::new(),
    };
    let largest_chunk = ManifestChunkV1 {
        index: u32::MAX,
        message_id: i64::from(i32::MAX),
        size: u64::MAX,
        sha256: Some("0".repeat(64)),
    };
    let header_bytes = serde_json::to_vec(&header)
        .map_err(|e| e.to_string())?
        .len() as u64;
    let per_chunk = serde_json::to_vec(&largest_chunk)
        .map_err(|e| e.to_string())?
        .len() as u64
        + 1;
    let estimated = u64::from(parts)
        .checked_mul(per_chunk)
        .and_then(|n| n.checked_add(header_bytes));
    if parts == 0 || estimated.is_none_or(|n| n > MAX_MANIFEST_BYTES) {
        return Err(format!(
            "File requires {parts} storage parts; its metadata exceeds the supported manifest capacity. No file data was uploaded."
        ));
    }
    Ok(())
}

pub fn encode_manifest(manifest: &LogicalFileManifestV1) -> Result<Vec<u8>, String> {
    validate_manifest(manifest, None)?;
    serde_json::to_vec(manifest).map_err(|e| format!("Failed to encode TeraRelay manifest: {e}"))
}

pub fn decode_manifest(
    bytes: &[u8],
    expected_file_id: Option<&str>,
    expected_logical_channel_id: Option<&str>,
) -> Result<LogicalFileManifestV1, String> {
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("TeraRelay manifest is too large".to_string());
    }
    let mut manifest: LogicalFileManifestV1 = serde_json::from_slice(bytes)
        .map_err(|e| format!("Invalid TeraRelay manifest JSON: {e}"))?;
    normalize_legacy_manifest_message_ids(&mut manifest)?;
    validate_manifest(&manifest, expected_logical_channel_id)?;
    if let Some(expected) = expected_file_id {
        if manifest.file_id != expected {
            return Err("Manifest file ID does not match its Telegram document".to_string());
        }
    }
    Ok(manifest)
}

pub async fn download_manifest(
    client: &grammers_client::Client,
    media: &Media,
    expected_file_id: Option<&str>,
    expected_logical_channel_id: Option<&str>,
) -> Result<LogicalFileManifestV1, String> {
    let mut data = Vec::new();
    let mut download = client.iter_download(media).chunk_size(64 * 1024);
    loop {
        match download.next().await {
            Ok(Some(chunk)) => {
                if data.len() as u64 + chunk.len() as u64 > MAX_MANIFEST_BYTES {
                    return Err("TeraRelay manifest exceeds the 2 MiB safety limit".to_string());
                }
                data.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("Failed to download TeraRelay manifest: {e}")),
        }
    }
    if data.is_empty() {
        return Err("TeraRelay manifest document is empty".to_string());
    }
    decode_manifest(&data, expected_file_id, expected_logical_channel_id)
}

pub async fn write_manifest_temp(
    app: &tauri::AppHandle,
    manifest: &LogicalFileManifestV1,
) -> Result<(PathBuf, u64), String> {
    let bytes = encode_manifest(manifest)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("TeraRelay manifest exceeds the 2 MiB safety limit".to_string());
    }

    let root = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("Failed to resolve TeraRelay cache directory: {e}"))?
        .join("manifest-staging");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| format!("Failed to create manifest staging directory: {e}"))?;
    let path = root.join(manifest_file_name(&manifest.file_id));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| format!("Failed to write TeraRelay manifest: {e}"))?;
    Ok((path, bytes.len() as u64))
}

pub fn persist_manifest(
    conn: &sqlite::Connection,
    manifest: &LogicalFileManifestV1,
) -> Result<(), String> {
    validate_manifest(manifest, None)?;
    let first_message_id = manifest
        .chunks
        .first()
        .map(|c| c.message_id)
        .ok_or_else(|| "Manifest has no first chunk".to_string())?;

    let mut stmt = conn
        .prepare(
            "INSERT INTO logical_files
             (file_id, logical_channel_id, first_message_id, display_name, total_size,
              mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'complete')
             ON CONFLICT(file_id) DO UPDATE SET
               logical_channel_id = excluded.logical_channel_id,
               first_message_id = excluded.first_message_id,
               display_name = excluded.display_name,
               total_size = excluded.total_size,
               mime_type = excluded.mime_type,
               chunk_count = excluded.chunk_count,
               whole_sha256 = excluded.whole_sha256,
               storage_version = excluded.storage_version,
               created_at = excluded.created_at,
               status = 'complete'",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, manifest.file_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, manifest.logical_channel_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, first_message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, manifest.original_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, manifest.total_size as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, manifest.mime_type.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, manifest.chunks.len() as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((8, manifest.whole_sha256.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((9, manifest.schema_version as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((10, manifest.created_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    drop(stmt);

    let mut delete = conn
        .prepare("DELETE FROM logical_file_chunks WHERE file_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete
        .bind((1, manifest.file_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete.next().map_err(|e: sqlite::Error| e.to_string())?;
    drop(delete);

    for chunk in &manifest.chunks {
        let mut insert = conn
            .prepare(
                "INSERT INTO logical_file_chunks
                 (file_id, chunk_index, message_id, chunk_size, sha256)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((1, manifest.file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((2, chunk.index as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((3, chunk.message_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((4, chunk.size as i64))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((5, chunk.sha256.as_deref()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert.next().map_err(|e: sqlite::Error| e.to_string())?;
    }
    Ok(())
}

pub fn cached_file_ids_for_channel(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT file_id FROM logical_files WHERE logical_channel_id = ? AND status = 'complete'")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        ids.insert(stmt.read::<String, _>(0).map_err(|e| e.to_string())?);
    }
    Ok(ids)
}

pub fn cached_file_metadata(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    active_remote_file_ids: &HashSet<String>,
    folder_id: i64,
) -> Result<Vec<FileMetadata>, String> {
    if active_remote_file_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT file_id, first_message_id, display_name, total_size, mime_type,
                    chunk_count, created_at
             FROM logical_files
             WHERE logical_channel_id = ? AND status = 'complete'
             ORDER BY created_at DESC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;

    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let file_id = stmt
            .read::<String, _>("file_id")
            .map_err(|e| e.to_string())?;
        if !active_remote_file_ids.contains(&file_id) {
            continue;
        }
        let name = stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?;
        let created_at = stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?;
        let created = chrono::DateTime::<chrono::Utc>::from_timestamp(created_at, 0)
            .map(|v| v.to_rfc3339())
            .unwrap_or_else(|| created_at.to_string());
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_string());

        files.push(FileMetadata {
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
        });
    }
    Ok(files)
}

pub fn cached_manifest_for_first_message(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    first_message_id: i64,
) -> Result<Option<LogicalFileManifestV1>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT file_id, display_name, total_size, mime_type, whole_sha256,
                    storage_version, created_at
             FROM logical_files
             WHERE logical_channel_id = ? AND first_message_id = ? AND status = 'complete'
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, first_message_id))
        .map_err(|e: sqlite::Error| e.to_string())?;

    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }

    let file_id = stmt
        .read::<String, _>("file_id")
        .map_err(|e| e.to_string())?;
    let original_name = stmt
        .read::<String, _>("display_name")
        .map_err(|e| e.to_string())?;
    let total_size = stmt
        .read::<i64, _>("total_size")
        .map_err(|e| e.to_string())? as u64;
    let mime_type = stmt.read::<Option<String>, _>("mime_type").ok().flatten();
    let whole_sha256 = stmt
        .read::<Option<String>, _>("whole_sha256")
        .ok()
        .flatten();
    let schema_version = stmt
        .read::<i64, _>("storage_version")
        .map_err(|e| e.to_string())? as u32;
    let created_at = stmt
        .read::<i64, _>("created_at")
        .map_err(|e| e.to_string())?;
    drop(stmt);

    let mut chunks = Vec::new();
    let mut chunk_stmt = conn
        .prepare(
            "SELECT chunk_index, message_id, chunk_size, sha256
             FROM logical_file_chunks
             WHERE file_id = ?
             ORDER BY chunk_index ASC",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    chunk_stmt
        .bind((1, file_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = chunk_stmt
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
    {
        chunks.push(ManifestChunkV1 {
            index: chunk_stmt
                .read::<i64, _>("chunk_index")
                .map_err(|e| e.to_string())? as u32,
            message_id: chunk_stmt
                .read::<i64, _>("message_id")
                .map_err(|e| e.to_string())?,
            size: chunk_stmt
                .read::<i64, _>("chunk_size")
                .map_err(|e| e.to_string())? as u64,
            sha256: chunk_stmt
                .read::<Option<String>, _>("sha256")
                .ok()
                .flatten(),
        });
    }

    let mut manifest = LogicalFileManifestV1 {
        schema_version,
        file_id,
        logical_channel_id: logical_channel_id.to_string(),
        original_name,
        total_size,
        mime_type,
        created_at,
        whole_sha256,
        chunks,
    };
    normalize_legacy_manifest_message_ids(&mut manifest)?;
    validate_manifest(&manifest, Some(logical_channel_id))?;
    Ok(Some(manifest))
}

pub fn delete_cached_file(conn: &sqlite::Connection, file_id: &str) -> Result<(), String> {
    let mut delete_chunks = conn
        .prepare("DELETE FROM logical_file_chunks WHERE file_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete_chunks
        .bind((1, file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete_chunks
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?;
    drop(delete_chunks);

    let mut delete_file = conn
        .prepare("DELETE FROM logical_files WHERE file_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete_file
        .bind((1, file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    delete_file
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

pub fn cached_chunk_message_ids(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    active_remote_file_ids: &HashSet<String>,
) -> Result<HashSet<i64>, String> {
    let mut ids = HashSet::new();
    if active_remote_file_ids.is_empty() {
        return Ok(ids);
    }

    let mut stmt = conn
        .prepare(
            "SELECT c.file_id, c.message_id
             FROM logical_file_chunks c
             JOIN logical_files f ON f.file_id = c.file_id
             WHERE f.logical_channel_id = ? AND f.status = 'complete'",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let file_id = stmt
            .read::<String, _>("file_id")
            .map_err(|e| e.to_string())?;
        if active_remote_file_ids.contains(&file_id) {
            ids.insert(
                stmt.read::<i64, _>("message_id")
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> LogicalFileManifestV1 {
        LogicalFileManifestV1 {
            schema_version: MANIFEST_SCHEMA_VERSION,
            file_id: "0123456789abcdef0123456789abcdef".to_string(),
            logical_channel_id: "fedcba9876543210fedcba9876543210".to_string(),
            original_name: "Movie.mkv".to_string(),
            total_size: 15,
            mime_type: Some("video/x-matroska".to_string()),
            created_at: 1_790_000_000,
            whole_sha256: None,
            chunks: vec![
                ManifestChunkV1 {
                    index: 1,
                    message_id: 101,
                    size: 10,
                    sha256: Some("a".repeat(64)),
                },
                ManifestChunkV1 {
                    index: 2,
                    message_id: 102,
                    size: 5,
                    sha256: Some("b".repeat(64)),
                },
            ],
        }
    }

    #[test]
    fn large_manifest_round_trip_supports_a_ten_terabyte_single_file() {
        let mut manifest = sample_manifest();
        manifest.original_name = "backup.tar.zst".to_string();
        manifest.chunks = (1..=5000)
            .map(|index| ManifestChunkV1 {
                index,
                message_id: i64::from(index),
                size: 2_000_000_000,
                sha256: Some("a".repeat(64)),
            })
            .collect();
        manifest.total_size = 10_000_000_000_000;
        ensure_upload_manifest_capacity(
            &manifest.logical_channel_id,
            &manifest.original_name,
            manifest.total_size,
            5000,
        )
        .unwrap();
        let bytes = encode_manifest(&manifest).unwrap();
        assert!(bytes.len() as u64 <= MAX_MANIFEST_BYTES);
        assert_eq!(decode_manifest(&bytes, None, None).unwrap(), manifest);
        assert!(
            ensure_upload_manifest_capacity("channel", "too-many.bin", u64::MAX, u32::MAX).is_err()
        );
    }

    #[test]
    fn manifest_round_trip_is_stable() {
        let manifest = sample_manifest();
        let bytes = encode_manifest(&manifest).unwrap();
        let decoded = decode_manifest(
            &bytes,
            Some(&manifest.file_id),
            Some(&manifest.logical_channel_id),
        )
        .unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn manifest_document_identity_is_strict() {
        let id = "0123456789abcdef0123456789abcdef";
        let name = manifest_file_name(id);
        let caption = manifest_caption(id);
        assert_eq!(manifest_file_id(&name, &caption).as_deref(), Some(id));
        assert_eq!(manifest_file_id(&name, "").as_deref(), Some(id));
        assert_eq!(
            manifest_file_id("ordinary.bin", &caption).as_deref(),
            Some(id)
        );
        assert!(manifest_file_id(".terarelay-manifest-bad.json", "TRM1:bad").is_none());
    }

    #[test]
    fn manifest_rejects_wrong_size_and_order() {
        let mut manifest = sample_manifest();
        manifest.total_size = 14;
        assert!(validate_manifest(&manifest, None).is_err());

        let mut manifest = sample_manifest();
        manifest.chunks[1].index = 3;
        assert!(validate_manifest(&manifest, None).is_err());
    }

    #[test]
    fn manifest_rejects_corrupt_json_duplicate_ids_bad_hash_and_incomplete_set() {
        let corrupt = b"{not-json";
        let err = decode_manifest(corrupt, None, None).unwrap_err();
        assert!(err.contains("Invalid TeraRelay manifest JSON"));

        let mut duplicate = sample_manifest();
        duplicate.chunks[1].message_id = duplicate.chunks[0].message_id;
        let err = validate_manifest(&duplicate, None).unwrap_err();
        assert!(err.contains("duplicate Telegram message ID"));

        let mut bad_hash = sample_manifest();
        bad_hash.chunks[0].sha256 = Some("not-a-sha256".to_string());
        let err = validate_manifest(&bad_hash, None).unwrap_err();
        assert!(err.contains("SHA-256"));

        let mut incomplete = sample_manifest();
        incomplete.chunks.remove(0);
        incomplete.total_size = incomplete.chunks.iter().map(|chunk| chunk.size).sum();
        let err = validate_manifest(&incomplete, None).unwrap_err();
        assert!(err.contains("chunk order"));
    }

    #[test]
    fn manifest_identity_mismatch_is_rejected() {
        let manifest = sample_manifest();
        let bytes = encode_manifest(&manifest).unwrap();

        let wrong_file_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let err = decode_manifest(
            &bytes,
            Some(wrong_file_id),
            Some(&manifest.logical_channel_id),
        )
        .unwrap_err();
        assert!(err.contains("file ID"));

        let wrong_channel_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let err =
            decode_manifest(&bytes, Some(&manifest.file_id), Some(wrong_channel_id)).unwrap_err();
        assert!(err.contains("different TeraRelay channel"));
    }

    #[test]
    fn legacy_tdlib_message_ids_are_normalized_on_decode() {
        let mut manifest = sample_manifest();
        manifest.schema_version = LEGACY_MANIFEST_SCHEMA_VERSION;
        manifest.chunks[0].message_id = 101_i64 << TDLIB_MESSAGE_ID_SHIFT;
        manifest.chunks[1].message_id = 102_i64 << TDLIB_MESSAGE_ID_SHIFT;

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let decoded = decode_manifest(
            &bytes,
            Some(&manifest.file_id),
            Some(&manifest.logical_channel_id),
        )
        .unwrap();

        assert_eq!(decoded.schema_version, LEGACY_MANIFEST_SCHEMA_VERSION);
        assert_eq!(decoded.chunks[0].message_id, 101);
        assert_eq!(decoded.chunks[1].message_id, 102);
    }

    #[test]
    fn current_manifest_raw_message_ids_are_not_rewritten() {
        let manifest = sample_manifest();
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        let bytes = encode_manifest(&manifest).unwrap();
        let decoded = decode_manifest(
            &bytes,
            Some(&manifest.file_id),
            Some(&manifest.logical_channel_id),
        )
        .unwrap();
        assert_eq!(decoded.chunks[0].message_id, 101);
        assert_eq!(decoded.chunks[1].message_id, 102);
    }

    fn test_manifest_db() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_files (
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
        .unwrap();
        conn
    }

    #[test]
    fn duplicate_display_names_remain_distinct_and_cache_is_idempotent() {
        let conn = test_manifest_db();
        let first = sample_manifest();
        let mut second = sample_manifest();
        second.file_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        second.chunks[0].message_id = 201;
        second.chunks[1].message_id = 202;
        second.created_at += 1;

        assert_eq!(first.original_name, second.original_name);
        persist_manifest(&conn, &first).unwrap();
        persist_manifest(&conn, &second).unwrap();
        persist_manifest(&conn, &first).unwrap();

        let file_count = {
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM logical_files WHERE display_name = ?")
                .unwrap();
            stmt.bind((1, first.original_name.as_str())).unwrap();
            assert!(matches!(stmt.next().unwrap(), sqlite::State::Row));
            stmt.read::<i64, _>(0).unwrap()
        };
        assert_eq!(file_count, 2);

        let chunk_count = {
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM logical_file_chunks WHERE file_id = ?")
                .unwrap();
            stmt.bind((1, first.file_id.as_str())).unwrap();
            assert!(matches!(stmt.next().unwrap(), sqlite::State::Row));
            stmt.read::<i64, _>(0).unwrap()
        };
        assert_eq!(chunk_count, first.chunks.len() as i64);
    }

    #[test]
    fn cached_manifest_rejects_missing_chunk_and_corrupt_hash() {
        let conn = test_manifest_db();
        let manifest = sample_manifest();
        persist_manifest(&conn, &manifest).unwrap();

        conn.execute(format!(
            "DELETE FROM logical_file_chunks WHERE file_id = '{}' AND chunk_index = 2",
            manifest.file_id
        ))
        .unwrap();
        let err = cached_manifest_for_first_message(
            &conn,
            &manifest.logical_channel_id,
            manifest.chunks[0].message_id,
        )
        .unwrap_err();
        assert!(err.contains("size mismatch") || err.contains("chunk"));

        persist_manifest(&conn, &manifest).unwrap();
        conn.execute(format!(
            "UPDATE logical_file_chunks SET sha256 = 'corrupt' WHERE file_id = '{}' AND chunk_index = 1",
            manifest.file_id
        ))
        .unwrap();
        let err = cached_manifest_for_first_message(
            &conn,
            &manifest.logical_channel_id,
            manifest.chunks[0].message_id,
        )
        .unwrap_err();
        assert!(err.contains("SHA-256"));
    }

    #[test]
    fn manifest_cache_round_trip_uses_first_message_identity() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_files (
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
        .unwrap();

        let manifest = sample_manifest();
        persist_manifest(&conn, &manifest).unwrap();

        let loaded = cached_manifest_for_first_message(
            &conn,
            &manifest.logical_channel_id,
            manifest.chunks[0].message_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, manifest);

        assert!(
            cached_manifest_for_first_message(&conn, &loaded.logical_channel_id, 999_999,)
                .unwrap()
                .is_none()
        );
    }
}
