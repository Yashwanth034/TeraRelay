//! Persistent, device-local undo journal for successfully committed Drive bulk renames/moves.
//! Entries are retained only in the same SQLite DB as metadata, never synced to Telegram.
use super::{DriveBulkChange, DriveBulkRequest};
use crate::commands::drive_metadata::{validate_drive_name, DriveSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_HISTORY: usize = 24;
const MAX_BATCH: usize = 100;
const MAX_JSON_BYTES: usize = 180_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BulkUndoFile {
    pub entry_id: String,
    pub previous_name: String,
    pub previous_directory_id: Option<String>,
    pub current_name: String,
    pub current_directory_id: Option<String>,
    pub file_id: String,
    pub storage_version: u32,
    pub size: u64,
    pub content_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BulkUndoHistory {
    pub id: String,
    pub created_at: i64,
    pub applied_revision: i64,
    pub undone_at: Option<i64>,
    pub files: Vec<BulkUndoFile>,
}

fn local_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_local_bulk_undo (
            id TEXT PRIMARY KEY NOT NULL,
            created_at INTEGER NOT NULL,
            applied_revision INTEGER NOT NULL,
            files_json TEXT NOT NULL,
            undone_at INTEGER);",
    )
    .map_err(|e| e.to_string())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn valid_batch_id(id: &str) -> bool {
    id.len() == 37 && id.starts_with("bulk-") && id[5..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_folder(id: &Option<String>) -> bool {
    id.as_deref().is_none_or(valid_id)
}

fn decode_files(value: &str) -> Result<Vec<BulkUndoFile>, String> {
    if value.len() > MAX_JSON_BYTES {
        return Err("Bulk undo journal exceeds safety limit.".into());
    }
    let files: Vec<BulkUndoFile> = serde_json::from_str(value)
        .map_err(|_| "Saved bulk undo history is damaged; no files were changed.")?;
    if files.is_empty() || files.len() > MAX_BATCH {
        return Err("Invalid saved bulk undo batch size.".into());
    }
    let mut seen = HashSet::new();
    for file in &files {
        if !valid_id(&file.entry_id)
            || !valid_id(&file.file_id)
            || !valid_folder(&file.previous_directory_id)
            || !valid_folder(&file.current_directory_id)
            || !seen.insert(&file.entry_id)
            || validate_drive_name(&file.previous_name).is_err()
            || validate_drive_name(&file.current_name).is_err()
            || !matches!(file.storage_version, 1 | 2)
            || file.content_fingerprint.as_ref().is_some_and(|hash| {
                hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || (file.previous_name == file.current_name
                && file.previous_directory_id == file.current_directory_id)
        {
            return Err("Saved bulk undo metadata is invalid; no files were changed.".into());
        }
    }
    Ok(files)
}

pub(super) fn record_applied(
    conn: &sqlite::Connection,
    request: &DriveBulkRequest,
    snapshot: &DriveSnapshot,
) -> Result<(), String> {
    local_schema(conn)?;
    let mut files = Vec::with_capacity(request.changes.len());
    for change in &request.changes {
        let result = snapshot
            .files
            .iter()
            .find(|file| file.entry_id == change.entry_id)
            .ok_or("Bulk result vanished before undo history was saved.")?;
        if result.display_name != change.new_name || result.directory_id != change.new_directory_id
        {
            return Err(
                "Bulk result no longer matches undo history; transaction cancelled.".into(),
            );
        }
        files.push(BulkUndoFile {
            entry_id: change.entry_id.clone(),
            previous_name: change.expected_name.clone(),
            previous_directory_id: change.expected_directory_id.clone(),
            current_name: change.new_name.clone(),
            current_directory_id: change.new_directory_id.clone(),
            file_id: result.file_id.clone(),
            storage_version: result.storage_version,
            size: result.plaintext_size.unwrap_or(result.total_size),
            content_fingerprint: result.content_fingerprint.clone(),
        });
    }
    let json = serde_json::to_string(&files).map_err(|e| e.to_string())?;
    decode_files(&json)?;
    let id = format!("bulk-{:032x}", rand::random::<u128>());
    let now = chrono::Utc::now().timestamp_millis();
    let mut insert = conn
        .prepare(
            "INSERT INTO drive_local_bulk_undo(id,created_at,applied_revision,files_json)
         VALUES(?,?,?,?)",
        )
        .map_err(|e| e.to_string())?;
    insert.bind((1, id.as_str())).map_err(|e| e.to_string())?;
    insert.bind((2, now)).map_err(|e| e.to_string())?;
    insert
        .bind((3, snapshot.revision))
        .map_err(|e| e.to_string())?;
    insert.bind((4, json.as_str())).map_err(|e| e.to_string())?;
    insert.next().map_err(|e| e.to_string())?;
    drop(insert);
    conn.execute(format!(
        "DELETE FROM drive_local_bulk_undo
         WHERE id NOT IN (
            SELECT id FROM drive_local_bulk_undo
            ORDER BY created_at DESC,id DESC LIMIT {MAX_HISTORY}
         )"
    ))
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn validate_row(
    id: String,
    created_at: i64,
    applied_revision: i64,
    undone_at: Option<i64>,
    json: String,
) -> Result<BulkUndoHistory, String> {
    if !valid_batch_id(&id)
        || created_at <= 0
        || applied_revision < 0
        || undone_at.is_some_and(|time| time < created_at)
    {
        return Err("Stored bulk undo history has invalid metadata.".into());
    }
    Ok(BulkUndoHistory {
        id,
        created_at,
        applied_revision,
        undone_at,
        files: decode_files(&json)?,
    })
}

pub(super) fn list(conn: &sqlite::Connection) -> Result<Vec<BulkUndoHistory>, String> {
    local_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT id,created_at,applied_revision,undone_at,files_json
         FROM drive_local_bulk_undo
         ORDER BY created_at DESC,id DESC LIMIT 25",
        )
        .map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    while stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if result.len() == MAX_HISTORY {
            return Err("Undo history exceeded retention cap; no operation attempted.".into());
        }
        result.push(validate_row(
            stmt.read::<String, _>(0).map_err(|e| e.to_string())?,
            stmt.read::<i64, _>(1).map_err(|e| e.to_string())?,
            stmt.read::<i64, _>(2).map_err(|e| e.to_string())?,
            stmt.read::<Option<i64>, _>(3).map_err(|e| e.to_string())?,
            stmt.read::<String, _>(4).map_err(|e| e.to_string())?,
        )?);
    }
    Ok(result)
}

pub(super) fn load(conn: &sqlite::Connection, batch_id: &str) -> Result<BulkUndoHistory, String> {
    if !valid_batch_id(batch_id) {
        return Err("Invalid bulk undo batch identity.".into());
    }
    let records = list(conn)?;
    records
        .into_iter()
        .find(|row| row.id == batch_id)
        .ok_or("Bulk undo history is unavailable or expired.".into())
}

pub(super) fn inverse(
    snapshot: &DriveSnapshot,
    history: &BulkUndoHistory,
) -> Result<DriveBulkRequest, String> {
    if history.undone_at.is_some() {
        return Err("This bulk operation was already undone.".into());
    }
    let mut changes = Vec::with_capacity(history.files.len());
    for item in &history.files {
        if snapshot
            .pending
            .iter()
            .any(|pending| pending.base_entry_id.as_deref() == Some(item.entry_id.as_str()))
        {
            return Err("A file in this batch has an unfinished copy-on-write replacement; undo is unavailable until recovery finishes.".into());
        }
        let file = snapshot
            .files
            .iter()
            .find(|file| file.entry_id == item.entry_id)
            .ok_or("A source file is missing or incomplete. Undo is unavailable.")?;
        if file.display_name != item.current_name
            || file.directory_id != item.current_directory_id
            || file.file_id != item.file_id
            || file.storage_version != item.storage_version
            || file.plaintext_size.unwrap_or(file.total_size) != item.size
            || file.content_fingerprint != item.content_fingerprint
        {
            return Err(format!(
                "File '{}' has changed since this batch. Undo will not overwrite it.",
                item.current_name
            ));
        }
        changes.push(DriveBulkChange {
            entry_id: item.entry_id.clone(),
            expected_name: item.current_name.clone(),
            expected_directory_id: item.current_directory_id.clone(),
            new_name: item.previous_name.clone(),
            new_directory_id: item.previous_directory_id.clone(),
        });
    }
    Ok(DriveBulkRequest {
        revision: snapshot.revision,
        changes,
    })
}

pub(super) fn mark_undone(conn: &sqlite::Connection, batch_id: &str) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut stmt = conn
        .prepare(
            "UPDATE drive_local_bulk_undo SET undone_at=?
         WHERE id=? AND undone_at IS NULL",
        )
        .map_err(|e| e.to_string())?;
    stmt.bind((1, now)).map_err(|e| e.to_string())?;
    stmt.bind((2, batch_id)).map_err(|e| e.to_string())?;
    stmt.next().map_err(|e| e.to_string())?;
    // History completion is part of the undo transaction. A trigger that
    // silently ignores this UPDATE must cancel the entire batch reversal.
    drop(stmt);
    let mut count = conn
        .prepare("SELECT changes()")
        .map_err(|e| e.to_string())?;
    if count.next().map_err(|e| e.to_string())? != sqlite::State::Row
        || count.read::<i64, _>(0).map_err(|e| e.to_string())? != 1
    {
        return Err(
            "Undo history completion was not recorded; all file changes will roll back.".into(),
        );
    }
    Ok(())
}
