use crate::commands::utils::map_error;
use crate::commands::TelegramState;
use crate::db::DbConnection;
use grammers_client::types::Peer;
use grammers_tl_types as tl;
use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

pub const DRIVE_STORAGE_TITLE_PREFIX: &str = "TeraRelay Drive Storage";
pub const DRIVE_STORAGE_ABOUT: &str =
    "TeraRelay Drive private backend storage\n[terarelay-drive-storage]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveStorageChannel {
    pub generation: i64,
    pub backing_channel_id: i64,
    pub state: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub retirement_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDriveStorageChannel {
    pub backing_channel_id: i64,
    pub title: String,
}

#[derive(Clone, Default)]
pub struct DriveStorageState {
    provision_lock: Arc<tokio::sync::Mutex<()>>,
}

impl DriveStorageState {
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub fn init_drive_storage_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_storage_channels (
            generation INTEGER PRIMARY KEY,
            backing_channel_id INTEGER NOT NULL UNIQUE,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            retirement_reason TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_drive_storage_channels_state
            ON drive_storage_channels(state, generation DESC);",
    )
    .map_err(|e: sqlite::Error| e.to_string())
}

fn read_channel(stmt: &sqlite::Statement) -> Result<DriveStorageChannel, String> {
    Ok(DriveStorageChannel {
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
            .map_err(|e| e.to_string())?,
    })
}

pub fn active_storage_channel(
    conn: &sqlite::Connection,
) -> Result<Option<DriveStorageChannel>, String> {
    init_drive_storage_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT generation, backing_channel_id, state, title, created_at, updated_at,
                    retirement_reason
             FROM drive_storage_channels
             WHERE state = 'active'
             ORDER BY generation DESC
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return read_channel(&stmt).map(Some);
    }
    Ok(None)
}

pub fn storage_channel_by_backing(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
) -> Result<Option<DriveStorageChannel>, String> {
    init_drive_storage_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT generation, backing_channel_id, state, title, created_at, updated_at,
                    retirement_reason
             FROM drive_storage_channels
             WHERE backing_channel_id = ?
             LIMIT 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return read_channel(&stmt).map(Some);
    }
    Ok(None)
}

pub fn next_storage_generation(conn: &sqlite::Connection) -> Result<i64, String> {
    init_drive_storage_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT COALESCE(MAX(generation), 0) + 1 FROM drive_storage_channels")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(1);
    }
    stmt.read::<i64, _>(0).map_err(|e| e.to_string())
}

pub fn storage_title(generation: i64) -> String {
    if generation <= 1 {
        DRIVE_STORAGE_TITLE_PREFIX.to_string()
    } else {
        format!("{DRIVE_STORAGE_TITLE_PREFIX} {generation:03}")
    }
}

fn storage_generation_from_title(title: &str) -> Option<i64> {
    if title == DRIVE_STORAGE_TITLE_PREFIX {
        return Some(1);
    }
    let suffix = title
        .strip_prefix(DRIVE_STORAGE_TITLE_PREFIX)?
        .strip_prefix(' ')?;
    let generation = suffix.parse::<i64>().ok()?;
    (generation > 1).then_some(generation)
}

