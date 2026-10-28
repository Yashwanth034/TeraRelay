//! Hot Drive metadata lookups for an already-initialized provider database.
//! Return current rows rather than caching names, sizes or content versions.
use crate::commands::drive_metadata::{DriveDirectoryRecord, DriveFileRecord, DrivePendingRecord};
use crate::drive_fs_core::DriveNodeId;
use crate::drive_posix::DriveSymlinkRecord;

#[cfg(test)]
std::thread_local! {
    static DRIVE_LOOKUP_QUERY_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_lookup_query_step() {
    DRIVE_LOOKUP_QUERY_STEPS.with(|steps| steps.set(steps.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn record_lookup_query_step() {}

#[cfg(test)]
pub(crate) fn reset_lookup_query_steps() {
    DRIVE_LOOKUP_QUERY_STEPS.with(|steps| steps.set(0));
}

#[cfg(test)]
pub(crate) fn lookup_query_steps() -> usize {
    DRIVE_LOOKUP_QUERY_STEPS.with(|steps| steps.get())
}

pub(crate) enum RegularFileEntry {
    Pending(DrivePendingRecord),
    File(DriveFileRecord),
}

pub(crate) struct RegularFileMetadata {
    pub(crate) entry: RegularFileEntry,
    pub(crate) mode: Option<u16>,
}

pub(crate) enum DriveChildEntry {
    Symlink(DriveSymlinkRecord),
    Directory(DriveDirectoryRecord),
    Pending(DrivePendingRecord),
    File(DriveFileRecord),
}

pub(crate) fn init_indexes(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE INDEX IF NOT EXISTS drive_lookup_dir_name ON drive_directories(parent_id,name) WHERE trashed_at IS NULL;
         CREATE INDEX IF NOT EXISTS drive_lookup_file_name ON drive_file_entries(directory_id,display_name) WHERE trashed_at IS NULL;
         CREATE INDEX IF NOT EXISTS drive_lookup_v2_name ON drive_entries_v2(directory_id,display_name) WHERE trashed_at IS NULL;
         CREATE INDEX IF NOT EXISTS drive_lookup_pending_name ON drive_pending_files(directory_id,display_name);
         CREATE INDEX IF NOT EXISTS drive_lookup_pending_base ON drive_pending_files(base_entry_id);"
    ).map_err(|e| e.to_string())
}

fn read_directory_timestamp(stmt: &sqlite::Statement<'_>, column: usize) -> Result<i64, String> {
    match stmt
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive directory timestamp: {e}"))?
    {
        sqlite::Value::Integer(value) if value >= 0 => Ok(value),
        _ => Err("Invalid Drive directory timestamp".to_string()),
    }
}

fn read_directory_timestamps(
    stmt: &sqlite::Statement<'_>,
    created_column: usize,
    updated_column: usize,
) -> Result<(i64, i64), String> {
    let created_at = read_directory_timestamp(stmt, created_column)?;
    let updated_at = read_directory_timestamp(stmt, updated_column)?;
    if updated_at < created_at {
        return Err("Invalid Drive directory timestamp order".to_string());
    }
    Ok((created_at, updated_at))
}

pub(crate) fn directory(
    conn: &sqlite::Connection,
    id: &str,
) -> Result<Option<DriveDirectoryRecord>, String> {
    record_lookup_query_step();
    let mut s = conn.prepare("SELECT id,parent_id,name,backing_logical_channel_id,created_at,updated_at FROM drive_directories WHERE id=? AND trashed_at IS NULL LIMIT 1").map_err(|e|e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let (created_at, updated_at) = read_directory_timestamps(&s, 4, 5)?;
    Ok(Some(DriveDirectoryRecord {
        id: s.read::<String, _>(0).map_err(|e| e.to_string())?,
        parent_id: s.read::<Option<String>, _>(1).map_err(|e| e.to_string())?,
        name: s.read::<String, _>(2).map_err(|e| e.to_string())?,
        backing_logical_channel_id: s.read::<Option<String>, _>(3).map_err(|e| e.to_string())?,
        created_at,
        updated_at,
        trashed_at: None,
    }))
}

/// SQLite is dynamically typed: a corrupt BLOB/REAL must not be coerced
/// into a parent, encryption identity or other nullable file attribute.
fn read_nullable_file_text(
    stmt: &sqlite::Statement<'_>,
    column: &str,
) -> Result<Option<String>, String> {
    match stmt
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive {column}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::String(value) => Ok(Some(value)),
        _ => Err(format!("Invalid Drive {column} type")),
    }
}

fn read_nullable_file_nonnegative_integer(
    stmt: &sqlite::Statement<'_>,
    column: &str,
) -> Result<Option<u64>, String> {
    match stmt
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive {column}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::Integer(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| format!("Invalid negative Drive {column}")),
        _ => Err(format!("Invalid Drive {column} type")),
    }
}

fn read_file_nonnegative_integer(
    stmt: &sqlite::Statement<'_>,
    column: &str,
) -> Result<u64, String> {
    read_nullable_file_nonnegative_integer(stmt, column)?
        .ok_or_else(|| format!("Missing Drive {column}"))
}

fn read_file_required_integer(stmt: &sqlite::Statement<'_>, column: &str) -> Result<i64, String> {
    match stmt
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive file {column}: {e}"))?
    {
        sqlite::Value::Integer(value) => Ok(value),
        _ => Err(format!("Invalid Drive file {column} integer type")),
    }
}

fn read_file_timestamp(stmt: &sqlite::Statement<'_>, column: &str) -> Result<i64, String> {
    match stmt
        .read::<sqlite::Value, _>(column)
        .map_err(|e| format!("Invalid Drive file {column}: {e}"))?
    {
        sqlite::Value::Integer(value) if value >= 0 => Ok(value),
        _ => Err(format!("Invalid Drive file {column} timestamp")),
    }
}

fn read_file_record(stmt: &sqlite::Statement<'_>, prefix: &str) -> Result<DriveFileRecord, String> {
    let entry_id_col = if prefix.is_empty() {
        "entry_id"
    } else {
        "file_entry_id"
    };
    let file_id_col = if prefix.is_empty() {
        "file_id"
    } else {
        "file_file_id"
    };
    let directory_id_col = if prefix.is_empty() {
        "directory_id"
    } else {
        "file_directory_id"
    };
    let display_name_col = if prefix.is_empty() {
        "display_name"
    } else {
        "file_display_name"
    };
    let total_size_col = if prefix.is_empty() {
        "total_size"
    } else {
        "file_total_size"
    };
    let mime_type_col = if prefix.is_empty() {
        "mime_type"
    } else {
        "file_mime_type"
    };
    let first_message_id_col = if prefix.is_empty() {
        "first_message_id"
    } else {
        "file_first_message_id"
    };
    let backing_channel_id_col = if prefix.is_empty() {
        "backing_channel_id"
    } else {
        "file_backing_channel_id"
    };
    let logical_channel_id_col = if prefix.is_empty() {
        "logical_channel_id"
    } else {
        "file_logical_channel_id"
    };
    let created_at_col = if prefix.is_empty() {
        "created_at"
    } else {
        "file_created_at"
    };
    let updated_at_col = if prefix.is_empty() {
        "updated_at"
    } else {
        "file_updated_at"
    };
    let encryption_version_col = if prefix.is_empty() {
        "encryption_version"
    } else {
        "file_encryption_version"
    };
    let plaintext_size_col = if prefix.is_empty() {
        "plaintext_size"
    } else {
        "file_plaintext_size"
    };
    let crypto_id_col = if prefix.is_empty() {
        "crypto_id"
    } else {
        "file_crypto_id"
    };
    let content_fingerprint_col = if prefix.is_empty() {
        "content_fingerprint"
    } else {
        "file_content_fingerprint"
    };
    let storage_version_col = if prefix.is_empty() {
        "storage_version"
    } else {
        "file_storage_version"
    };
    let inode_id_col = if prefix.is_empty() {
        "inode_id"
    } else {
        "file_inode_id"
    };
    let display_name = stmt
        .read::<String, _>(display_name_col)
        .map_err(|e| e.to_string())?;
    let version = match stmt
        .read::<sqlite::Value, _>(storage_version_col)
        .map_err(|e| format!("Invalid Drive {storage_version_col}: {e}"))?
    {
        sqlite::Value::Integer(1) => 1,
        sqlite::Value::Integer(2) => 2,
        _ => return Err("Unsupported or corrupt Drive file storage version".to_string()),
    };
    let mime_type = if version == 2 {
        mime_guess::from_path(&display_name)
            .first()
            .map(|mime| mime.essence_str().to_string())
    } else {
        read_nullable_file_text(stmt, mime_type_col)?
    };
    let created_at = read_file_timestamp(stmt, created_at_col)?;
    let updated_at = read_file_timestamp(stmt, updated_at_col)?;
    if updated_at < created_at {
        return Err("Invalid Drive file timestamp order".to_string());
    }
    Ok(DriveFileRecord {
        entry_id: stmt
            .read::<String, _>(entry_id_col)
            .map_err(|e| e.to_string())?,
        file_id: stmt
            .read::<String, _>(file_id_col)
            .map_err(|e| e.to_string())?,
        inode_id: read_nullable_file_text(stmt, inode_id_col)?,
        directory_id: read_nullable_file_text(stmt, directory_id_col)?,
        display_name,
        total_size: read_file_nonnegative_integer(stmt, total_size_col)?,
        mime_type,
        first_message_id: read_file_required_integer(stmt, first_message_id_col)?,
        backing_channel_id: read_file_required_integer(stmt, backing_channel_id_col)?,
        logical_channel_id: stmt
            .read::<String, _>(logical_channel_id_col)
            .map_err(|e| e.to_string())?,
        created_at,
        updated_at,
        encryption_version: match stmt
            .read::<sqlite::Value, _>(encryption_version_col)
            .map_err(|e| format!("Invalid Drive {encryption_version_col}: {e}"))?
        {
            sqlite::Value::Integer(0) => 0,
            sqlite::Value::Integer(1) => crate::drive_crypto::DRIVE_ENCRYPTION_VERSION,
            _ => return Err("Unsupported or corrupt Drive file encryption version".to_string()),
        },
        plaintext_size: read_nullable_file_nonnegative_integer(stmt, plaintext_size_col)?,
        crypto_id: read_nullable_file_text(stmt, crypto_id_col)?,
        content_fingerprint: read_nullable_file_text(stmt, content_fingerprint_col)?,
        storage_version: version,
    })
}

pub(crate) fn file(conn: &sqlite::Connection, id: &str) -> Result<Option<DriveFileRecord>, String> {
    record_lookup_query_step();
    // V2 has precedence, matching drive_file_by_entry_id. LIMIT avoids loading
    // unrelated entries, manifests, directories or pending writes.
    let mut s=conn.prepare(
        "SELECT e.entry_id,e.object_id AS file_id,e.directory_id,e.display_name,
                o.plaintext_size AS total_size,NULL AS mime_type,
                COALESCE(p.message_id,0) AS first_message_id,COALESCE(p.backing_channel_id,0) AS backing_channel_id,
                '' AS logical_channel_id,e.created_at,e.updated_at,o.encryption_version,
                o.plaintext_size,o.crypto_id,o.content_fingerprint,2 AS storage_version,
                e.inode_id AS inode_id
         FROM drive_entries_v2 e JOIN drive_objects o ON o.object_id=e.object_id
         LEFT JOIN drive_object_parts p ON p.object_id=e.object_id AND p.part_index=0
         WHERE e.entry_id=?1 AND e.trashed_at IS NULL
         UNION ALL
         SELECT e.entry_id,e.file_id,e.directory_id,e.display_name,f.total_size,f.mime_type,
                f.first_message_id,c.backing_channel_id,f.logical_channel_id,f.created_at,e.updated_at,
                e.encryption_version,e.plaintext_size,e.crypto_id,e.content_fingerprint,1,
                NULL AS inode_id
         FROM drive_file_entries e JOIN logical_files f ON f.file_id=e.file_id
         JOIN logical_channels c ON c.logical_id=f.logical_channel_id
         WHERE e.entry_id=?1 AND e.trashed_at IS NULL AND f.status='complete'
           AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
         LIMIT 1"
    ).map_err(|e|e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    read_file_record(&s, "").map(Some)
}

fn pending_required_integer(stmt: &sqlite::Statement<'_>, field: &str) -> Result<i64, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid Drive pending {field}: {e}"))?
    {
        sqlite::Value::Integer(value) => Ok(value),
        _ => Err(format!("Invalid Drive pending {field} type")),
    }
}

fn pending_optional_integer(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<i64>, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid Drive pending {field}: {e}"))?
    {
        sqlite::Value::Null => Ok(None),
        sqlite::Value::Integer(value) => Ok(Some(value)),
        _ => Err(format!("Invalid Drive pending {field} type")),
    }
}

fn pending_optional_string(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<String>, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Invalid Drive pending {field}: {e}"))?
    {
        sqlite::Value::Null => return Ok(None),
        sqlite::Value::String(_) => {}
        _ => return Err(format!("Invalid Drive pending {field} type")),
    }
    // `String` decoding can stop at an embedded NUL. Staging paths and
    // pending IDs must reflect the exact SQLite bytes, never a truncated path.
    let bytes = stmt
        .read::<Vec<u8>, _>(field)
        .map_err(|e| format!("Invalid Drive pending {field}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!("Invalid Drive pending {field} contains NUL"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("Invalid Drive pending {field} is not UTF-8"))
}

fn pending_required_string(stmt: &sqlite::Statement<'_>, field: &str) -> Result<String, String> {
    pending_optional_string(stmt, field)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Invalid Drive pending {field}"))
}

/// All Drive snapshot and targeted lookups use this same fail-closed decoder.
/// SQLite's dynamic typing must never silently downgrade an encrypted V2
/// pending write to an unencrypted V1 record on a failed conversion.
pub(crate) fn read_pending_record(
    stmt: &sqlite::Statement<'_>,
) -> Result<DrivePendingRecord, String> {
    let format_version = pending_required_integer(stmt, "format_version")?;
    if !matches!(format_version, 1 | 2) {
        return Err("Unsupported Drive pending format version".to_string());
    }
    let encryption_version = pending_required_integer(stmt, "encryption_version")?;
    if !matches!(encryption_version, 0 | 1) {
        return Err("Unsupported Drive pending encryption version".to_string());
    }
    let size = pending_required_integer(stmt, "size")?;
    let size = u64::try_from(size).map_err(|_| "Invalid Drive pending size".to_string())?;
    let created_at = pending_required_integer(stmt, "created_at")?;
    let updated_at = pending_required_integer(stmt, "updated_at")?;
    let closed_at = pending_optional_integer(stmt, "closed_at")?;
    if created_at < 0
        || updated_at < created_at
        || closed_at.is_some_and(|closed| closed < created_at || closed > updated_at)
    {
        return Err("Invalid Drive pending timestamps".to_string());
    }
    Ok(DrivePendingRecord {
        id: pending_required_string(stmt, "id")?,
        directory_id: pending_optional_string(stmt, "directory_id")?,
        display_name: pending_required_string(stmt, "display_name")?,
        staging_path: pending_required_string(stmt, "staging_path")?,
        size,
        format_version: format_version as u32,
        backing_channel_id: pending_required_integer(stmt, "backing_channel_id")?,
        base_entry_id: pending_optional_string(stmt, "base_entry_id")?,
        encryption_version: encryption_version as u32,
        crypto_id: pending_optional_string(stmt, "crypto_id")?,
        closed_at,
        created_at,
        updated_at,
    })
}

pub(crate) fn pending_for_file(
    conn: &sqlite::Connection,
    id: &str,
) -> Result<Option<DrivePendingRecord>, String> {
    record_lookup_query_step();
    let mut stmt = conn
        .prepare(
            "SELECT id, directory_id, display_name, staging_path, size, format_version,
                    backing_channel_id, base_entry_id, encryption_version, crypto_id,
                    closed_at, created_at, updated_at
             FROM drive_pending_files
             WHERE base_entry_id=?1 OR id=?1
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    stmt.bind((1, id)).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    read_pending_record(&stmt).map(Some)
}

pub(crate) fn regular_file_metadata(
    conn: &sqlite::Connection,
    id: &str,
) -> Result<Option<RegularFileMetadata>, String> {
    regular_file_metadata_inner(conn, id, true)
}

fn regular_file_metadata_inner(
    conn: &sqlite::Connection,
    id: &str,
    include_mode: bool,
) -> Result<Option<RegularFileMetadata>, String> {
    record_lookup_query_step();
    let mut stmt = conn
        .prepare(
            "WITH pending AS (
                SELECT id, directory_id, display_name, staging_path, size, format_version,
                       backing_channel_id, base_entry_id, encryption_version, crypto_id,
                       closed_at, created_at, updated_at
                FROM drive_pending_files
                WHERE base_entry_id=?1 OR id=?1
                ORDER BY created_at ASC
                LIMIT 1
             ),
             remote_files AS (
                SELECT e.entry_id, e.object_id AS file_id, e.directory_id, e.display_name,
                       o.plaintext_size AS total_size, NULL AS mime_type,
                       COALESCE(p.message_id,0) AS first_message_id,
                       COALESCE(p.backing_channel_id,0) AS backing_channel_id,
                       '' AS logical_channel_id, e.created_at, e.updated_at,
                       o.encryption_version, o.plaintext_size, o.crypto_id,
                       o.content_fingerprint, 2 AS storage_version, e.inode_id
                FROM drive_entries_v2 e
                JOIN drive_objects o ON o.object_id=e.object_id
                LEFT JOIN drive_object_parts p ON p.object_id=e.object_id AND p.part_index=0
                WHERE e.entry_id=?1 AND e.trashed_at IS NULL
                UNION ALL
                SELECT e.entry_id, e.file_id, e.directory_id, e.display_name,
                       f.total_size, f.mime_type, f.first_message_id, c.backing_channel_id,
                       f.logical_channel_id, f.created_at, e.updated_at,
                       e.encryption_version, e.plaintext_size, e.crypto_id,
                       e.content_fingerprint, 1, NULL AS inode_id
                FROM drive_file_entries e
                JOIN logical_files f ON f.file_id=e.file_id
                JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                WHERE e.entry_id=?1 AND e.trashed_at IS NULL AND f.status='complete'
           AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
             )
             SELECT row_kind, id, directory_id, display_name, staging_path, size,
                    format_version, backing_channel_id, base_entry_id, encryption_version,
                    crypto_id, closed_at, created_at, updated_at,
                    file_entry_id, file_file_id, file_directory_id, file_display_name,
                    file_total_size, file_mime_type, file_first_message_id,
                    file_backing_channel_id, file_logical_channel_id, file_created_at,
                    file_updated_at, file_encryption_version, file_plaintext_size,
                    file_crypto_id, file_content_fingerprint, file_storage_version,
                    file_inode_id, effective_mode
             FROM (
                SELECT 0 AS row_kind, p.id AS id, p.directory_id AS directory_id,
                       p.display_name AS display_name, p.staging_path AS staging_path,
                       p.size AS size, p.format_version AS format_version,
                       p.backing_channel_id AS backing_channel_id,
                       p.base_entry_id AS base_entry_id,
                       p.encryption_version AS encryption_version,
                       p.crypto_id AS crypto_id, p.closed_at AS closed_at,
                       p.created_at AS created_at, p.updated_at AS updated_at,
                       NULL AS file_entry_id, NULL AS file_file_id,
                       NULL AS file_directory_id, NULL AS file_display_name,
                       NULL AS file_total_size, NULL AS file_mime_type,
                       NULL AS file_first_message_id, NULL AS file_backing_channel_id,
                       NULL AS file_logical_channel_id, NULL AS file_created_at,
                       NULL AS file_updated_at, NULL AS file_encryption_version,
                       NULL AS file_plaintext_size, NULL AS file_crypto_id,
                       NULL AS file_content_fingerprint, NULL AS file_storage_version,
                       NULL AS file_inode_id,
                       CASE WHEN ?2=1 THEN COALESCE(
                           (SELECT mode FROM drive_permissions
                            WHERE node_kind='pending' AND node_id=p.id),
                           (SELECT mode FROM drive_permissions
                            WHERE node_kind='file' AND node_id=p.base_entry_id)
                       ) ELSE NULL END AS effective_mode
                FROM pending p
                UNION ALL
                SELECT 1 AS row_kind, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                       NULL, NULL, NULL, NULL, NULL,
                       f.entry_id AS file_entry_id, f.file_id AS file_file_id,
                       f.directory_id AS file_directory_id,
                       f.display_name AS file_display_name,
                       f.total_size AS file_total_size, f.mime_type AS file_mime_type,
                       f.first_message_id AS file_first_message_id,
                       f.backing_channel_id AS file_backing_channel_id,
                       f.logical_channel_id AS file_logical_channel_id,
                       f.created_at AS file_created_at,
                       f.updated_at AS file_updated_at,
                       f.encryption_version AS file_encryption_version,
                       f.plaintext_size AS file_plaintext_size,
                       f.crypto_id AS file_crypto_id,
                       f.content_fingerprint AS file_content_fingerprint,
                       f.storage_version AS file_storage_version,
                       f.inode_id AS file_inode_id,
                       CASE WHEN ?2=1 THEN
                           (SELECT mode FROM drive_permissions
                            WHERE node_kind='file' AND node_id=f.entry_id)
                       ELSE NULL END AS effective_mode
                FROM remote_files f
                WHERE f.entry_id=?1 AND NOT EXISTS (SELECT 1 FROM pending)
             )
             ORDER BY row_kind ASC, file_storage_version DESC
             LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    stmt.bind((1, id)).map_err(|e| e.to_string())?;
    stmt.bind((2, if include_mode { 1_i64 } else { 0_i64 }))
        .map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    // The indexed FUSE getattr path must not coerce corrupt REAL/TEXT/BLOB
    // chmod values to i64. It bypasses the slower direct permission reader,
    // so enforce the same raw SQLite INTEGER storage class here.
    let mode = match stmt
        .read::<sqlite::Value, _>("effective_mode")
        .map_err(|e| e.to_string())?
    {
        sqlite::Value::Null => None,
        sqlite::Value::Integer(value) if (0..=0o777).contains(&value) => Some(value as u16),
        _ => return Err("Invalid stored Drive permissions".into()),
    };
    let entry = match stmt.read::<i64, _>("row_kind").map_err(|e| e.to_string())? {
        0 => RegularFileEntry::Pending(read_pending_record(&stmt)?),
        1 => RegularFileEntry::File(read_file_record(&stmt, "file_")?),
        _ => return Err("Invalid Drive regular-file lookup result".into()),
    };
    Ok(Some(RegularFileMetadata { entry, mode }))
}

pub(crate) fn regular_file_entry(
    conn: &sqlite::Connection,
    id: &str,
) -> Result<Option<RegularFileEntry>, String> {
    regular_file_metadata_inner(conn, id, false)
        .map(|metadata| metadata.map(|metadata| metadata.entry))
}

pub(crate) fn child_entry(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    name: &str,
) -> Result<Option<DriveChildEntry>, String> {
    record_lookup_query_step();
    let mut stmt = conn
        .prepare(
            "WITH valid_parent AS (
                SELECT 1 AS ok WHERE ?1 IS NULL
                UNION ALL
                SELECT 1 AS ok FROM drive_directories
                WHERE id=?1 AND trashed_at IS NULL
             ),
             files AS (
                SELECT e.entry_id, e.object_id AS file_id, e.directory_id, e.display_name,
                       o.plaintext_size AS total_size, NULL AS mime_type,
                       COALESCE(p.message_id,0) AS first_message_id,
                       COALESCE(p.backing_channel_id,0) AS backing_channel_id,
                       '' AS logical_channel_id, e.created_at, e.updated_at,
                       o.encryption_version, o.plaintext_size, o.crypto_id,
                       o.content_fingerprint, 2 AS storage_version, e.inode_id, 4 AS row_kind
                FROM drive_entries_v2 e
                JOIN drive_objects o ON o.object_id=e.object_id
                LEFT JOIN drive_object_parts p ON p.object_id=e.object_id AND p.part_index=0
                WHERE e.directory_id IS ?1 AND e.display_name=?2 AND e.trashed_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM drive_pending_files pending
                      WHERE pending.base_entry_id=e.entry_id
                  )
                UNION ALL
                SELECT e.entry_id, COALESCE(v.object_id, e.file_id) AS file_id,
                       COALESCE(v.directory_id, e.directory_id) AS directory_id,
                       COALESCE(v.display_name, e.display_name) AS display_name,
                       COALESCE(vo.plaintext_size, f.total_size) AS total_size,
                       CASE WHEN v.entry_id IS NOT NULL THEN NULL ELSE f.mime_type END AS mime_type,
                       COALESCE(vp.message_id, f.first_message_id) AS first_message_id,
                       COALESCE(vp.backing_channel_id, c.backing_channel_id) AS backing_channel_id,
                       CASE WHEN v.entry_id IS NOT NULL THEN '' ELSE f.logical_channel_id END
                           AS logical_channel_id,
                       COALESCE(v.created_at, f.created_at) AS created_at,
                       COALESCE(v.updated_at, e.updated_at) AS updated_at,
                       COALESCE(vo.encryption_version, e.encryption_version) AS encryption_version,
                       COALESCE(vo.plaintext_size, e.plaintext_size) AS plaintext_size,
                       COALESCE(vo.crypto_id, e.crypto_id) AS crypto_id,
                       COALESCE(vo.content_fingerprint, e.content_fingerprint) AS content_fingerprint,
                       CASE WHEN v.entry_id IS NOT NULL THEN 2 ELSE 1 END AS storage_version,
                       v.inode_id AS inode_id, 3 AS row_kind
                FROM drive_file_entries e
                JOIN logical_files f ON f.file_id=e.file_id
                JOIN logical_channels c ON c.logical_id=f.logical_channel_id
                LEFT JOIN drive_entries_v2 v
                  ON v.entry_id=e.entry_id AND v.trashed_at IS NULL
                     AND EXISTS (
                         SELECT 1 FROM drive_objects existing_object
                         WHERE existing_object.object_id=v.object_id
                     )
                LEFT JOIN drive_objects vo ON vo.object_id=v.object_id
                LEFT JOIN drive_object_parts vp
                  ON vp.object_id=v.object_id AND vp.part_index=0
                WHERE e.directory_id IS ?1 AND e.display_name=?2
                  AND e.trashed_at IS NULL AND f.status='complete'
                  AND NOT EXISTS (
                      SELECT 1 FROM drive_pending_files pending
                      WHERE pending.base_entry_id=e.entry_id
                  )
                  AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
             ),
             children AS (
                SELECT 0 AS row_kind, s.id AS id, s.directory_id AS directory_id,
                       s.display_name AS display_name, s.target AS symlink_target,
                       NULL AS parent_id, NULL AS backing_logical_channel_id,
                       s.created_at AS created_at, s.updated_at AS updated_at,
                       s.trashed_at AS trashed_at,
                       NULL AS staging_path, NULL AS size, NULL AS format_version,
                       NULL AS backing_channel_id, NULL AS base_entry_id,
                       NULL AS encryption_version, NULL AS crypto_id, NULL AS closed_at,
                       NULL AS file_entry_id, NULL AS file_file_id, NULL AS file_directory_id,
                       NULL AS file_display_name, NULL AS file_total_size, NULL AS file_mime_type,
                       NULL AS file_first_message_id, NULL AS file_backing_channel_id,
                       NULL AS file_logical_channel_id, NULL AS file_created_at,
                       NULL AS file_updated_at, NULL AS file_encryption_version,
                       NULL AS file_plaintext_size, NULL AS file_crypto_id,
                       NULL AS file_content_fingerprint, NULL AS file_storage_version,
                       NULL AS file_inode_id
                FROM drive_symlinks s
                WHERE s.directory_id IS ?1 AND s.display_name=?2 AND s.trashed_at IS NULL
                UNION ALL
                SELECT 1 AS row_kind, d.id AS id, NULL AS directory_id, d.name AS display_name,
                       NULL AS symlink_target, d.parent_id AS parent_id,
                       d.backing_logical_channel_id AS backing_logical_channel_id,
                       d.created_at AS created_at, d.updated_at AS updated_at,
                       d.trashed_at AS trashed_at,
                       NULL AS staging_path, NULL AS size, NULL AS format_version,
                       NULL AS backing_channel_id, NULL AS base_entry_id,
                       NULL AS encryption_version, NULL AS crypto_id, NULL AS closed_at,
                       NULL AS file_entry_id, NULL AS file_file_id, NULL AS file_directory_id,
                       NULL AS file_display_name, NULL AS file_total_size, NULL AS file_mime_type,
                       NULL AS file_first_message_id, NULL AS file_backing_channel_id,
                       NULL AS file_logical_channel_id, NULL AS file_created_at,
                       NULL AS file_updated_at, NULL AS file_encryption_version,
                       NULL AS file_plaintext_size, NULL AS file_crypto_id,
                       NULL AS file_content_fingerprint, NULL AS file_storage_version,
                       NULL AS file_inode_id
                FROM drive_directories d
                WHERE d.parent_id IS ?1 AND d.name=?2 AND d.trashed_at IS NULL
                UNION ALL
                SELECT 2 AS row_kind, p.id AS id, p.directory_id AS directory_id,
                       p.display_name AS display_name, NULL AS symlink_target,
                       NULL AS parent_id, NULL AS backing_logical_channel_id,
                       p.created_at AS created_at, p.updated_at AS updated_at,
                       NULL AS trashed_at, p.staging_path AS staging_path,
                       p.size AS size, p.format_version AS format_version,
                       p.backing_channel_id AS backing_channel_id,
                       p.base_entry_id AS base_entry_id,
                       p.encryption_version AS encryption_version,
                       p.crypto_id AS crypto_id, p.closed_at AS closed_at,
                       NULL AS file_entry_id, NULL AS file_file_id, NULL AS file_directory_id,
                       NULL AS file_display_name, NULL AS file_total_size, NULL AS file_mime_type,
                       NULL AS file_first_message_id, NULL AS file_backing_channel_id,
                       NULL AS file_logical_channel_id, NULL AS file_created_at,
                       NULL AS file_updated_at, NULL AS file_encryption_version,
                       NULL AS file_plaintext_size, NULL AS file_crypto_id,
                       NULL AS file_content_fingerprint, NULL AS file_storage_version,
                       NULL AS file_inode_id
                FROM drive_pending_files p
                WHERE p.directory_id IS ?1 AND p.display_name=?2
                UNION ALL
                SELECT f.row_kind AS row_kind, NULL AS id, NULL AS directory_id,
                       NULL AS display_name, NULL AS symlink_target, NULL AS parent_id,
                       NULL AS backing_logical_channel_id, NULL AS created_at,
                       NULL AS updated_at, NULL AS trashed_at,
                       NULL AS staging_path, NULL AS size, NULL AS format_version,
                       NULL AS backing_channel_id, NULL AS base_entry_id,
                       NULL AS encryption_version, NULL AS crypto_id, NULL AS closed_at,
                       f.entry_id AS file_entry_id, f.file_id AS file_file_id,
                       f.directory_id AS file_directory_id, f.display_name AS file_display_name,
                       f.total_size AS file_total_size, f.mime_type AS file_mime_type,
                       f.first_message_id AS file_first_message_id,
                       f.backing_channel_id AS file_backing_channel_id,
                       f.logical_channel_id AS file_logical_channel_id,
                       f.created_at AS file_created_at, f.updated_at AS file_updated_at,
                       f.encryption_version AS file_encryption_version,
                       f.plaintext_size AS file_plaintext_size, f.crypto_id AS file_crypto_id,
                       f.content_fingerprint AS file_content_fingerprint,
                       f.storage_version AS file_storage_version,
                       f.inode_id AS file_inode_id
                FROM files f
             )
             SELECT row_kind, id, directory_id, display_name, symlink_target,
                    parent_id, backing_logical_channel_id, created_at, updated_at, trashed_at,
                    staging_path, size, format_version, backing_channel_id, base_entry_id,
                    encryption_version, crypto_id, closed_at, file_entry_id, file_file_id,
                    file_directory_id, file_display_name, file_total_size, file_mime_type,
                    file_first_message_id, file_backing_channel_id, file_logical_channel_id,
                    file_created_at, file_updated_at, file_encryption_version,
                    file_plaintext_size, file_crypto_id, file_content_fingerprint,
                    file_storage_version, file_inode_id
             FROM children
             WHERE EXISTS (SELECT 1 FROM valid_parent)
             ORDER BY row_kind ASC
             LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    stmt.bind((1, parent)).map_err(|e| e.to_string())?;
    stmt.bind((2, name)).map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    match stmt.read::<i64, _>("row_kind").map_err(|e| e.to_string())? {
        0 => crate::drive_posix::read_link_with_target(&stmt, "symlink_target")
            .map(DriveChildEntry::Symlink)
            .map(Some),
        1 => {
            // The indexed child lookup must validate the same persisted
            // directory timestamps as direct getattr. These are projected
            // columns 7 and 8 in the shared child-entry SELECT.
            let (created_at, updated_at) = read_directory_timestamps(&stmt, 7, 8)?;
            Ok(Some(DriveChildEntry::Directory(DriveDirectoryRecord {
                id: stmt.read::<String, _>("id").map_err(|e| e.to_string())?,
                parent_id: stmt
                    .read::<Option<String>, _>("parent_id")
                    .map_err(|e| e.to_string())?,
                name: stmt
                    .read::<String, _>("display_name")
                    .map_err(|e| e.to_string())?,
                backing_logical_channel_id: stmt
                    .read::<Option<String>, _>("backing_logical_channel_id")
                    .map_err(|e| e.to_string())?,
                created_at,
                updated_at,
                trashed_at: stmt
                    .read::<Option<i64>, _>("trashed_at")
                    .map_err(|e| e.to_string())?,
            })))
        }
        2 => read_pending_record(&stmt)
            .map(DriveChildEntry::Pending)
            .map(Some),
        3 | 4 => read_file_record(&stmt, "file_")
            .map(DriveChildEntry::File)
            .map(Some),
        _ => Err("Invalid Drive child entry result".into()),
    }
}

pub(crate) fn child_with_symlink(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    name: &str,
) -> Result<Option<(DriveNodeId, bool)>, String> {
    record_lookup_query_step();
    // Keep the symlink-first lookup semantics, but resolve every namespace kind
    // with one indexed child-name query instead of probing symlinks separately.
    let mut s = conn
        .prepare(
            "WITH valid_parent AS (
                SELECT 1 AS ok WHERE ?1 IS NULL
                UNION ALL
                SELECT 1 AS ok FROM drive_directories
                WHERE id=?1 AND trashed_at IS NULL
             ),
             children AS (
                 SELECT 0 AS kind, id
                 FROM drive_symlinks
                 WHERE directory_id IS ?1 AND display_name = ?2 AND trashed_at IS NULL
                 UNION ALL
                 SELECT 1 AS kind, id
                 FROM drive_directories
                 WHERE parent_id IS ?1 AND name = ?2 AND trashed_at IS NULL
                 UNION ALL
                 SELECT 2 AS kind, id
                 FROM drive_pending_files
                 WHERE directory_id IS ?1 AND display_name = ?2
                 UNION ALL
                 SELECT 3 AS kind, e.entry_id AS id
                 FROM drive_file_entries e
                 JOIN logical_files f ON f.file_id = e.file_id
                 JOIN logical_channels c ON c.logical_id = f.logical_channel_id
                 WHERE e.directory_id IS ?1 AND e.display_name = ?2
                   AND e.trashed_at IS NULL AND f.status = 'complete'
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id = e.entry_id
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_entries_v2 shadow
                       JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                       WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                   )
                 UNION ALL
                 SELECT 4 AS kind, e.entry_id AS id
                 FROM drive_entries_v2 e
                 JOIN drive_objects o ON o.object_id = e.object_id
                 WHERE e.directory_id IS ?1 AND e.display_name = ?2
                   AND e.trashed_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id = e.entry_id
                   )
             )
             SELECT kind, id FROM children
             WHERE EXISTS (SELECT 1 FROM valid_parent)
             ORDER BY kind ASC
             LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    s.bind((1, parent)).map_err(|e| e.to_string())?;
    s.bind((2, name)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let kind = s.read::<i64, _>("kind").map_err(|e| e.to_string())?;
    let id = s.read::<String, _>("id").map_err(|e| e.to_string())?;
    let symlink = kind == 0;
    let identity = match kind {
        0 | 3 | 4 => DriveNodeId::File(id),
        1 => DriveNodeId::Directory(id),
        2 => DriveNodeId::Pending(id),
        _ => return Err("Invalid Drive child lookup result".into()),
    };
    Ok(Some((identity, symlink)))
}

pub(crate) fn child(
    conn: &sqlite::Connection,
    parent: Option<&str>,
    name: &str,
) -> Result<Option<DriveNodeId>, String> {
    let mut s=conn.prepare(
        "SELECT 0 AS kind,id FROM drive_directories WHERE parent_id IS ?1 AND name=?2 AND trashed_at IS NULL
         UNION ALL
         SELECT 1,id FROM drive_pending_files WHERE directory_id IS ?1 AND display_name=?2
         UNION ALL
         SELECT 2,e.entry_id FROM drive_file_entries e JOIN logical_files f ON f.file_id=e.file_id
         JOIN logical_channels c ON c.logical_id=f.logical_channel_id
         WHERE e.directory_id IS ?1 AND e.display_name=?2 AND e.trashed_at IS NULL AND f.status='complete'
           AND NOT EXISTS(SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id)
           AND NOT EXISTS (
                     SELECT 1 FROM drive_entries_v2 shadow
                     JOIN drive_objects shadow_object ON shadow_object.object_id = shadow.object_id
                     WHERE shadow.entry_id = e.entry_id AND shadow.trashed_at IS NOT NULL
                 )
         UNION ALL
         SELECT 2,e.entry_id FROM drive_entries_v2 e JOIN drive_objects o ON o.object_id=e.object_id
         WHERE e.directory_id IS ?1 AND e.display_name=?2 AND e.trashed_at IS NULL
           AND NOT EXISTS(SELECT 1 FROM drive_pending_files p WHERE p.base_entry_id=e.entry_id)
         LIMIT 1"
    ).map_err(|e|e.to_string())?;
    s.bind((1, parent)).map_err(|e| e.to_string())?;
    s.bind((2, name)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let id = s.read::<String, _>(1).map_err(|e| e.to_string())?;
    Ok(Some(
        match s.read::<i64, _>(0).map_err(|e| e.to_string())? {
            0 => DriveNodeId::Directory(id),
            1 => DriveNodeId::Pending(id),
            _ => DriveNodeId::File(id),
        },
    ))
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
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        init_indexes(&conn).unwrap();
        conn
    }

    fn insert_legacy_file(conn: &sqlite::Connection) {
        conn.execute(
            "INSERT INTO logical_channels
                (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES ('lookup-channel', 73, 'lookup-channel', 'owner', 1, 1, 1);
             INSERT INTO logical_files
                (file_id, logical_channel_id, first_message_id, display_name, total_size,
                 mime_type, chunk_count, whole_sha256, storage_version, created_at, status)
             VALUES ('lookup-file', 'lookup-channel', 99, 'lookup.txt', 2048,
                     'text/plain', 1, NULL, 1, 1, 'complete');
             INSERT INTO drive_file_entries
                (entry_id, file_id, directory_id, display_name, created_at, updated_at,
                 trashed_at, encryption_version, plaintext_size, crypto_id, content_fingerprint)
             VALUES ('lookup-entry', 'lookup-file', NULL, 'lookup.txt', 1, 1,
                     NULL, 0, 2048, NULL, NULL);",
        )
        .unwrap();
    }

    fn insert_v2_file(conn: &sqlite::Connection) {
        conn.execute(
            "INSERT INTO drive_objects
                (object_id, plaintext_size, encryption_version, crypto_id, content_fingerprint,
                 block_count, block_hashes_b64, created_at)
             VALUES ('lookup-v2-object', 4096, 1, 'v2-crypto', 'v2-fingerprint', 0, NULL, 2);
             INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('lookup-entry', 'lookup-v2-object', NULL, 'v2.txt', 2, 2, NULL);",
        )
        .unwrap();
    }

    #[test]
    fn native_directory_lookup_rejects_corrupt_timestamps() {
        let conn = memory_db();
        conn.execute(
            "INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at)
             VALUES ('timestamp-folder', NULL, 'Project', NULL, 10, 11)",
        )
        .unwrap();
        let valid = directory(&conn, "timestamp-folder").unwrap().unwrap();
        assert_eq!((valid.created_at, valid.updated_at), (10, 11));
        assert!(matches!(
            child_entry(&conn, None, "Project").unwrap(),
            Some(DriveChildEntry::Directory(_))
        ));

        for (case, damage) in [
            ("negative creation", "created_at=-1"),
            ("negative update", "updated_at=-1"),
            ("reversed order", "created_at=12, updated_at=11"),
            ("blob creation", "created_at=X'FF'"),
            ("real update", "updated_at=1.5"),
            ("text creation", "created_at='not-an-integer'"),
        ] {
            conn.execute(
                "UPDATE drive_directories SET created_at=10, updated_at=11
                 WHERE id='timestamp-folder'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_directories SET {damage} WHERE id='timestamp-folder'"
            ))
            .unwrap();
            assert!(
                directory(&conn, "timestamp-folder")
                    .unwrap_err()
                    .contains("Invalid Drive directory timestamp"),
                "{case} should fail closed in direct metadata lookup"
            );
            assert!(
                child_entry(&conn, None, "Project")
                    .err()
                    .expect("corrupted child should fail")
                    .contains("Invalid Drive directory timestamp"),
                "{case} should fail closed in native child lookup"
            );
        }
        conn.execute(
            "UPDATE drive_directories SET created_at=10, updated_at=11
             WHERE id='timestamp-folder'",
        )
        .unwrap();
        assert_eq!(
            directory(&conn, "timestamp-folder")
                .unwrap()
                .unwrap()
                .updated_at,
            11
        );
    }

    #[test]
    fn native_file_lookup_rejects_corrupt_legacy_and_v2_timestamps() {
        // V1 creation comes from logical_files while its update comes from
        // drive_file_entries; V2 stores both timestamps on the entry.
        for (v2, table, key, damage) in [
            (false, "logical_files", "file_id", "created_at=-1"),
            (false, "logical_files", "file_id", "created_at=X'FF'"),
            (false, "drive_file_entries", "entry_id", "updated_at=-1"),
            (false, "drive_file_entries", "entry_id", "updated_at=1.5"),
            (false, "drive_file_entries", "entry_id", "updated_at=0"),
            (true, "drive_entries_v2", "entry_id", "created_at=-1"),
            (true, "drive_entries_v2", "entry_id", "updated_at=-1"),
            (
                true,
                "drive_entries_v2",
                "entry_id",
                "created_at=3,updated_at=2",
            ),
            (true, "drive_entries_v2", "entry_id", "updated_at=X'FF'"),
            (true, "drive_entries_v2", "entry_id", "created_at='invalid'"),
        ] {
            let conn = memory_db();
            if v2 {
                insert_v2_file(&conn);
            } else {
                insert_legacy_file(&conn);
            }
            let name = if v2 { "v2.txt" } else { "lookup.txt" };
            assert!(file(&conn, "lookup-entry").unwrap().is_some());
            assert!(matches!(
                regular_file_entry(&conn, "lookup-entry").unwrap(),
                Some(RegularFileEntry::File(_))
            ));
            assert!(matches!(
                child_entry(&conn, None, name).unwrap(),
                Some(DriveChildEntry::File(_))
            ));

            let id = if table == "logical_files" {
                "lookup-file"
            } else {
                "lookup-entry"
            };
            conn.execute(format!("UPDATE {table} SET {damage} WHERE {key}='{id}'"))
                .unwrap();
            assert!(file(&conn, "lookup-entry").is_err(), "{table}: {damage}");
            assert!(
                regular_file_entry(&conn, "lookup-entry").is_err(),
                "{table}: {damage}: native file metadata"
            );
            assert!(
                child_entry(&conn, None, name).is_err(),
                "{table}: {damage}: native child metadata"
            );
        }
    }

    #[test]
    fn legacy_native_file_hot_lookups_reject_coerced_message_and_peer_ids() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        assert_eq!(
            file(&conn, "lookup-entry")
                .unwrap()
                .unwrap()
                .first_message_id,
            99
        );
        assert_eq!(
            file(&conn, "lookup-entry")
                .unwrap()
                .unwrap()
                .backing_channel_id,
            73
        );
        for (table, field, valid) in [
            ("logical_files", "first_message_id", 99),
            ("logical_channels", "backing_channel_id", 73),
        ] {
            for corrupted in ["3.5", "'not-an-integer'", "x'3132'"] {
                conn.execute(format!("UPDATE {table} SET {field}={corrupted}"))
                    .unwrap();
                assert!(
                    file(&conn, "lookup-entry").is_err(),
                    "{table}.{field}={corrupted} passed direct file lookup"
                );
                assert!(
                    regular_file_entry(&conn, "lookup-entry").is_err(),
                    "{table}.{field}={corrupted} passed regular file lookup"
                );
                assert!(
                    child_entry(&conn, None, "lookup.txt").is_err(),
                    "{table}.{field}={corrupted} passed child lookup"
                );
                conn.execute(format!("UPDATE {table} SET {field}={valid}"))
                    .unwrap();
            }
        }
        conn.execute("UPDATE logical_channels SET backing_channel_id=-10012345")
            .unwrap();
        let valid = file(&conn, "lookup-entry").unwrap().unwrap();
        assert_eq!(valid.first_message_id, 99);
        assert_eq!(valid.backing_channel_id, -10012345);
        assert!(regular_file_entry(&conn, "lookup-entry").unwrap().is_some());
        assert!(child_entry(&conn, None, "lookup.txt").unwrap().is_some());
    }

    #[test]
    fn native_file_lookup_rejects_corrupt_v2_and_legacy_file_metadata() {
        let v2 = memory_db();
        insert_v2_file(&v2);
        assert_eq!(file(&v2, "lookup-entry").unwrap().unwrap().total_size, 4096);
        assert!(matches!(
            regular_file_entry(&v2, "lookup-entry").unwrap(),
            Some(RegularFileEntry::File(_))
        ));
        assert!(matches!(
            child_entry(&v2, None, "v2.txt").unwrap(),
            Some(DriveChildEntry::File(_))
        ));

        for (case, change) in [
            ("negative object size", "plaintext_size=-1"),
            ("bad object size type", "plaintext_size=X'FF'"),
            ("unsupported encryption", "encryption_version=9"),
            ("corrupt encryption type", "encryption_version=X'FF'"),
            ("corrupt crypto identity", "crypto_id=X'FF'"),
            ("corrupt fingerprint", "content_fingerprint=X'FF'"),
        ] {
            v2.execute(
                "UPDATE drive_objects
                 SET plaintext_size=4096, encryption_version=1,
                     crypto_id='v2-crypto', content_fingerprint='v2-fingerprint'
                 WHERE object_id='lookup-v2-object'",
            )
            .unwrap();
            v2.execute(format!(
                "UPDATE drive_objects SET {change} WHERE object_id='lookup-v2-object'"
            ))
            .unwrap();
            assert!(file(&v2, "lookup-entry").is_err(), "{case}: direct lookup");
            assert!(
                regular_file_entry(&v2, "lookup-entry").is_err(),
                "{case}: native metadata lookup"
            );
            assert!(
                child_entry(&v2, None, "v2.txt").is_err(),
                "{case}: native directory child lookup"
            );
        }
        v2.execute(
            "UPDATE drive_objects
             SET plaintext_size=4096, encryption_version=1,
                 crypto_id='v2-crypto', content_fingerprint='v2-fingerprint'
             WHERE object_id='lookup-v2-object';
             UPDATE drive_entries_v2 SET directory_id=X'FF'
             WHERE entry_id='lookup-entry';",
        )
        .unwrap();
        assert!(file(&v2, "lookup-entry").is_err());
        assert!(regular_file_entry(&v2, "lookup-entry").is_err());
        v2.execute("UPDATE drive_entries_v2 SET directory_id=NULL WHERE entry_id='lookup-entry'")
            .unwrap();
        assert_eq!(file(&v2, "lookup-entry").unwrap().unwrap().total_size, 4096);

        let legacy = memory_db();
        insert_legacy_file(&legacy);
        assert_eq!(
            file(&legacy, "lookup-entry").unwrap().unwrap().total_size,
            2048
        );
        for (case, change) in [
            ("negative legacy size", "total_size=-1"),
            ("bad legacy size", "total_size=X'FF'"),
        ] {
            legacy
                .execute("UPDATE logical_files SET total_size=2048 WHERE file_id='lookup-file'")
                .unwrap();
            legacy
                .execute(format!(
                    "UPDATE logical_files SET {change} WHERE file_id='lookup-file'"
                ))
                .unwrap();
            assert!(file(&legacy, "lookup-entry").is_err(), "{case}");
            assert!(
                regular_file_entry(&legacy, "lookup-entry").is_err(),
                "{case}"
            );
        }
        legacy
            .execute("UPDATE logical_files SET total_size=2048 WHERE file_id='lookup-file'")
            .unwrap();
        for (case, change) in [
            ("negative legacy plaintext size", "plaintext_size=-1"),
            ("corrupt legacy encryption", "encryption_version=X'FF'"),
            ("unsupported legacy encryption", "encryption_version=9"),
            ("corrupt legacy crypto id", "crypto_id=X'FF'"),
            ("corrupt legacy parent", "directory_id=X'FF'"),
        ] {
            legacy
                .execute(
                    "UPDATE drive_file_entries
                 SET directory_id=NULL, plaintext_size=2048,
                     encryption_version=0, crypto_id=NULL
                 WHERE entry_id='lookup-entry'",
                )
                .unwrap();
            legacy
                .execute(format!(
                    "UPDATE drive_file_entries SET {change} WHERE entry_id='lookup-entry'"
                ))
                .unwrap();
            assert!(file(&legacy, "lookup-entry").is_err(), "{case}");
            assert!(
                regular_file_entry(&legacy, "lookup-entry").is_err(),
                "{case}"
            );
        }
        legacy
            .execute(
                "UPDATE drive_file_entries
             SET directory_id=NULL, plaintext_size=2048,
                 encryption_version=0, crypto_id=NULL
             WHERE entry_id='lookup-entry'",
            )
            .unwrap();
        assert!(file(&legacy, "lookup-entry").unwrap().is_some());
    }

    fn insert_pending(
        conn: &sqlite::Connection,
        id: &str,
        base_entry_id: Option<&str>,
        created_at: i64,
        size: i64,
    ) {
        let display_name = format!("{id}.bin");
        let staging_path = format!("/private/{id}.stage");
        let crypto_id = format!("crypto-{id}");
        let mut stmt = conn
            .prepare(
                "INSERT INTO drive_pending_files
                 (id, directory_id, display_name, staging_path, size, format_version,
                  backing_channel_id, base_entry_id, encryption_version, crypto_id,
                  closed_at, created_at, updated_at)
                 VALUES (?, NULL, ?, ?, ?, 2, 73, ?, 1, ?, ?, ?, ?)",
            )
            .unwrap();
        stmt.bind((1, id)).unwrap();
        stmt.bind((2, display_name.as_str())).unwrap();
        stmt.bind((3, staging_path.as_str())).unwrap();
        stmt.bind((4, size)).unwrap();
        stmt.bind((5, base_entry_id)).unwrap();
        stmt.bind((6, crypto_id.as_str())).unwrap();
        stmt.bind((7, Some(created_at + 1))).unwrap();
        stmt.bind((8, created_at)).unwrap();
        stmt.bind((9, created_at + 7)).unwrap();
        stmt.next().unwrap();
    }

    fn reset_pending_query_steps() {
        DRIVE_LOOKUP_QUERY_STEPS.with(|steps| steps.set(0));
    }

    fn pending_query_steps() -> usize {
        DRIVE_LOOKUP_QUERY_STEPS.with(|steps| steps.get())
    }

    #[test]
    fn indexed_symlink_child_lookup_uses_the_same_fail_closed_decoder_as_readlink() {
        let conn = memory_db();
        for (case, corruption) in [
            ("negative creation", "created_at=-1"),
            ("real creation", "created_at=1.5"),
            ("blob creation", "created_at=x'01'"),
            ("reversed update", "updated_at=9"),
            ("negative update", "updated_at=-1"),
            ("text update", "updated_at='broken'"),
            ("blob update", "updated_at=x'01'"),
            ("empty target", "target=''"),
            ("nul target", "target='good' || char(0) || 'bad'"),
            ("blob target", "target=x'676f6f64'"),
            ("oversize target", "target=printf('%04096d',0)"),
            ("empty id", "id=''"),
            ("nul id", "id='indexed' || char(0) || '-link'"),
            ("blob id", "id=x'6964'"),
        ] {
            conn.execute(
                "DELETE FROM drive_symlinks;
                 INSERT INTO drive_symlinks
                   (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
                 VALUES('indexed-link',NULL,'entry-link','target.txt',10,20,NULL);",
            )
            .unwrap();
            let Some(DriveChildEntry::Symlink(valid)) =
                child_entry(&conn, None, "entry-link").unwrap()
            else {
                panic!("a valid symlink must be discoverable through the indexed path");
            };
            assert_eq!(valid.id, "indexed-link");
            assert_eq!(valid.target, "target.txt");
            conn.execute(format!(
                "UPDATE drive_symlinks SET {corruption} WHERE id='indexed-link'"
            ))
            .unwrap();
            assert!(
                child_entry(&conn, None, "entry-link").is_err(),
                "indexed symlink child accepted {case}"
            );
        }

        conn.execute(
            "DELETE FROM drive_symlinks;
             INSERT INTO drive_symlinks
               (id,directory_id,display_name,target,created_at,updated_at,trashed_at)
             VALUES('indexed-link',NULL,'entry-link','target.txt',10,20,NULL);",
        )
        .unwrap();
        assert!(matches!(
            child_entry(&conn, None, "entry-link").unwrap(),
            Some(DriveChildEntry::Symlink(_))
        ));
    }

    #[test]
    fn child_with_symlink_combines_namespace_lookup_and_preserves_priority() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_v2_file(&conn);
        conn.execute(
            "INSERT INTO drive_symlinks
                (id, directory_id, display_name, target, created_at, updated_at, trashed_at)
             VALUES ('link-entry', NULL, 'lookup.txt', 'target.txt', 1, 1, NULL);
             INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES ('lookup-folder', NULL, 'folder', NULL, 1, 1, NULL);",
        )
        .unwrap();

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "lookup.txt").unwrap(),
            Some((DriveNodeId::File("link-entry".into()), true)),
            "symlink lookup must retain its existing priority over other namespace rows"
        );
        assert_eq!(pending_query_steps(), 1);

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "folder").unwrap(),
            Some((DriveNodeId::Directory("lookup-folder".into()), false))
        );
        assert_eq!(pending_query_steps(), 1);

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "v2.txt").unwrap(),
            Some((DriveNodeId::File("lookup-entry".into()), false))
        );
        assert_eq!(pending_query_steps(), 1);

        conn.execute("UPDATE drive_symlinks SET trashed_at = 2 WHERE id = 'link-entry'")
            .unwrap();

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "lookup.txt").unwrap(),
            Some((DriveNodeId::File("lookup-entry".into()), false)),
            "the legacy file must resolve once the higher-priority symlink is trashed"
        );
        assert_eq!(pending_query_steps(), 1);

        insert_pending(&conn, "overlay", Some("lookup-entry"), 10, 512);
        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "overlay.bin").unwrap(),
            Some((DriveNodeId::Pending("overlay".into()), false))
        );
        assert_eq!(pending_query_steps(), 1);

        reset_pending_query_steps();
        assert_eq!(child_with_symlink(&conn, None, "lookup.txt").unwrap(), None);
        assert_eq!(pending_query_steps(), 1);

        reset_pending_query_steps();
        assert_eq!(child_with_symlink(&conn, None, "missing").unwrap(), None);
        assert_eq!(pending_query_steps(), 1);

        conn.execute(
            "INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES ('dead-folder', NULL, 'dead', NULL, 1, 1, 10);
             INSERT INTO drive_symlinks
                (id, directory_id, display_name, target, created_at, updated_at, trashed_at)
             VALUES ('hidden-link', 'dead-folder', 'hidden.txt', 'target.txt', 1, 1, NULL);",
        )
        .unwrap();
        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, Some("dead-folder"), "hidden.txt").unwrap(),
            None,
            "indexed stale parent IDs must not expose children of trashed directories"
        );
        assert_eq!(pending_query_steps(), 1);
    }

    #[test]
    fn child_with_symlink_hides_legacy_alias_when_real_v2_identity_is_trashed() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_v2_file(&conn);
        conn.execute(
            "UPDATE drive_entries_v2 SET trashed_at=2, updated_at=2 WHERE entry_id='lookup-entry';",
        )
        .unwrap();

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "lookup.txt").unwrap(),
            None,
            "a trashed authoritative V2 entry must hide its historical live legacy alias"
        );
        assert_eq!(pending_query_steps(), 1);

        reset_pending_query_steps();
        assert_eq!(child_with_symlink(&conn, None, "v2.txt").unwrap(), None);
        assert_eq!(pending_query_steps(), 1);
    }

    #[test]
    fn child_with_symlink_preserves_legacy_fallback_for_orphan_v2_metadata() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('lookup-entry', 'missing-object', NULL, 'v2-orphan.txt', 2, 2, NULL);",
        )
        .unwrap();

        reset_pending_query_steps();
        assert_eq!(
            child_with_symlink(&conn, None, "lookup.txt").unwrap(),
            Some((DriveNodeId::File("lookup-entry".into()), false)),
            "an orphan V2 metadata row must not suppress a valid legacy fallback"
        );
        assert_eq!(pending_query_steps(), 1);
    }

    #[test]
    fn child_entry_resolves_all_live_namespace_kinds_in_one_statement() {
        use DriveChildEntry::{Directory, File, Pending, Symlink};

        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_v2_file(&conn);
        insert_pending(&conn, "pending-direct", None, 13, 123);
        conn.execute(
            "INSERT INTO drive_symlinks
                (id, directory_id, display_name, target, created_at, updated_at, trashed_at)
             VALUES ('link-entry', NULL, 'lookup.txt', 'target.txt', 1, 1, NULL),
                    ('hidden-link', 'dead-folder', 'hidden.txt', 'nowhere', 1, 1, NULL);
             INSERT INTO drive_directories
                (id, parent_id, name, backing_logical_channel_id, created_at, updated_at, trashed_at)
             VALUES ('lookup-folder', NULL, 'folder', NULL, 1, 1, NULL),
                    ('dead-folder', NULL, 'dead', NULL, 1, 1, 10);",
        )
        .unwrap();

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "lookup.txt").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let Symlink(link) = entry else {
            panic!("symlinks must preserve priority over same-name legacy files");
        };
        assert_eq!(link.id, "link-entry");
        assert_eq!(link.target, "target.txt");

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "folder").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let Directory(directory) = entry else {
            panic!("directory rows must resolve as directories");
        };
        assert_eq!(directory.id, "lookup-folder");
        assert_eq!(directory.name, "folder");

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "pending-direct.bin")
            .unwrap()
            .unwrap();
        assert_eq!(pending_query_steps(), 1);
        let Pending(pending) = entry else {
            panic!("pending rows must resolve as pending entries");
        };
        assert_eq!(pending.id, "pending-direct");

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "v2.txt").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let File(file) = entry else {
            panic!("V2 file rows must resolve as files");
        };
        assert_eq!(file.file_id, "lookup-v2-object");
        assert_eq!(file.storage_version, 2);

        conn.execute("UPDATE drive_symlinks SET trashed_at=2 WHERE id='link-entry'")
            .unwrap();
        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "lookup.txt").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let File(file) = entry else {
            panic!("legacy file lookup must work after the higher-priority symlink is trashed");
        };
        assert_eq!(file.file_id, "lookup-v2-object");
        assert_eq!(file.storage_version, 2);
        assert_eq!(file.display_name, "v2.txt");

        insert_pending(&conn, "overlay", Some("lookup-entry"), 20, 512);
        reset_pending_query_steps();
        assert!(child_entry(&conn, None, "lookup.txt").unwrap().is_none());
        assert_eq!(
            pending_query_steps(),
            1,
            "pending overlays must hide the base entry"
        );

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "overlay.bin").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        assert!(matches!(entry, Pending(pending) if pending.id == "overlay"));

        reset_pending_query_steps();
        assert!(child_entry(&conn, None, "hidden.txt").unwrap().is_none());
        assert_eq!(
            pending_query_steps(),
            1,
            "children under a trashed parent must stay hidden"
        );

        reset_pending_query_steps();
        assert!(child_entry(&conn, Some("missing-parent"), "anything")
            .unwrap()
            .is_none());
        assert_eq!(pending_query_steps(), 1, "missing parents must fail closed");

        reset_pending_query_steps();
        assert!(child_entry(&conn, None, "absent").unwrap().is_none());
        assert_eq!(pending_query_steps(), 1);
    }

    #[test]
    fn child_entry_falls_back_to_legacy_when_v2_object_is_missing() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        conn.execute(
            "INSERT INTO drive_entries_v2
                (entry_id, object_id, directory_id, display_name, created_at, updated_at, trashed_at)
             VALUES ('lookup-entry', 'missing-object', NULL, 'v2-orphan.txt', 2, 2, NULL);",
        )
        .unwrap();

        reset_pending_query_steps();
        let entry = child_entry(&conn, None, "lookup.txt").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let DriveChildEntry::File(file) = entry else {
            panic!("an orphaned V2 row must not replace the valid legacy file");
        };
        assert_eq!(file.entry_id, "lookup-entry");
        assert_eq!(file.file_id, "lookup-file");
        assert_eq!(file.display_name, "lookup.txt");
        assert_eq!(file.storage_version, 1);
    }

    #[test]
    fn pending_for_file_loads_the_direct_pending_row_in_one_query() {
        let conn = memory_db();
        insert_pending(&conn, "pending-direct", None, 13, 123);
        reset_pending_query_steps();

        let pending = pending_for_file(&conn, "pending-direct")
            .unwrap()
            .expect("direct pending IDs must resolve");

        assert_eq!(
            pending_query_steps(),
            1,
            "pending resolution must use one row query"
        );
        assert_eq!(pending.id, "pending-direct");
        assert_eq!(pending.directory_id, None);
        assert_eq!(pending.display_name, "pending-direct.bin");
        assert_eq!(pending.staging_path, "/private/pending-direct.stage");
        assert_eq!(pending.size, 123);
        assert_eq!(pending.format_version, 2);
        assert_eq!(pending.backing_channel_id, 73);
        assert_eq!(pending.base_entry_id, None);
        assert_eq!(pending.encryption_version, 1);
        assert_eq!(pending.crypto_id.as_deref(), Some("crypto-pending-direct"));
        assert_eq!(pending.closed_at, Some(14));
        assert_eq!(pending.created_at, 13);
        assert_eq!(pending.updated_at, 20);
    }

    #[test]
    fn pending_lookup_rejects_corrupt_timestamp_order_and_negative_fields() {
        let conn = memory_db();
        insert_pending(&conn, "pending-timestamp", None, 13, 123);
        for (field_update, detail) in [
            ("created_at=-1", "negative creation"),
            ("updated_at=-1", "negative modification"),
            ("updated_at=12", "modification before creation"),
            ("closed_at=-1", "negative close"),
            ("closed_at=21", "close after modification"),
        ] {
            conn.execute(
                "UPDATE drive_pending_files SET created_at=13, updated_at=20,
                 closed_at=14 WHERE id='pending-timestamp'",
            )
            .unwrap();
            conn.execute(format!(
                "UPDATE drive_pending_files SET {field_update} WHERE id='pending-timestamp'"
            ))
            .unwrap();
            assert!(
                pending_for_file(&conn, "pending-timestamp").is_err(),
                "{detail} must be rejected by targeted pending reads"
            );
        }
        conn.execute(
            "UPDATE drive_pending_files SET created_at=13, updated_at=20,
             closed_at=NULL WHERE id='pending-timestamp'",
        )
        .unwrap();
        assert_eq!(
            pending_for_file(&conn, "pending-timestamp")
                .unwrap()
                .unwrap()
                .closed_at,
            None,
            "an open pending file must remain valid"
        );
    }

    #[test]
    fn pending_lookup_rejects_corrupt_staging_text_without_truncating_paths() {
        for (column, value, lookup_id) in [
            ("id", "'pending-safe' || char(0) || '-alias'", "base-index"),
            (
                "directory_id",
                "'folder' || char(0) || '-alias'",
                "base-index",
            ),
            (
                "display_name",
                "'visible' || char(0) || '.bin'",
                "base-index",
            ),
            (
                "staging_path",
                "'/private/valid' || char(0) || '/hidden'",
                "base-index",
            ),
            (
                "base_entry_id",
                "'base-index' || char(0) || '-alias'",
                "pending-safe",
            ),
            ("crypto_id", "'crypto' || char(0) || '-alias'", "base-index"),
            ("staging_path", "X'2f746d702f7374616765'", "base-index"),
            ("staging_path", "''", "base-index"),
        ] {
            let conn = memory_db();
            insert_pending(&conn, "pending-safe", Some("base-index"), 13, 123);
            assert!(pending_for_file(&conn, lookup_id).unwrap().is_some());
            conn.execute(format!(
                "UPDATE drive_pending_files SET {column}={value} WHERE id='pending-safe'"
            ))
            .unwrap();
            assert!(
                pending_for_file(&conn, lookup_id).is_err(),
                "corrupt pending {column}={value} must not be coerced or truncated"
            );
        }
    }

    #[test]
    fn pending_for_file_prefers_the_oldest_copy_on_write_overlay_for_a_base_entry() {
        let conn = memory_db();
        insert_pending(&conn, "overlay-newer", Some("base-entry"), 50, 50);
        insert_pending(&conn, "overlay-older", Some("base-entry"), 20, 20);
        insert_pending(&conn, "unrelated", Some("other-entry"), 1, 1);
        reset_pending_query_steps();

        let pending = pending_for_file(&conn, "base-entry")
            .unwrap()
            .expect("base entry IDs must resolve to their pending overlay");

        assert_eq!(
            pending_query_steps(),
            1,
            "overlay resolution must use one row query"
        );
        assert_eq!(pending.id, "overlay-older");
        assert_eq!(pending.base_entry_id.as_deref(), Some("base-entry"));
        assert_eq!(pending.size, 20);
        assert_eq!(pending.created_at, 20);
    }

    #[test]
    fn pending_for_file_returns_none_for_an_unknown_identity() {
        let conn = memory_db();
        reset_pending_query_steps();

        assert!(pending_for_file(&conn, "missing-entry").unwrap().is_none());
        assert_eq!(pending_query_steps(), 1);
    }

    #[test]
    fn regular_file_entry_uses_one_query_when_no_pending_overlay_exists() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        reset_pending_query_steps();

        let entry = regular_file_entry(&conn, "lookup-entry").unwrap().unwrap();

        assert_eq!(
            pending_query_steps(),
            1,
            "resolving a normal remote file should execute one SQLite row query"
        );
        let RegularFileEntry::File(file) = entry else {
            panic!("a legacy remote entry without a pending overlay must resolve as a file");
        };
        assert_eq!(file.entry_id, "lookup-entry");
        assert_eq!(file.file_id, "lookup-file");
        assert_eq!(file.display_name, "lookup.txt");
        assert_eq!(file.total_size, 2048);
        assert_eq!(file.storage_version, 1);
        assert_eq!(file.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn regular_file_entry_prefers_oldest_pending_overlay_and_direct_pending_id() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_pending(&conn, "overlay-newer", Some("lookup-entry"), 50, 50);
        insert_pending(&conn, "overlay-older", Some("lookup-entry"), 20, 20);
        insert_pending(&conn, "pending-direct", None, 13, 123);

        reset_pending_query_steps();
        let entry = regular_file_entry(&conn, "lookup-entry").unwrap().unwrap();
        assert_eq!(pending_query_steps(), 1);
        let RegularFileEntry::Pending(pending) = entry else {
            panic!("pending COW overlays must take precedence over live remote entries");
        };
        assert_eq!(pending.id, "overlay-older");
        assert_eq!(pending.base_entry_id.as_deref(), Some("lookup-entry"));
        assert_eq!(pending.size, 20);

        reset_pending_query_steps();
        let entry = regular_file_entry(&conn, "pending-direct")
            .unwrap()
            .unwrap();
        assert_eq!(pending_query_steps(), 1);
        let RegularFileEntry::Pending(pending) = entry else {
            panic!("direct pending IDs must resolve to pending records");
        };
        assert_eq!(pending.id, "pending-direct");
        assert_eq!(pending.size, 123);
    }

    #[test]
    fn regular_file_entry_prefers_v2_over_legacy_for_the_same_entry_id() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_v2_file(&conn);
        reset_pending_query_steps();

        let entry = regular_file_entry(&conn, "lookup-entry").unwrap().unwrap();

        assert_eq!(pending_query_steps(), 1);
        let RegularFileEntry::File(file) = entry else {
            panic!("a file without an overlay must resolve to its authoritative remote row");
        };
        assert_eq!(file.entry_id, "lookup-entry");
        assert_eq!(file.file_id, "lookup-v2-object");
        assert_eq!(file.display_name, "v2.txt");
        assert_eq!(file.total_size, 4096);
        assert_eq!(file.storage_version, 2);
        assert_eq!(
            file.encryption_version,
            crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
        );
        assert_eq!(file.crypto_id.as_deref(), Some("v2-crypto"));
        assert_eq!(file.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn regular_file_entry_returns_none_for_unknown_identity_in_one_query() {
        let conn = memory_db();
        reset_pending_query_steps();

        assert!(regular_file_entry(&conn, "missing-entry")
            .unwrap()
            .is_none());
        assert_eq!(pending_query_steps(), 1);
    }

    fn set_test_permission(conn: &sqlite::Connection, kind: &str, id: &str, mode: u32) {
        crate::drive_posix::upsert_permission(
            conn,
            &crate::drive_posix::DrivePermissionRecord {
                node_kind: kind.into(),
                node_id: id.into(),
                mode,
                updated_at: 1,
            },
        )
        .unwrap();
    }

    #[test]
    fn regular_file_metadata_reads_file_mode_in_the_same_query() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        set_test_permission(&conn, "file", "lookup-entry", 0o640);
        reset_pending_query_steps();

        let metadata = regular_file_metadata(&conn, "lookup-entry")
            .unwrap()
            .unwrap();

        assert_eq!(pending_query_steps(), 1);
        assert_eq!(metadata.mode, Some(0o640));
        let RegularFileEntry::File(file) = metadata.entry else {
            panic!("legacy row without a pending overlay must resolve as a file");
        };
        assert_eq!(file.entry_id, "lookup-entry");
        assert_eq!(file.storage_version, 1);
    }

    #[test]
    fn indexed_file_getattr_rejects_coerced_sqlite_permission_modes() {
        for storage_version in [1, 2] {
            let conn = memory_db();
            insert_legacy_file(&conn);
            if storage_version == 2 {
                insert_v2_file(&conn);
            }
            set_test_permission(&conn, "file", "lookup-entry", 0o640);
            assert_eq!(
                regular_file_metadata(&conn, "lookup-entry")
                    .unwrap()
                    .unwrap()
                    .mode,
                Some(0o640)
            );
            for bad_mode in ["420.5", "'invalid-mode'", "x'01'", "-1", "512"] {
                conn.execute(format!(
                    "UPDATE drive_permissions SET mode={bad_mode}                      WHERE node_kind='file' AND node_id='lookup-entry'"
                ))
                .unwrap();
                reset_pending_query_steps();
                assert!(
                    regular_file_metadata(&conn, "lookup-entry")
                        .err()
                        .expect("corrupt chmod must reject native getattr")
                        .contains("Invalid stored Drive permissions"),
                    "V{storage_version} indexed getattr accepted mode={bad_mode}"
                );
                assert_eq!(
                    pending_query_steps(),
                    1,
                    "permission check must not add a separate FUSE lookup query"
                );
                // Ordinary identity lookup doesn't request a mode and still
                // resolves a valid file independently of chmod corruption.
                assert!(matches!(
                    regular_file_entry(&conn, "lookup-entry").unwrap(),
                    Some(RegularFileEntry::File(_))
                ));
                conn.execute(
                    "UPDATE drive_permissions SET mode=416                      WHERE node_kind='file' AND node_id='lookup-entry'",
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn indexed_pending_getattr_rejects_invalid_overlay_and_fallback_modes() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_pending(&conn, "overlay", Some("lookup-entry"), 10, 512);
        set_test_permission(&conn, "file", "lookup-entry", 0o600);
        set_test_permission(&conn, "pending", "overlay", 0o640);

        for (kind, id) in [("pending", "overlay"), ("file", "lookup-entry")] {
            if kind == "file" {
                conn.execute(
                    "DELETE FROM drive_permissions                      WHERE node_kind='pending' AND node_id='overlay'",
                )
                .unwrap();
            }
            for bad_mode in ["420.5", "'invalid-mode'", "x'01'"] {
                conn.execute(format!(
                    "UPDATE drive_permissions SET mode={bad_mode}                      WHERE node_kind='{kind}' AND node_id='{id}'"
                ))
                .unwrap();
                reset_pending_query_steps();
                assert!(
                    regular_file_metadata(&conn, "lookup-entry")
                        .err()
                        .expect("corrupt chmod must reject pending getattr")
                        .contains("Invalid stored Drive permissions"),
                    "pending overlay indexed getattr accepted {kind} mode={bad_mode}"
                );
                assert_eq!(pending_query_steps(), 1);
                conn.execute(format!(
                    "UPDATE drive_permissions SET mode=384                      WHERE node_kind='{kind}' AND node_id='{id}'"
                ))
                .unwrap();
            }
        }
        assert_eq!(
            regular_file_metadata(&conn, "lookup-entry")
                .unwrap()
                .unwrap()
                .mode,
            Some(0o600)
        );
    }

    #[test]
    fn regular_file_metadata_uses_pending_mode_then_base_file_mode() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_pending(&conn, "overlay", Some("lookup-entry"), 10, 512);
        set_test_permission(&conn, "file", "lookup-entry", 0o600);
        set_test_permission(&conn, "pending", "overlay", 0o640);
        reset_pending_query_steps();

        let metadata = regular_file_metadata(&conn, "lookup-entry")
            .unwrap()
            .unwrap();

        assert_eq!(pending_query_steps(), 1);
        assert_eq!(metadata.mode, Some(0o640));
        let RegularFileEntry::Pending(pending) = metadata.entry else {
            panic!("the COW overlay must remain authoritative for the indexed identity");
        };
        assert_eq!(pending.id, "overlay");

        conn.execute(
            "DELETE FROM drive_permissions WHERE node_kind='pending' AND node_id='overlay'",
        )
        .unwrap();
        reset_pending_query_steps();
        let metadata = regular_file_metadata(&conn, "lookup-entry")
            .unwrap()
            .unwrap();
        assert_eq!(pending_query_steps(), 1);
        assert_eq!(metadata.mode, Some(0o600));
        assert!(matches!(metadata.entry, RegularFileEntry::Pending(_)));
    }

    #[test]
    fn regular_file_metadata_keeps_v2_identity_and_permission_precedence() {
        let conn = memory_db();
        insert_legacy_file(&conn);
        insert_v2_file(&conn);
        set_test_permission(&conn, "file", "lookup-entry", 0o604);
        reset_pending_query_steps();

        let metadata = regular_file_metadata(&conn, "lookup-entry")
            .unwrap()
            .unwrap();

        assert_eq!(pending_query_steps(), 1);
        assert_eq!(metadata.mode, Some(0o604));
        let RegularFileEntry::File(file) = metadata.entry else {
            panic!("the V2 row must win over its legacy row");
        };
        assert_eq!(file.file_id, "lookup-v2-object");
        assert_eq!(file.storage_version, 2);
    }

    /// Direct SQLite-side diagnostic for indexed FUSE getattr resolution.
    /// Deliberately NOT a FUSE/local SSD-relative timing or acceptance limit.
    #[test]
    fn profile_indexed_getattr_for_large_pending_file_working_sets() {
        use std::time::Instant;
        let conn = memory_db();
        let mut previous = 0_usize;
        for count in [64_usize, 512, 1024] {
            conn.execute("BEGIN IMMEDIATE").unwrap();
            for index in previous..count {
                insert_pending(&conn, &format!("profile-{index:05}"), None, 1, 256);
            }
            conn.execute("COMMIT").unwrap();
            previous = count;
            let ids: Vec<_> = (0..count)
                .map(|index| format!("profile-{index:05}"))
                .collect();
            let mut samples_us = Vec::new();
            for _ in 0..5 {
                reset_pending_query_steps();
                let started = Instant::now();
                let mut bytes_total = 0_u64;
                for id in &ids {
                    let resolved = regular_file_metadata(&conn, id)
                        .unwrap()
                        .expect("indexed pending file must resolve");
                    let RegularFileEntry::Pending(pending) = resolved.entry else {
                        panic!("pending data must not resolve as remote file");
                    };
                    assert_eq!(pending.id, *id);
                    bytes_total += pending.size;
                }
                samples_us.push(started.elapsed().as_micros());
                assert_eq!(bytes_total, count as u64 * 256);
                assert_eq!(pending_query_steps(), count);
            }
            samples_us.sort_unstable();
            eprintln!(
                "diagnostic_only_indexed_getattr_pending files={count} median_us={} min_us={} max_us={} scope=sqlite_no_fuse",
                samples_us[2], samples_us[0], samples_us[4]
            );
        }
    }

    #[test]
    fn regular_file_metadata_returns_none_for_unknown_identity_in_one_query() {
        let conn = memory_db();
        reset_pending_query_steps();

        assert!(regular_file_metadata(&conn, "missing-entry")
            .unwrap()
            .is_none());
        assert_eq!(pending_query_steps(), 1);
    }
}
