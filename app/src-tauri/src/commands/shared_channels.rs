//! Logical TeraRelay channels backed by an existing owned Telegram channel.
//! A negative, JS-safe presentation ID remains distinct from the positive
//! Telegram peer ID. No operation may delete/rename the physical peer via an alias.
use crate::commands::TelegramState;
use crate::db::DbConnection;
use crate::models::FolderMetadata;
use serde::{Deserialize, Serialize};
use sqlite;
use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};
use tauri::State;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedChannelRecord {
    pub channel_id: i64,
    pub logical_id: String,
    pub storage_channel_id: i64,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub deleted_at: Option<i64>,
}
static ALIASES: OnceLock<RwLock<HashMap<i64, i64>>> = OnceLock::new();

fn aliases() -> &'static RwLock<HashMap<i64, i64>> {
    ALIASES.get_or_init(|| RwLock::new(HashMap::new()))
}
/// Drop account-specific routing on logout. The next account must bootstrap
/// its own verified channels before any shared view can resolve.
pub fn clear_aliases() {
    if let Ok(mut routes) = aliases().write() {
        routes.clear();
    }
}
pub fn alias_for_peer(id: i64) -> Result<i64, String> {
    if id >= 0 {
        return Ok(id);
    }
    aliases()
        .read()
        .map_err(|_| "Shared channel alias lock failed".to_string())?
        .get(&id)
        .copied()
        .ok_or_else(|| {
            "Shared channel is unavailable. Synchronize channels before continuing.".into()
        })
}
pub fn refresh_aliases(conn: &sqlite::Connection) -> Result<(), String> {
    init_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT channel_id, storage_channel_id FROM shared_channel_views WHERE deleted_at IS NULL"
    ).map_err(|e: sqlite::Error| e.to_string())?;
    let mut values = HashMap::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        values.insert(
            stmt.read::<i64, _>(0).map_err(|e| e.to_string())?,
            stmt.read::<i64, _>(1).map_err(|e| e.to_string())?,
        );
    }
    *aliases()
        .write()
        .map_err(|_| "Shared channel alias lock failed".to_string())? = values;
    Ok(())
}
pub fn init_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS shared_channel_views (
            channel_id INTEGER PRIMARY KEY CHECK(channel_id < 0),
            logical_id TEXT NOT NULL UNIQUE,
            storage_channel_id INTEGER NOT NULL CHECK(storage_channel_id > 0),
            name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            deleted_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_shared_channel_storage
          ON shared_channel_views(storage_channel_id) WHERE deleted_at IS NULL;",
    )
    .map_err(|e: sqlite::Error| e.to_string())
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn validate_record(r: &SharedChannelRecord) -> Result<(), String> {
    if r.channel_id >= 0
        || r.channel_id < -(1_i64 << 52)
        || r.storage_channel_id <= 0
        || !valid_id(&r.logical_id)
        || r.name.trim() != r.name
        || r.name.is_empty()
        || r.name.len() > 128
        || r.name.chars().any(|c| c.is_control())
        || r.created_at <= 0
        || r.updated_at < r.created_at
        || r.deleted_at
            .is_some_and(|t| t < r.created_at || t > r.updated_at)
    {
        return Err("Invalid shared-channel metadata".into());
    }
    Ok(())
}
pub fn validate_records(records: &[SharedChannelRecord]) -> Result<(), String> {
    if records.len() > 5_000 {
        return Err("Too many shared channels in metadata".into());
    }
    let mut ids = HashSet::new();
    let mut logical_ids = HashSet::new();
    for r in records {
        validate_record(r)?;
        if !ids.insert(r.channel_id) || !logical_ids.insert(&r.logical_id) {
            return Err("Duplicate shared-channel identity in metadata".into());
        }
    }
    Ok(())
}
fn query_record(
    conn: &sqlite::Connection,
    channel_id: i64,
) -> Result<Option<SharedChannelRecord>, String> {
    init_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT channel_id, logical_id, storage_channel_id, name, created_at, updated_at, deleted_at
         FROM shared_channel_views WHERE channel_id = ?"
    ).map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if stmt.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    Ok(Some(SharedChannelRecord {
        channel_id: stmt.read::<i64, _>(0).map_err(|e| e.to_string())?,
        logical_id: stmt.read::<String, _>(1).map_err(|e| e.to_string())?,
        storage_channel_id: stmt.read::<i64, _>(2).map_err(|e| e.to_string())?,
        name: stmt.read::<String, _>(3).map_err(|e| e.to_string())?,
        created_at: stmt.read::<i64, _>(4).map_err(|e| e.to_string())?,
        updated_at: stmt.read::<i64, _>(5).map_err(|e| e.to_string())?,
        deleted_at: stmt.read::<Option<i64>, _>(6).map_err(|e| e.to_string())?,
    }))
}
pub fn record(conn: &sqlite::Connection, id: i64) -> Result<Option<SharedChannelRecord>, String> {
    if id >= 0 {
        return Ok(None);
    }
    query_record(conn, id)
}
pub fn is_shared(conn: &sqlite::Connection, id: Option<i64>) -> Result<bool, String> {
    match id {
        Some(id) if id < 0 => Ok(record(conn, id)?.is_some_and(|r| r.deleted_at.is_none())),
        _ => Ok(false),
    }
}
pub fn physical_id(conn: &sqlite::Connection, id: Option<i64>) -> Result<Option<i64>, String> {
    match id {
        Some(id) if id < 0 => {
            let r = record(conn, id)?.ok_or("Shared channel was not found")?;
            if r.deleted_at.is_some() {
                return Err("Shared channel has been removed".into());
            }
            Ok(Some(r.storage_channel_id))
        }
        _ => Ok(id),
    }
}
pub fn logical_id_in_storage(
    conn: &sqlite::Connection,
    logical_id: &str,
    physical: i64,
) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM shared_channel_views
         WHERE logical_id = ? AND storage_channel_id = ? AND deleted_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_id)).map_err(|e| e.to_string())?;
    stmt.bind((2, physical)).map_err(|e| e.to_string())?;
    Ok(stmt.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row)
}
pub fn records(conn: &sqlite::Connection) -> Result<Vec<SharedChannelRecord>, String> {
    init_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT channel_id FROM shared_channel_views ORDER BY channel_id")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut ids = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        ids.push(stmt.read::<i64, _>(0).map_err(|e| e.to_string())?);
    }
    drop(stmt);
    ids.into_iter()
        .map(|id| query_record(conn, id)?.ok_or("Missing shared-channel row".into()))
        .collect()
}
pub fn choose_newer(
    a: SharedChannelRecord,
    b: SharedChannelRecord,
) -> Result<SharedChannelRecord, String> {
    if a.channel_id != b.channel_id
        || a.logical_id != b.logical_id
        || a.storage_channel_id != b.storage_channel_id
        || a.created_at != b.created_at
    {
        return Err("Conflicting shared-channel identity. Neither record was changed.".into());
    }
    if a.updated_at != b.updated_at {
        return Ok(if a.updated_at > b.updated_at { a } else { b });
    }
    if a.deleted_at.is_some() != b.deleted_at.is_some() {
        return Ok(if a.deleted_at.is_some() { a } else { b });
    }
    Ok(
        if serde_json::to_vec(&a).map_err(|e| e.to_string())?
            >= serde_json::to_vec(&b).map_err(|e| e.to_string())?
        {
            a
        } else {
            b
        },
    )
}
/// Existing local file records are never deleted while applying a remote view tombstone.
pub fn apply_record(conn: &sqlite::Connection, r: &SharedChannelRecord) -> Result<(), String> {
    validate_record(r)?;
    if let Some(existing) = query_record(conn, r.channel_id)? {
        if existing.logical_id != r.logical_id
            || existing.storage_channel_id != r.storage_channel_id
            || existing.created_at != r.created_at
        {
            return Err("Shared channel already belongs to different storage".into());
        }
    }
    // A logical ID must never be rebound to a different visible folder ID.
    let mut existing_logical = conn
        .prepare("SELECT backing_channel_id FROM logical_channels WHERE logical_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    existing_logical
        .bind((1, r.logical_id.as_str()))
        .map_err(|e| e.to_string())?;
    if existing_logical
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
        == sqlite::State::Row
        && existing_logical
            .read::<i64, _>(0)
            .map_err(|e| e.to_string())?
            != r.channel_id
    {
        return Err("Shared-channel logical identity conflicts with another channel".into());
    }
    drop(existing_logical);
    let mut stmt = conn
        .prepare(
            "INSERT INTO shared_channel_views
         (channel_id,logical_id,storage_channel_id,name,created_at,updated_at,deleted_at)
         VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(channel_id) DO UPDATE SET
           name=excluded.name, updated_at=excluded.updated_at, deleted_at=excluded.deleted_at",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, r.channel_id)).map_err(|e| e.to_string())?;
    stmt.bind((2, r.logical_id.as_str()))
        .map_err(|e| e.to_string())?;
    stmt.bind((3, r.storage_channel_id))
        .map_err(|e| e.to_string())?;
    stmt.bind((4, r.name.as_str())).map_err(|e| e.to_string())?;
    stmt.bind((5, r.created_at)).map_err(|e| e.to_string())?;
    stmt.bind((6, r.updated_at)).map_err(|e| e.to_string())?;
    stmt.bind((7, r.deleted_at)).map_err(|e| e.to_string())?;
    stmt.next().map_err(|e| e.to_string())?;
    drop(stmt);
    let mut logical = conn
        .prepare(
            "INSERT INTO logical_channels
         (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
         VALUES (?,?,?, ?, 1,?,?)
         ON CONFLICT(logical_id) DO UPDATE SET name=excluded.name, role=excluded.role",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical
        .bind((1, r.logical_id.as_str()))
        .map_err(|e| e.to_string())?;
    logical.bind((2, r.channel_id)).map_err(|e| e.to_string())?;
    logical
        .bind((3, r.name.as_str()))
        .map_err(|e| e.to_string())?;
    logical
        .bind((
            4,
            if r.deleted_at.is_some() {
                "shared_deleted"
            } else {
                "owner"
            },
        ))
        .map_err(|e| e.to_string())?;
    logical.bind((5, r.created_at)).map_err(|e| e.to_string())?;
    logical.bind((6, r.created_at)).map_err(|e| e.to_string())?;
    logical.next().map_err(|e| e.to_string())?;
    drop(logical);
    if r.deleted_at.is_some() {
        let mut del = conn
            .prepare("DELETE FROM folder_metadata WHERE channel_id = ?")
            .map_err(|e| e.to_string())?;
        del.bind((1, r.channel_id)).map_err(|e| e.to_string())?;
        del.next().map_err(|e| e.to_string())?;
    } else {
        let mut stmt = conn.prepare(
            "INSERT INTO folder_metadata(channel_id,name,username,is_public,display_order,group_id)
             VALUES(?,?,NULL,0,(SELECT COALESCE(MAX(display_order),0)+1 FROM folder_metadata),NULL)
             ON CONFLICT(channel_id) DO UPDATE SET name=excluded.name"
        ).map_err(|e| e.to_string())?;
        stmt.bind((1, r.channel_id)).map_err(|e| e.to_string())?;
        stmt.bind((2, r.name.as_str())).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn apply_records(
    conn: &sqlite::Connection,
    incoming: &[SharedChannelRecord],
) -> Result<(), String> {
    init_schema(conn)?;
    validate_records(incoming)?;
    for remote in incoming {
        let merged = match query_record(conn, remote.channel_id)? {
            Some(local) => choose_newer(local, remote.clone())?,
            None => remote.clone(),
        };
        apply_record(conn, &merged)?;
    }
    Ok(())
}

/// Before removing a shared view, inspect authoritative Telegram control
/// documents, not only local SQLite (which can be empty on a new device).
/// A transient network failure or incomplete scan refuses removal safely.
pub async fn ensure_remote_channel_empty(
    client: &grammers_client::Client,
    peer: &grammers_client::types::Peer,
    logical_id: &str,
) -> Result<(), String> {
    use grammers_client::types::Media;
    const SCAN_LIMIT: usize = 50_000;
    let mut messages = client.iter_messages(peer).limit(SCAN_LIMIT + 1);
    let mut checked = HashSet::new();
    let mut count = 0usize;
    while let Some(message) = messages.next().await.map_err(|e| {
        format!("Could not check Telegram storage before removing shared channel: {e}")
    })? {
        count += 1;
        if count > SCAN_LIMIT {
            return Err(
                "Shared-storage history is too large to verify as empty. Channel not removed."
                    .into(),
            );
        }
        let Some(Media::Document(document)) = message.media() else {
            continue;
        };
        let Some(file_id) =
            crate::commands::logical_files::manifest_file_id(document.name(), message.text())
        else {
            continue;
        };
        if checked.contains(&file_id) {
            continue;
        }
        let manifest = crate::commands::logical_files::download_manifest(
            client, &Media::Document(document), Some(&file_id), None
        ).await.map_err(|e| format!(
            "Could not verify a Telegram file manifest before removing channel; no deletion occurred: {e}"
        ))?;
        checked.insert(file_id);
        if manifest.logical_channel_id == logical_id {
            return Err("Shared channel still contains files on Telegram. Move or download its files first; storage was not deleted.".into());
        }
    }
    Ok(())
}

fn timestamp() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn random_logical_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[tauri::command]
pub async fn cmd_create_shared_folder(
    name: String,
    storage_folder_id: i64,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<FolderMetadata, String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 128 || name.chars().any(|c| c.is_control()) {
        return Err("Channel name must contain 1–128 printable characters".into());
    }
    if storage_folder_id <= 0 {
        return Err("Choose an existing dedicated Telegram channel".into());
    }
    // Only the owner of the real Telegram backing channel can create shared views.
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or("Telegram is not connected")?;
    let peer =
        crate::commands::utils::resolve_peer(&client, Some(storage_folder_id), &state.peer_cache)
            .await?;
    match peer {
        grammers_client::types::Peer::Channel(c) if c.raw.creator => {}
        _ => return Err("Shared storage requires an owned Telegram channel".into()),
    }
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    crate::commands::logical_channels::require_owner_role(&conn, storage_folder_id)?;
    let mut stmt = conn
        .prepare("SELECT 1 FROM folder_metadata WHERE channel_id=? AND channel_id>0")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, storage_folder_id))
        .map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Err("Backing Telegram channel is not registered in TeraRelay".into());
    }
    drop(stmt);
    let mut duplicates = conn
        .prepare(
            "SELECT 1 FROM shared_channel_views
         WHERE storage_channel_id=? AND name=? COLLATE NOCASE AND deleted_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    duplicates
        .bind((1, storage_folder_id))
        .map_err(|e| e.to_string())?;
    duplicates
        .bind((2, name.as_str()))
        .map_err(|e| e.to_string())?;
    if duplicates
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
        == sqlite::State::Row
    {
        return Err(
            "A shared channel with this name already exists in the selected storage".into(),
        );
    }
    drop(duplicates);
    let mut rec;
    loop {
        let raw = rand::random::<u64>() & ((1_u64 << 52) - 1);
        if raw == 0 {
            continue;
        }
        let id = -(raw as i64);
        if query_record(&conn, id)?.is_none() {
            rec = SharedChannelRecord {
                channel_id: id,
                logical_id: random_logical_id(),
                storage_channel_id: storage_folder_id,
                name: name.clone(),
                created_at: timestamp(),
                updated_at: timestamp(),
                deleted_at: None,
            };
            break;
        }
    }
    rec.updated_at = rec.created_at;
    conn.execute("SAVEPOINT create_shared_channel")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        apply_record(&conn, &rec)?;
        crate::commands::drive_metadata::mark_drive_dirty(&conn)?;
        Ok::<(), String>(())
    })();
    crate::drive_posix::finish_savepoint(&conn, "create_shared_channel", result)?;
    refresh_aliases(&conn)?;
    Ok(FolderMetadata {
        id: rec.channel_id,
        parent_id: None,
        name: rec.name,
        username: None,
        is_public: false,
        group_id: None,
        display_order: i32::MAX,
        role: Some("owner".into()),
    })
}
pub fn rename_shared(
    conn: &sqlite::Connection,
    channel_id: i64,
    new_name: &str,
) -> Result<(), String> {
    let mut r = record(conn, channel_id)?.ok_or("Unknown shared channel")?;
    if r.deleted_at.is_some() {
        return Err("Shared channel has been removed".into());
    }
    let value = new_name.trim();
    if value.is_empty() || value.len() > 128 || value.chars().any(|c| c.is_control()) {
        return Err("Invalid shared channel name".into());
    }
    let mut duplicate = conn
        .prepare(
            "SELECT 1 FROM shared_channel_views
         WHERE storage_channel_id = ? AND name = ? COLLATE NOCASE
           AND channel_id != ? AND deleted_at IS NULL LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    duplicate
        .bind((1, r.storage_channel_id))
        .map_err(|e| e.to_string())?;
    duplicate.bind((2, value)).map_err(|e| e.to_string())?;
    duplicate.bind((3, channel_id)).map_err(|e| e.to_string())?;
    if duplicate.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
        return Err(
            "Another shared channel with this name already exists in the selected storage".into(),
        );
    }
    drop(duplicate);
    r.name = value.into();
    r.updated_at = timestamp().max(r.updated_at + 1);
    conn.execute("SAVEPOINT rename_shared_channel")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        apply_record(conn, &r)?;
        crate::commands::drive_metadata::mark_drive_dirty(conn)?;
        Ok::<(), String>(())
    })();
    crate::drive_posix::finish_savepoint(conn, "rename_shared_channel", result)
}
/// Never hide a mounted Drive folder if it still contains user data. Empty
/// imported channel directories are tombstoned (not physically deleted), so
/// other devices receive the same removal through normal Drive sync.
fn tombstone_empty_imported_folder(
    conn: &sqlite::Connection,
    logical_id: &str,
    when: i64,
) -> Result<(), String> {
    let mut table = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='drive_directories'")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if table.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Ok(());
    }
    drop(table);
    let id = format!("channel-{logical_id}");
    let mut found = conn
        .prepare("SELECT 1 FROM drive_directories WHERE id = ? AND trashed_at IS NULL")
        .map_err(|e: sqlite::Error| e.to_string())?;
    found.bind((1, id.as_str())).map_err(|e| e.to_string())?;
    if found.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Ok(());
    }
    drop(found);
    for query in [
        "SELECT 1 FROM drive_directories WHERE parent_id = ? AND trashed_at IS NULL LIMIT 1",
        "SELECT 1 FROM drive_file_entries WHERE directory_id = ? AND trashed_at IS NULL LIMIT 1",
        "SELECT 1 FROM drive_entries_v2 WHERE directory_id = ? AND trashed_at IS NULL LIMIT 1",
        "SELECT 1 FROM drive_pending_files WHERE directory_id = ? LIMIT 1",
        "SELECT 1 FROM drive_symlinks WHERE directory_id = ? AND trashed_at IS NULL LIMIT 1",
    ] {
        let mut stmt = conn
            .prepare(query)
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
        if stmt.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
            return Err("This shared channel still contains files or folders in TeraRelay Drive. Move them before removing the channel.".into());
        }
    }
    let mut update = conn
        .prepare(
            "UPDATE drive_directories SET trashed_at = ?, updated_at = ?
         WHERE id = ? AND trashed_at IS NULL",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    update.bind((1, when)).map_err(|e| e.to_string())?;
    update.bind((2, when)).map_err(|e| e.to_string())?;
    update.bind((3, id.as_str())).map_err(|e| e.to_string())?;
    update.next().map_err(|e| e.to_string())?;
    Ok(())
}
/// A transfer can be running while its remote manifest has not yet
/// published. Refuse to remove the view until both durable queues have drained.
fn ensure_no_queued_transfers(conn: &sqlite::Connection, channel_id: i64) -> Result<(), String> {
    let mut table = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='transfer_queues'")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if table.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row {
        return Ok(());
    }
    drop(table);
    let mut stmt = conn
        .prepare("SELECT kind, items FROM transfer_queues")
        .map_err(|e: sqlite::Error| e.to_string())?;
    while stmt.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
        let kind = stmt.read::<String, _>(0).map_err(|e| e.to_string())?;
        if !matches!(kind.as_str(), "upload" | "download") {
            continue;
        }
        let contents = stmt.read::<String, _>(1).map_err(|e| e.to_string())?;
        let items: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| format!("Cannot safely read queued transfers: {e}"))?;
        let rows = items
            .as_array()
            .ok_or("Invalid saved transfer queue; channel was not removed")?;
        for item in rows {
            if item.get("folderId").and_then(|id| id.as_i64()) == Some(channel_id) {
                match item.get("status").and_then(|status| status.as_str()) {
                    Some("success" | "cancelled" | "skipped") => {}
                    _ => return Err(format!(
                        "This shared channel has a queued or unfinished {kind}. Finish or cancel it before removing the channel."
                    )),
                }
            }
        }
    }
    Ok(())
}
pub fn remove_empty_shared(conn: &sqlite::Connection, channel_id: i64) -> Result<(), String> {
    let mut r = record(conn, channel_id)?.ok_or("Unknown shared channel")?;
    ensure_no_queued_transfers(conn, channel_id)?;
    if r.deleted_at.is_some() {
        return Err("Shared channel already removed".into());
    }
    // Never destroy channel or Telegram storage while any file is assigned here.
    let mut files = conn
        .prepare("SELECT 1 FROM logical_files WHERE logical_channel_id=? LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    files
        .bind((1, r.logical_id.as_str()))
        .map_err(|e| e.to_string())?;
    if files.next().map_err(|e: sqlite::Error| e.to_string())? == sqlite::State::Row {
        return Err("This shared channel contains files. Move or download them before removing the channel; the shared Telegram storage is safe.".into());
    }
    drop(files);
    let now = timestamp().max(r.updated_at + 1);
    r.deleted_at = Some(now);
    r.updated_at = now;
    conn.execute("SAVEPOINT remove_shared_channel")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| {
        tombstone_empty_imported_folder(conn, &r.logical_id, now)?;
        apply_record(conn, &r)?;
        crate::commands::drive_metadata::mark_drive_dirty(conn)?;
        Ok::<(), String>(())
    })();
    crate::drive_posix::finish_savepoint(conn, "remove_shared_channel", result)?;
    refresh_aliases(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> sqlite::Connection {
        let c = sqlite::open(":memory:").unwrap();
        c.execute("PRAGMA foreign_keys=ON; CREATE TABLE logical_channels(logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER NOT NULL UNIQUE,name TEXT NOT NULL,role TEXT NOT NULL,storage_version INTEGER NOT NULL,created_at INTEGER NOT NULL,joined_at INTEGER NOT NULL);
           CREATE TABLE folder_metadata(channel_id INTEGER PRIMARY KEY,name TEXT NOT NULL,username TEXT,is_public INTEGER DEFAULT 0,display_order INTEGER DEFAULT 0,group_id INTEGER);
           CREATE TABLE logical_files(file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL REFERENCES logical_channels(logical_id),first_message_id INTEGER, display_name TEXT, total_size INTEGER, mime_type TEXT, chunk_count INTEGER, whole_sha256 TEXT, storage_version INTEGER, created_at INTEGER, status TEXT);
           CREATE TABLE drive_state(singleton INTEGER PRIMARY KEY, revision INTEGER NOT NULL,dirty INTEGER NOT NULL);
           INSERT INTO drive_state VALUES(1,1,0);").unwrap();
        init_schema(&c).unwrap();
        c
    }
    fn sample(id: i64) -> SharedChannelRecord {
        SharedChannelRecord {
            channel_id: id,
            logical_id: "a".repeat(32),
            storage_channel_id: 123,
            name: "Movies".into(),
            created_at: 5,
            updated_at: 5,
            deleted_at: None,
        }
    }
    #[test]
    fn shared_isolated_from_physical_channel_and_tombstone_preserves_rows() {
        let conn = db();
        let mut r = sample(-5);
        apply_record(&conn, &r).unwrap();
        assert_eq!(physical_id(&conn, Some(-5)).unwrap(), Some(123));
        assert_eq!(physical_id(&conn, Some(123)).unwrap(), Some(123));
        conn.execute(format!("INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,mime_type,chunk_count,whole_sha256,storage_version,created_at,status) VALUES ('f','{}',9,'v',1,NULL,1,NULL,1,1,'complete')",r.logical_id)).unwrap();
        assert!(remove_empty_shared(&conn, -5).is_err());
        r.updated_at = 7;
        r.deleted_at = Some(7);
        apply_record(&conn, &r).unwrap();
        assert!(physical_id(&conn, Some(-5)).is_err());
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM logical_files").unwrap();
        assert_eq!(stmt.next().unwrap(), sqlite::State::Row);
        assert_eq!(stmt.read::<i64, _>(0).unwrap(), 1);
    }
    #[test]
    fn cannot_remove_shared_view_while_upload_or_download_is_queued() {
        let conn = db();
        apply_record(&conn, &sample(-9)).unwrap();
        conn.execute(
            r#"CREATE TABLE transfer_queues(kind TEXT PRIMARY KEY, items TEXT NOT NULL);
               INSERT INTO transfer_queues VALUES
               ('upload','[{"folderId":-9,"status":"uploading"}]'),
               ('download','[{"folderId":-9,"status":"success"}]');"#,
        )
        .unwrap();
        assert!(remove_empty_shared(&conn, -9)
            .unwrap_err()
            .contains("unfinished upload"));
        assert!(record(&conn, -9).unwrap().unwrap().deleted_at.is_none());
        conn.execute(
            r#"UPDATE transfer_queues SET items='[{"folderId":-9,"status":"success"}]'
               WHERE kind='upload';
               UPDATE transfer_queues SET items='[{"folderId":-9,"status":"pending"}]'
               WHERE kind='download';"#,
        )
        .unwrap();
        assert!(remove_empty_shared(&conn, -9)
            .unwrap_err()
            .contains("unfinished download"));
        conn.execute(
            r#"UPDATE transfer_queues SET items='[{"folderId":-9,"status":"cancelled"}]'
               WHERE kind='download';"#,
        )
        .unwrap();
        remove_empty_shared(&conn, -9).unwrap();
        assert!(physical_id(&conn, Some(-9)).is_err());
        assert_eq!(physical_id(&conn, Some(123)).unwrap(), Some(123));
    }
    #[test]
    fn shared_channels_with_one_physical_channel_have_distinct_names_and_logical_scopes() {
        let conn = db();
        let first = sample(-50);
        let mut second = sample(-60);
        second.logical_id = "b".repeat(32);
        second.name = "Family".into();
        apply_record(&conn, &first).unwrap();
        apply_record(&conn, &second).unwrap();
        assert_eq!(physical_id(&conn, Some(-50)).unwrap(), Some(123));
        assert_eq!(physical_id(&conn, Some(-60)).unwrap(), Some(123));
        assert!(logical_id_in_storage(&conn, &first.logical_id, 123).unwrap());
        assert!(logical_id_in_storage(&conn, &second.logical_id, 123).unwrap());
        assert_ne!(first.logical_id, second.logical_id);
        assert!(rename_shared(&conn, -60, "movies")
            .unwrap_err()
            .contains("already exists"));
        rename_shared(&conn, -60, "Comedy").unwrap();
        assert_eq!(record(&conn, -50).unwrap().unwrap().name, "Movies");
        assert_eq!(record(&conn, -60).unwrap().unwrap().name, "Comedy");
    }

    #[test]
    fn remote_merge_keeps_deletes_and_rejects_alias_hijacking() {
        let base = sample(-8);
        let mut dead = base.clone();
        dead.updated_at = 10;
        dead.deleted_at = Some(10);
        assert_eq!(choose_newer(base.clone(), dead.clone()).unwrap(), dead);
        let mut malicious = base.clone();
        malicious.storage_channel_id = 124;
        assert!(choose_newer(base, malicious).is_err());
    }
}