pub fn register_storage_channel(
    conn: &sqlite::Connection,
    generation: i64,
    backing_channel_id: i64,
    title: &str,
    state: &str,
    retirement_reason: Option<&str>,
) -> Result<DriveStorageChannel, String> {
    init_drive_storage_schema(conn)?;
    if generation <= 0 || backing_channel_id <= 0 {
        return Err("Drive storage channel identity is invalid".to_string());
    }
    if !matches!(state, "active" | "read_only" | "retired") {
        return Err("Drive storage channel state is invalid".to_string());
    }
    let now = now_ms();
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| -> Result<(), String> {
        if state == "active" {
            let mut retire = conn
                .prepare(
                    "UPDATE drive_storage_channels
                     SET state = 'read_only', updated_at = ?
                     WHERE state = 'active' AND backing_channel_id != ?",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            retire
                .bind((1, now))
                .map_err(|e: sqlite::Error| e.to_string())?;
            retire
                .bind((2, backing_channel_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            retire.next().map_err(|e: sqlite::Error| e.to_string())?;
        }
        let mut stmt = conn
            .prepare(
                "INSERT INTO drive_storage_channels
                 (generation, backing_channel_id, state, title, created_at, updated_at, retirement_reason)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(backing_channel_id) DO UPDATE SET
                    state = excluded.state,
                    title = excluded.title,
                    updated_at = excluded.updated_at,
                    retirement_reason = excluded.retirement_reason",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, generation))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, backing_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((3, state))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((4, title))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((5, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((6, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((7, retirement_reason))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
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
    storage_channel_by_backing(conn, backing_channel_id)?
        .ok_or_else(|| "Drive storage channel was not persisted".to_string())
}

pub fn is_drive_storage_channel(title: &str, about: &str) -> bool {
    title.starts_with(DRIVE_STORAGE_TITLE_PREFIX)
        && about.contains("[terarelay-drive-storage]")
        && !about.contains("[terarelay-folder]")
}

pub fn is_transient_storage_error(error: &str) -> bool {
    let value = error.to_ascii_uppercase();
    value.contains("FLOOD_WAIT")
        || value.contains("FLOOD_PREMIUM_WAIT")
        || value.contains("TIMEOUT")
        || value.contains("RPC_")
        || value.contains("NETWORK")
        || value.contains("OFFLINE")
        || value.contains("CONNECTION")
        || value.contains("TEMPORARY")
}

fn permanent_storage_error_code(error: &str) -> Option<&'static str> {
    let value = error.to_ascii_uppercase();
    // Keep this list intentionally small. These errors mean the current peer is
    // no longer writable/usable; file-size, media, network and rate-limit errors
    // must never create another Telegram channel.
    for code in [
        "CHAT_WRITE_FORBIDDEN",
        "CHANNEL_PRIVATE",
        "CHANNEL_INVALID",
        "USER_BANNED_IN_CHANNEL",
        "CHAT_ADMIN_REQUIRED",
    ] {
        if value.contains(code) {
            return Some(code);
        }
    }
    None
}

/// Rollover is intentionally allowlisted. FloodWait, network failures, upload
/// format/size errors and unknown RPC failures stay on the same channel.
pub fn should_rotate_after_error(error: &str) -> bool {
    // The permanent allowlist is the authority. This intentionally still works
    // when a library formats the same RPC as `RPC_CHAT_WRITE_FORBIDDEN`; unknown
    // RPC/network/rate-limit errors remain false because they match no allowlist code.
    permanent_storage_error_code(error).is_some()
}

pub async fn ensure_active_storage_channel_with<Discover, DiscoverFuture, Create, CreateFuture>(
    db: &DbConnection,
    state: &DriveStorageState,
    discover: Discover,
    create: Create,
) -> Result<DriveStorageChannel, String>
where
    Discover: FnOnce() -> DiscoverFuture,
    DiscoverFuture: Future<Output = Result<Option<DiscoveredDriveStorageChannel>, String>>,
    Create: FnOnce() -> CreateFuture,
    CreateFuture: Future<Output = Result<DiscoveredDriveStorageChannel, String>>,
{
    {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        if let Some(active) = active_storage_channel(&conn)? {
            return Ok(active);
        }
    }

    let _guard = state.provision_lock.lock().await;
    {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        if let Some(active) = active_storage_channel(&conn)? {
            return Ok(active);
        }
    }

    if let Some(found) = discover().await? {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        if let Some(existing) = storage_channel_by_backing(&conn, found.backing_channel_id)? {
            if existing.state == "active" {
                return Ok(existing);
            }
        }
        let generation = next_storage_generation(&conn)?;
        return register_storage_channel(
            &conn,
            generation,
            found.backing_channel_id,
            &found.title,
            "active",
            None,
        );
    }

    let created = create().await?;
    let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
    let generation = next_storage_generation(&conn)?;
    register_storage_channel(
        &conn,
        generation,
        created.backing_channel_id,
        &created.title,
        "active",
        None,
    )
}

fn activate_storage_replacement(
    conn: &sqlite::Connection,
    failed_backing_channel_id: i64,
    replacement: &DiscoveredDriveStorageChannel,
    retirement_reason: &str,
) -> Result<DriveStorageChannel, String> {
    init_drive_storage_schema(conn)?;
    if failed_backing_channel_id <= 0
        || replacement.backing_channel_id <= 0
        || replacement.backing_channel_id == failed_backing_channel_id
    {
        return Err("Drive storage rollover channel identity is invalid".to_string());
    }
    let existing_replacement = storage_channel_by_backing(conn, replacement.backing_channel_id)?;
    let generation = match existing_replacement.as_ref() {
        Some(existing) => existing.generation,
        None => next_storage_generation(conn)?,
    };
    let now = now_ms();
    conn.execute("BEGIN IMMEDIATE")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let result = (|| -> Result<(), String> {
        let mut retire = conn
            .prepare(
                "UPDATE drive_storage_channels
                 SET state = 'read_only', updated_at = ?, retirement_reason = ?
                 WHERE backing_channel_id = ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        retire
            .bind((1, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        retire
            .bind((2, retirement_reason))
            .map_err(|e: sqlite::Error| e.to_string())?;
        retire
            .bind((3, failed_backing_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        retire.next().map_err(|e: sqlite::Error| e.to_string())?;

        // Defensive single-writer invariant: even if a concurrent metadata sync
        // introduced a different active row, only the replacement is active after
        // this transaction commits.
        let mut demote = conn
            .prepare(
                "UPDATE drive_storage_channels
                 SET state = 'read_only', updated_at = ?
                 WHERE state = 'active' AND backing_channel_id != ?",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        demote
            .bind((1, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        demote
            .bind((2, replacement.backing_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        demote.next().map_err(|e: sqlite::Error| e.to_string())?;

        let mut insert = conn
            .prepare(
                "INSERT INTO drive_storage_channels
                 (generation, backing_channel_id, state, title, created_at, updated_at, retirement_reason)
                 VALUES (?, ?, 'active', ?, ?, ?, NULL)
                 ON CONFLICT(backing_channel_id) DO UPDATE SET
                    state = 'active',
                    title = excluded.title,
                    updated_at = excluded.updated_at,
                    retirement_reason = NULL",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((1, generation))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((2, replacement.backing_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((3, replacement.title.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((4, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert
            .bind((5, now))
            .map_err(|e: sqlite::Error| e.to_string())?;
        insert.next().map_err(|e: sqlite::Error| e.to_string())?;
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
    storage_channel_by_backing(conn, replacement.backing_channel_id)?
        .ok_or_else(|| "Drive storage rollover was not persisted".to_string())
}

pub async fn rotate_storage_channel_with<Discover, DiscoverFuture, Create, CreateFuture>(
    db: &DbConnection,
    state: &DriveStorageState,
    failed_backing_channel_id: i64,
    error: &str,
    discover: Discover,
    create: Create,
) -> Result<DriveStorageChannel, String>
where
    Discover: FnOnce() -> DiscoverFuture,
    DiscoverFuture: Future<Output = Result<Option<DiscoveredDriveStorageChannel>, String>>,
    Create: FnOnce() -> CreateFuture,
    CreateFuture: Future<Output = Result<DiscoveredDriveStorageChannel, String>>,
{
    let retirement_reason = permanent_storage_error_code(error)
        .filter(|_| should_rotate_after_error(error))
        .ok_or_else(|| "Drive storage rollover refused for a non-permanent error".to_string())?;

    let _guard = state.provision_lock.lock().await;
    {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        if let Some(active) = active_storage_channel(&conn)? {
            if active.backing_channel_id != failed_backing_channel_id {
                // Another caller already completed rollover while we waited.
                return Ok(active);
            }
        }
    }

    // Rediscover before creating. This recovers safely if the previous process
    // created the next Telegram channel but crashed before persisting its DB row.
    let discovered = discover().await?;
    let replacement = match discovered {
        Some(found) if found.backing_channel_id != failed_backing_channel_id => found,
        _ => create().await?,
    };
    if replacement.backing_channel_id == failed_backing_channel_id {
        return Err("Drive storage rollover rediscovered only the failed channel".to_string());
    }

    let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
    if let Some(active) = active_storage_channel(&conn)? {
        if active.backing_channel_id != failed_backing_channel_id {
            return Ok(active);
        }
    }
    activate_storage_replacement(
        &conn,
        failed_backing_channel_id,
        &replacement,
        retirement_reason,
    )
}

async fn discover_remote_storage_channel(
    state: &TelegramState,
) -> Result<Option<DiscoveredDriveStorageChannel>, String> {
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Telegram client is not connected".to_string())?;
    let mut dialogs = client.iter_dialogs();
    let mut best: Option<(i64, DiscoveredDriveStorageChannel)> = None;
    while let Some(dialog) = dialogs.next().await.map_err(map_error)? {
        let Peer::Channel(channel) = &dialog.peer else {
            continue;
        };
        if !channel.raw.creator {
            continue;
        }
        let Some(generation) = storage_generation_from_title(&channel.raw.title) else {
            continue;
        };
        let Some(access_hash) = channel.raw.access_hash else {
            continue;
        };
        let input = tl::enums::InputChannel::Channel(tl::types::InputChannel {
            channel_id: channel.raw.id,
            access_hash,
        });
        let full = match client
            .invoke(&tl::functions::channels::GetFullChannel { channel: input })
            .await
        {
            Ok(full) => full,
            Err(error) => {
                let error = map_error(error);
                // An old Drive generation can itself be the permanently failed
                // peer we are rotating away from. Skip only allowlisted permanent
                // peer failures; transient/global failures abort discovery so we
                // never create a duplicate channel just because Telegram is flaky.
                if should_rotate_after_error(&error) {
                    continue;
                }
                return Err(error);
            }
        };
        let tl::enums::messages::ChatFull::Full(full) = full;
        let tl::enums::ChatFull::Full(info) = full.full_chat else {
            continue;
        };
        if !is_drive_storage_channel(&channel.raw.title, &info.about) {
            continue;
        }
        state
            .peer_cache
            .write()
            .await
            .insert(channel.raw.id, dialog.peer.clone());
        let candidate = DiscoveredDriveStorageChannel {
            backing_channel_id: channel.raw.id,
            title: channel.raw.title.clone(),
        };
        let replace = best
            .as_ref()
            .map(|(best_generation, best_channel)| {
                generation > *best_generation
                    || (generation == *best_generation
                        && candidate.backing_channel_id > best_channel.backing_channel_id)
            })
            .unwrap_or(true);
        if replace {
            best = Some((generation, candidate));
        }
    }
    Ok(best.map(|(_, channel)| channel))
}

async fn create_remote_storage_channel(
    state: &TelegramState,
    db: &DbConnection,
) -> Result<DiscoveredDriveStorageChannel, String> {
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Telegram client is not connected".to_string())?;
    let generation = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        next_storage_generation(&conn)?
    };
    let title = storage_title(generation);
    let result = client
        .invoke(&tl::functions::channels::CreateChannel {
            broadcast: true,
            megagroup: false,
            title: title.clone(),
            about: DRIVE_STORAGE_ABOUT.to_string(),
            geo_point: None,
            address: None,
            for_import: false,
            forum: false,
            ttl_period: None,
        })
        .await
        .map_err(map_error)?;

    let (channel_id, access_hash, peer) = match result {
        tl::enums::Updates::Updates(updates) => {
            let chat = updates
                .chats
                .into_iter()
                .next()
                .ok_or_else(|| "Drive storage channel creation returned no chat".to_string())?;
            match chat {
                tl::enums::Chat::Channel(raw) => {
                    let channel = grammers_client::types::Channel { raw: raw.clone() };
                    let peer = Peer::Channel(channel);
                    (raw.id, raw.access_hash.unwrap_or(0), peer)
                }
                _ => return Err("Created Drive storage chat is not a channel".to_string()),
            }
        }
        _ => return Err("Unexpected Drive storage channel creation response".to_string()),
    };

    state.peer_cache.write().await.insert(channel_id, peer);
    let _ = client
        .invoke(&tl::functions::messages::SetHistoryTtl {
            peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id,
                access_hash,
            }),
            period: 0,
        })
        .await;

    Ok(DiscoveredDriveStorageChannel {
        backing_channel_id: channel_id,
        title,
    })
}

pub async fn ensure_active_storage_channel(
    _app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    storage: &DriveStorageState,
) -> Result<DriveStorageChannel, String> {
    let channel = ensure_active_storage_channel_with(
        db,
        storage,
        || discover_remote_storage_channel(state),
        || create_remote_storage_channel(state, db),
    )
    .await?;

    ensure_storage_logical_identity(db, &channel)?;
    Ok(channel)
}

fn ensure_storage_logical_identity(
    db: &DbConnection,
    channel: &DriveStorageChannel,
) -> Result<(), String> {
    // Reuse the proven logical-file manifest/range-streaming layer without
    // exposing this backend channel as a user folder. `folder_metadata` is
    // deliberately untouched; the special role also excludes it from Drive's
    // legacy import path.
    let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
    crate::commands::logical_channels::ensure_logical_channel(
        &conn,
        channel.backing_channel_id,
        &channel.title,
        "drive_storage",
    )?;
    Ok(())
}

pub async fn rotate_storage_channel_after_permanent_error(
    state: &TelegramState,
    db: &DbConnection,
    storage: &DriveStorageState,
    failed_backing_channel_id: i64,
    error: &str,
) -> Result<DriveStorageChannel, String> {
    let channel = rotate_storage_channel_with(
        db,
        storage,
        failed_backing_channel_id,
        error,
        || discover_remote_storage_channel(state),
        || create_remote_storage_channel(state, db),
    )
    .await?;
    ensure_storage_logical_identity(db, &channel)?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn memory_db() -> sqlite::Connection {
        sqlite::open(":memory:").unwrap()
    }

    #[test]
    fn schema_is_idempotent_and_active_channel_is_newest_active() {
        let conn = memory_db();
        init_drive_storage_schema(&conn).unwrap();
        init_drive_storage_schema(&conn).unwrap();
        register_storage_channel(&conn, 1, 1001, "TeraRelay Drive Storage", "active", None)
            .unwrap();
        register_storage_channel(
            &conn,
            2,
            1002,
            "TeraRelay Drive Storage 002",
            "read_only",
            Some("rotated"),
        )
        .unwrap();
        register_storage_channel(
            &conn,
            3,
            1003,
            "TeraRelay Drive Storage 003",
            "active",
            None,
        )
        .unwrap();
        let active = active_storage_channel(&conn).unwrap().unwrap();
        assert_eq!(active.generation, 3);
        assert_eq!(active.backing_channel_id, 1003);
        assert_eq!(active.state, "active");
    }

    #[test]
    fn drive_storage_marker_is_not_a_user_folder_marker() {
        assert!(is_drive_storage_channel(
            "TeraRelay Drive Storage",
            DRIVE_STORAGE_ABOUT
        ));
        assert!(!is_drive_storage_channel(
            "Photos [TR]",
            "TeraRelay Storage Folder\n[terarelay-folder]"
        ));
        assert!(!DRIVE_STORAGE_ABOUT.contains("[terarelay-folder]"));
    }

    #[test]
    fn only_proven_permanent_channel_errors_request_rotation() {
        for error in [
            "FLOOD_WAIT_60",
            "FLOOD_PREMIUM_WAIT_12",
            "TIMEOUT",
            "RPC_CALL_FAIL",
            "network offline",
            "FILE_PARTS_INVALID",
            "FILE_TOO_LARGE",
            "MESSAGE_TOO_LONG",
        ] {
            assert!(!should_rotate_after_error(error), "{error}");
        }
        for error in [
            "CHAT_WRITE_FORBIDDEN",
            "RPC_CHAT_WRITE_FORBIDDEN",
            "CHANNEL_PRIVATE",
            "CHANNEL_INVALID",
            "USER_BANNED_IN_CHANNEL",
            "CHAT_ADMIN_REQUIRED",
        ] {
            assert!(should_rotate_after_error(error), "{error}");
        }
    }

    #[test]
    fn storage_titles_have_stable_generation_numbers() {
        assert_eq!(
            storage_generation_from_title("TeraRelay Drive Storage"),
            Some(1)
        );
        assert_eq!(
            storage_generation_from_title("TeraRelay Drive Storage 002"),
            Some(2)
        );
        assert_eq!(
            storage_generation_from_title("TeraRelay Drive Storage 120"),
            Some(120)
        );
        assert_eq!(
            storage_generation_from_title("TeraRelay Drive Storage 000"),
            None
        );
        assert_eq!(
            storage_generation_from_title("TeraRelay Drive Storage backup"),
            None
        );
        assert_eq!(storage_generation_from_title("Photos"), None);
    }

    #[tokio::test]
    async fn serialized_permanent_failure_rollover_creates_exactly_once() {
        let db = Arc::new(std::sync::Mutex::new(memory_db()));
        {
            let conn = db.lock().unwrap();
            init_drive_storage_schema(&conn).unwrap();
            register_storage_channel(&conn, 1, 1001, "TeraRelay Drive Storage", "active", None)
                .unwrap();
        }
        let state = Arc::new(DriveStorageState::new());
        let creates = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let db = db.clone();
            let state = state.clone();
            let creates = creates.clone();
            tasks.push(tokio::spawn(async move {
                rotate_storage_channel_with(
                    &db,
                    &state,
                    1001,
                    "CHAT_WRITE_FORBIDDEN",
                    || async { Ok(None) },
                    || {
                        let creates = creates.clone();
                        async move {
                            creates.fetch_add(1, Ordering::SeqCst);
                            Ok(DiscoveredDriveStorageChannel {
                                backing_channel_id: 2002,
                                title: "TeraRelay Drive Storage 002".to_string(),
                            })
                        }
                    },
                )
                .await
                .unwrap()
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().backing_channel_id, 2002);
        }
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        let conn = db.lock().unwrap();
        assert_eq!(
            active_storage_channel(&conn)
                .unwrap()
                .unwrap()
                .backing_channel_id,
            2002
        );
        let old = storage_channel_by_backing(&conn, 1001).unwrap().unwrap();
        assert_eq!(old.state, "read_only");
        assert_eq!(
            old.retirement_reason.as_deref(),
            Some("CHAT_WRITE_FORBIDDEN")
        );
    }

    #[tokio::test]
    async fn rollover_rediscovers_newer_remote_channel_before_creating_another() {
        let db = Arc::new(std::sync::Mutex::new(memory_db()));
        {
            let conn = db.lock().unwrap();
            init_drive_storage_schema(&conn).unwrap();
            register_storage_channel(&conn, 1, 1001, "TeraRelay Drive Storage", "active", None)
                .unwrap();
        }
        let state = DriveStorageState::new();
        let creates = AtomicUsize::new(0);
        let replacement = rotate_storage_channel_with(
            &db,
            &state,
            1001,
            "CHANNEL_PRIVATE",
            || async {
                Ok(Some(DiscoveredDriveStorageChannel {
                    backing_channel_id: 2002,
                    title: "TeraRelay Drive Storage 002".to_string(),
                }))
            },
            || async {
                creates.fetch_add(1, Ordering::SeqCst);
                Err("must not create".to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(replacement.backing_channel_id, 2002);
        assert_eq!(creates.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_rollover_creation_leaves_current_channel_active() {
        let db = Arc::new(std::sync::Mutex::new(memory_db()));
        {
            let conn = db.lock().unwrap();
            init_drive_storage_schema(&conn).unwrap();
            register_storage_channel(&conn, 1, 1001, "TeraRelay Drive Storage", "active", None)
                .unwrap();
        }
        let state = DriveStorageState::new();
        let error = rotate_storage_channel_with(
            &db,
            &state,
            1001,
            "CHAT_WRITE_FORBIDDEN",
            || async { Ok(None) },
            || async { Err("FLOOD_WAIT_60".to_string()) },
        )
        .await
        .unwrap_err();
        assert_eq!(error, "FLOOD_WAIT_60");
        let conn = db.lock().unwrap();
        let active = active_storage_channel(&conn).unwrap().unwrap();
        assert_eq!(active.backing_channel_id, 1001);
        assert_eq!(active.state, "active");
    }

    #[tokio::test]
    async fn serialized_provisioning_creates_once_for_concurrent_callers() {
        let db = Arc::new(std::sync::Mutex::new(memory_db()));
        {
            let conn = db.lock().unwrap();
            init_drive_storage_schema(&conn).unwrap();
        }
        let state = Arc::new(DriveStorageState::new());
        let creates = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let db = db.clone();
            let state = state.clone();
            let creates = creates.clone();
            tasks.push(tokio::spawn(async move {
                ensure_active_storage_channel_with(
                    &db,
                    &state,
                    || async { Ok(None) },
                    || {
                        let creates = creates.clone();
                        async move {
                            creates.fetch_add(1, Ordering::SeqCst);
                            Ok(DiscoveredDriveStorageChannel {
                                backing_channel_id: 777,
                                title: "TeraRelay Drive Storage".to_string(),
                            })
                        }
                    },
                )
                .await
                .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.unwrap().backing_channel_id);
        }
        assert!(ids.iter().all(|id| *id == 777));
        assert_eq!(creates.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rediscovery_prevents_duplicate_creation_after_missing_local_row() {
        let db = Arc::new(std::sync::Mutex::new(memory_db()));
        {
            let conn = db.lock().unwrap();
            init_drive_storage_schema(&conn).unwrap();
        }
        let state = DriveStorageState::new();
        let creates = AtomicUsize::new(0);
        let found = ensure_active_storage_channel_with(
            &db,
            &state,
            || async {
                Ok(Some(DiscoveredDriveStorageChannel {
                    backing_channel_id: 888,
                    title: "TeraRelay Drive Storage".to_string(),
                }))
            },
            || async {
                creates.fetch_add(1, Ordering::SeqCst);
                Err("must not create".to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(found.backing_channel_id, 888);
        assert_eq!(creates.load(Ordering::SeqCst), 0);
        let conn = db.lock().unwrap();
        assert_eq!(
            active_storage_channel(&conn)
                .unwrap()
                .unwrap()
                .backing_channel_id,
            888
        );
    }
}
