use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use crate::models::FolderMetadata;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use grammers_client::types::Peer;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

const INVITE_SCHEME_PREFIX: &str = "terarelay://join/";
const INVITE_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicalChannelRecord {
    pub logical_id: String,
    pub backing_channel_id: i64,
    pub name: String,
    pub role: String,
    pub storage_version: i32,
    pub created_at: i64,
    pub joined_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelInvitePayload {
    v: u8,
    logical_id: String,
    name: String,
    backing_channel_id: i64,
    telegram_link: String,
    role: String,
    checksum: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TeraChannelInviteInfo {
    pub link: String,
    pub telegram_link: String,
    pub logical_id: String,
    pub channel_name: String,
    pub backing_channel_id: i64,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinChannelResult {
    pub folder: FolderMetadata,
    pub logical_id: String,
    pub role: String,
    pub already_joined: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TeraChannelMember {
    pub display_name: String,
    pub username: Option<String>,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn invite_checksum(
    v: u8,
    logical_id: &str,
    name: &str,
    backing_channel_id: i64,
    telegram_link: &str,
    role: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(v.to_string().as_bytes());
    h.update(b"\n");
    h.update(logical_id.as_bytes());
    h.update(b"\n");
    h.update(name.as_bytes());
    h.update(b"\n");
    h.update(backing_channel_id.to_string().as_bytes());
    h.update(b"\n");
    h.update(telegram_link.as_bytes());
    h.update(b"\n");
    h.update(role.as_bytes());
    format!("{:x}", h.finalize())
}

fn random_logical_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_logical_channel(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
) -> Result<Option<LogicalChannelRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at
             FROM logical_channels WHERE backing_channel_id = ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;

    if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        return Ok(Some(LogicalChannelRecord {
            logical_id: stmt
                .read::<String, _>("logical_id")
                .map_err(|e| e.to_string())?,
            backing_channel_id: stmt
                .read::<i64, _>("backing_channel_id")
                .map_err(|e| e.to_string())?,
            name: stmt.read::<String, _>("name").map_err(|e| e.to_string())?,
            role: stmt.read::<String, _>("role").map_err(|e| e.to_string())?,
            storage_version: stmt
                .read::<i64, _>("storage_version")
                .map_err(|e| e.to_string())? as i32,
            created_at: stmt
                .read::<i64, _>("created_at")
                .map_err(|e| e.to_string())?,
            joined_at: stmt
                .read::<i64, _>("joined_at")
                .map_err(|e| e.to_string())?,
        }));
    }
    Ok(None)
}

pub fn role_for_backing_channel(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
) -> Result<Option<String>, String> {
    Ok(read_logical_channel(conn, backing_channel_id)?.map(|record| record.role))
}

pub fn logical_channel_id_for_backing(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
) -> Result<Option<String>, String> {
    Ok(read_logical_channel(conn, backing_channel_id)?.map(|record| record.logical_id))
}

pub fn require_owner_role(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
) -> Result<(), String> {
    match role_for_backing_channel(conn, backing_channel_id)? {
        Some(role) if role == "member" => Err(
            "This TeraRelay channel is read-only for members. Only the owner can modify it."
                .to_string(),
        ),
        _ => Ok(()),
    }
}

pub fn ensure_channel_can_upload(
    conn: &sqlite::Connection,
    backing_channel_id: Option<i64>,
) -> Result<(), String> {
    if let Some(channel_id) = backing_channel_id {
        require_owner_role(conn, channel_id)?;
    }
    Ok(())
}

pub fn ensure_logical_channel(
    conn: &sqlite::Connection,
    backing_channel_id: i64,
    name: &str,
    role: &str,
) -> Result<LogicalChannelRecord, String> {
    if let Some(existing) = read_logical_channel(conn, backing_channel_id)? {
        return Ok(existing);
    }

    let now = unix_now();
    let record = LogicalChannelRecord {
        logical_id: random_logical_id(),
        backing_channel_id,
        name: name.to_string(),
        role: role.to_string(),
        storage_version: 1,
        created_at: now,
        joined_at: now,
    };

    let mut stmt = conn
        .prepare(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, record.logical_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, record.backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, record.name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, record.role.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, record.storage_version as i64))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((6, record.created_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((7, record.joined_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(record)
}

fn upsert_joined_logical_channel(
    conn: &sqlite::Connection,
    payload: &ChannelInvitePayload,
) -> Result<LogicalChannelRecord, String> {
    let now = unix_now();
    let mut stmt = conn
        .prepare(
            "INSERT INTO logical_channels
             (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
             VALUES (?, ?, ?, 'member', 1, ?, ?)
             ON CONFLICT(backing_channel_id) DO UPDATE SET
               logical_id = CASE
                   WHEN logical_channels.role = 'owner' THEN logical_channels.logical_id
                   ELSE excluded.logical_id
               END,
               name = CASE
                   WHEN logical_channels.role = 'owner' THEN logical_channels.name
                   ELSE excluded.name
               END,
               role = CASE
                   WHEN logical_channels.role = 'owner' THEN 'owner'
                   ELSE 'member'
               END,
               joined_at = excluded.joined_at",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, payload.logical_id.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, payload.backing_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, payload.name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, now))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;

    read_logical_channel(conn, payload.backing_channel_id)?
        .ok_or_else(|| "Failed to persist TeraRelay channel membership".to_string())
}

fn next_display_order(conn: &sqlite::Connection) -> Result<i64, String> {
    let mut stmt = conn
        .prepare("SELECT COALESCE(MAX(display_order), -1) + 1 FROM folder_metadata")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        return stmt.read::<i64, _>(0).map_err(|e| e.to_string());
    }
    Ok(0)
}

fn upsert_folder_metadata(
    conn: &sqlite::Connection,
    channel_id: i64,
    name: &str,
    username: Option<&str>,
    is_public: bool,
) -> Result<i32, String> {
    let existing_order = {
        let mut stmt = conn
            .prepare("SELECT display_order FROM folder_metadata WHERE channel_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            Some(stmt.read::<i64, _>(0).map_err(|e| e.to_string())?)
        } else {
            None
        }
    };
    let display_order = match existing_order {
        Some(order) => order,
        None => next_display_order(conn)?,
    };

    let mut stmt = conn
        .prepare(
            "INSERT INTO folder_metadata
             (channel_id, name, username, is_public, display_order, group_id)
             VALUES (?, ?, ?, ?, ?, NULL)
             ON CONFLICT(channel_id) DO UPDATE SET
               name = excluded.name,
               username = excluded.username,
               is_public = excluded.is_public",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, name))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, username))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((4, if is_public { 1 } else { 0 }))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((5, display_order))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(display_order as i32)
}

fn extract_private_invite_hash(link: &str) -> Option<String> {
    let trimmed = link.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://t.me/+") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("http://t.me/+") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("https://t.me/joinchat/") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("http://t.me/joinchat/") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    None
}

fn extract_public_username(link: &str) -> Option<String> {
    let trimmed = link.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://t.me/")
        .or_else(|| trimmed.strip_prefix("http://t.me/"))?;
    if rest.is_empty() || rest.starts_with('+') || rest.starts_with("joinchat/") {
        return None;
    }
    Some(rest.trim_start_matches('@').to_string())
}

async fn find_channel_peer(
    client: &grammers_client::Client,
    expected_id: i64,
    state: &TelegramState,
) -> Result<Option<Peer>, String> {
    if let Some(peer) = state.peer_cache.read().await.get(&expected_id).cloned() {
        return Ok(Some(peer));
    }

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
        if let Peer::Channel(c) = &dialog.peer {
            state
                .peer_cache
                .write()
                .await
                .insert(c.raw.id, dialog.peer.clone());
            if c.raw.id == expected_id {
                return Ok(Some(dialog.peer));
            }
        }
    }
    Ok(None)
}

async fn join_backing_channel(
    client: &grammers_client::Client,
    payload: &ChannelInvitePayload,
    state: &TelegramState,
) -> Result<(Peer, bool), String> {
    if let Some(peer) = find_channel_peer(client, payload.backing_channel_id, state).await? {
        return Ok((peer, true));
    }

    if let Some(hash) = extract_private_invite_hash(&payload.telegram_link) {
        client
            .invoke(&tl::functions::messages::ImportChatInvite { hash })
            .await
            .map_err(|e| format!("Failed to join backing Telegram storage: {}", map_error(e)))?;
    } else if let Some(username) = extract_public_username(&payload.telegram_link) {
        let resolved = client
            .invoke(&tl::functions::contacts::ResolveUsername {
                username,
                referer: None,
            })
            .await
            .map_err(|e| format!("Failed to resolve public backing channel: {}", map_error(e)))?;

        let tl::enums::contacts::ResolvedPeer::Peer(resolved) = resolved;
        let mut input_channel = None;
        for chat in resolved.chats {
            if let tl::enums::Chat::Channel(channel) = chat {
                if channel.id == payload.backing_channel_id {
                    let access_hash = channel
                        .access_hash
                        .ok_or_else(|| "Public backing channel has no access hash".to_string())?;
                    input_channel =
                        Some(tl::enums::InputChannel::Channel(tl::types::InputChannel {
                            channel_id: channel.id,
                            access_hash,
                        }));
                    break;
                }
            }
        }
        let input_channel = input_channel.ok_or_else(|| {
            "The public invite resolved to a different Telegram channel".to_string()
        })?;
        client
            .invoke(&tl::functions::channels::JoinChannel {
                channel: input_channel,
            })
            .await
            .map_err(|e| format!("Failed to join public backing channel: {}", map_error(e)))?;
    } else {
        return Err("Unsupported or malformed Telegram backing invite".to_string());
    }

    let peer = find_channel_peer(client, payload.backing_channel_id, state)
        .await?
        .ok_or_else(|| {
            "Telegram accepted the invite, but the expected backing channel was not found"
                .to_string()
        })?;
    Ok((peer, false))
}

fn decode_invite(invite: &str) -> Result<ChannelInvitePayload, String> {
    let encoded = invite
        .trim()
        .strip_prefix(INVITE_SCHEME_PREFIX)
        .unwrap_or(invite.trim());
    if encoded.is_empty() {
        return Err("Empty TeraRelay channel invite".to_string());
    }

    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "Invalid TeraRelay channel invite encoding".to_string())?;
    let payload: ChannelInvitePayload = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid TeraRelay channel invite payload".to_string())?;
    if payload.v != INVITE_VERSION {
        return Err(format!(
            "Unsupported TeraRelay invite version {}",
            payload.v
        ));
    }
    if payload.logical_id.len() < 16 || payload.name.trim().is_empty() {
        return Err("Incomplete TeraRelay channel invite".to_string());
    }
    let expected = invite_checksum(
        payload.v,
        &payload.logical_id,
        &payload.name,
        payload.backing_channel_id,
        &payload.telegram_link,
        &payload.role,
    );
    if expected != payload.checksum {
        return Err("TeraRelay channel invite checksum mismatch".to_string());
    }
    Ok(payload)
}

async fn export_raw_telegram_invite(
    client: &grammers_client::Client,
    folder_id: i64,
    state: &TelegramState,
) -> Result<String, String> {
    let peer = resolve_peer(client, Some(folder_id), &state.peer_cache).await?;
    let Peer::Channel(c) = &peer else {
        return Err("Only Telegram channels can back a TeraRelay channel".to_string());
    };

    if let Some(username) = &c.raw.username {
        return Ok(format!("https://t.me/{username}"));
    }

    let access_hash = c
        .raw
        .access_hash
        .ok_or_else(|| "No access hash for backing channel".to_string())?;
    let result = client
        .invoke(&tl::functions::messages::ExportChatInvite {
            peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: c.raw.id,
                access_hash,
            }),
            legacy_revoke_permanent: false,
            request_needed: false,
            expire_date: None,
            usage_limit: None,
            title: Some("TeraRelay channel invite".to_string()),
            subscription_pricing: None,
        })
        .await
        .map_err(|e| format!("Failed to export Telegram storage invite: {}", map_error(e)))?;

    match result {
        tl::enums::ExportedChatInvite::ChatInviteExported(invite) => Ok(invite.link),
        tl::enums::ExportedChatInvite::ChatInvitePublicJoinRequests => {
            Err("This Telegram channel uses join requests and cannot be used for direct TeraRelay membership yet".to_string())
        }
    }
}

#[tauri::command]
pub async fn cmd_export_tera_channel_invite(
    folder_id: i64,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<TeraChannelInviteInfo, String> {
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;

    let record = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let name = {
            let mut stmt = conn
                .prepare("SELECT name FROM folder_metadata WHERE channel_id = ?")
                .map_err(|e: sqlite::Error| e.to_string())?;
            stmt.bind((1, folder_id))
                .map_err(|e: sqlite::Error| e.to_string())?;
            if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
                stmt.read::<String, _>(0).map_err(|e| e.to_string())?
            } else {
                format!("Channel {folder_id}")
            }
        };
        ensure_logical_channel(&conn, folder_id, &name, "owner")?
    };

    if record.role != "owner" {
        return Err("Only the TeraRelay channel owner can create member invites".to_string());
    }

    let telegram_link = export_raw_telegram_invite(&client, folder_id, state.inner()).await?;
    let role = "member".to_string();
    let checksum = invite_checksum(
        INVITE_VERSION,
        &record.logical_id,
        &record.name,
        record.backing_channel_id,
        &telegram_link,
        &role,
    );
    let payload = ChannelInvitePayload {
        v: INVITE_VERSION,
        logical_id: record.logical_id.clone(),
        name: record.name.clone(),
        backing_channel_id: record.backing_channel_id,
        telegram_link: telegram_link.clone(),
        role: role.clone(),
        checksum,
    };
    let encoded = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&payload)
            .map_err(|e| format!("Failed to encode TeraRelay invite: {e}"))?,
    );

    Ok(TeraChannelInviteInfo {
        link: format!("{INVITE_SCHEME_PREFIX}{encoded}"),
        telegram_link,
        logical_id: record.logical_id,
        channel_name: record.name,
        backing_channel_id: record.backing_channel_id,
        role,
    })
}

#[tauri::command]
pub async fn cmd_join_tera_channel(
    invite: String,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<JoinChannelResult, String> {
    let payload = decode_invite(&invite)?;
    if payload.role != "member" {
        return Err("Unsupported TeraRelay invite role".to_string());
    }

    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;

    let (peer, already_joined) = join_backing_channel(&client, &payload, state.inner()).await?;
    let (username, is_public) = match &peer {
        Peer::Channel(c) => (c.raw.username.clone(), c.raw.username.is_some()),
        _ => return Err("Invite did not resolve to a Telegram channel".to_string()),
    };

    let display_order = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let record = upsert_joined_logical_channel(&conn, &payload)?;
        if record.logical_id != payload.logical_id {
            return Err("Logical channel identity mismatch".to_string());
        }
        upsert_folder_metadata(
            &conn,
            payload.backing_channel_id,
            &payload.name,
            username.as_deref(),
            is_public,
        )?
    };

    Ok(JoinChannelResult {
        folder: FolderMetadata {
            id: payload.backing_channel_id,
            parent_id: None,
            name: payload.name,
            username,
            is_public,
            group_id: None,
            display_order,
            role: Some("member".to_string()),
        },
        logical_id: payload.logical_id,
        role: "member".to_string(),
        already_joined,
    })
}

#[tauri::command]
pub fn cmd_get_logical_channel(
    folder_id: i64,
    db_pool: State<'_, DbConnection>,
) -> Result<Option<LogicalChannelRecord>, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    read_logical_channel(&conn, folder_id)
}

#[tauri::command]
pub async fn cmd_get_tera_channel_members(
    folder_id: i64,
    state: State<'_, TelegramState>,
) -> Result<Vec<TeraChannelMember>, String> {
    let client = {
        let guard = state.client.lock().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "Telegram is not connected".to_string())?
    };
    let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
    let mut participants = client.iter_participants(peer);
    let mut members = Vec::new();

    while let Some(participant) = participants.next().await.map_err(map_error)? {
        let username = participant.user.username().map(ToString::to_string);
        let full_name = participant.user.full_name();
        let display_name = if full_name.trim().is_empty() {
            username
                .as_ref()
                .map(|value| format!("@{value}"))
                .unwrap_or_else(|| "Telegram user".to_string())
        } else {
            full_name
        };
        members.push(TeraChannelMember {
            display_name,
            username,
        });
    }

    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tera_invite_round_trip_and_checksum() {
        let role = "member";
        let checksum = invite_checksum(
            INVITE_VERSION,
            "0123456789abcdef0123456789abcdef",
            "test",
            123,
            "https://t.me/+abc",
            role,
        );
        let payload = ChannelInvitePayload {
            v: INVITE_VERSION,
            logical_id: "0123456789abcdef0123456789abcdef".into(),
            name: "test".into(),
            backing_channel_id: 123,
            telegram_link: "https://t.me/+abc".into(),
            role: role.into(),
            checksum,
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let decoded = decode_invite(&format!("{INVITE_SCHEME_PREFIX}{encoded}")).unwrap();
        assert_eq!(decoded.logical_id, payload.logical_id);
        assert_eq!(decoded.backing_channel_id, 123);
    }

    #[test]
    fn private_invite_hash_parser_accepts_current_telegram_forms() {
        assert_eq!(
            extract_private_invite_hash("https://t.me/+abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_private_invite_hash("https://t.me/joinchat/abc123").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn public_username_parser_rejects_private_links() {
        assert_eq!(
            extract_public_username("https://t.me/mychannel").as_deref(),
            Some("mychannel")
        );
        assert!(extract_public_username("https://t.me/+abc123").is_none());
    }

    fn test_channel_db(role: &str) -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY,
                backing_channel_id INTEGER NOT NULL UNIQUE,
                name TEXT NOT NULL,
                role TEXT NOT NULL,
                storage_version INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                joined_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO logical_channels
                 (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
                 VALUES ('0123456789abcdef0123456789abcdef', 123, 'test', ?, 1, 1, 1)",
            )
            .unwrap();
        stmt.bind((1, role)).unwrap();
        stmt.next().unwrap();
        drop(stmt);
        conn
    }

    #[test]
    fn member_role_blocks_channel_writes() {
        let conn = test_channel_db("member");
        let err = require_owner_role(&conn, 123).unwrap_err();
        assert!(err.contains("read-only"));
        assert!(ensure_channel_can_upload(&conn, Some(123)).is_err());
    }

    #[test]
    fn owner_role_allows_channel_writes() {
        let conn = test_channel_db("owner");
        require_owner_role(&conn, 123).unwrap();
        ensure_channel_can_upload(&conn, Some(123)).unwrap();
    }

    #[test]
    fn personal_vault_and_legacy_unscoped_paths_remain_allowed() {
        let conn = test_channel_db("member");
        ensure_channel_can_upload(&conn, None).unwrap();
        require_owner_role(&conn, 999).unwrap();
    }

    #[test]
    fn tampered_invite_checksum_is_rejected() {
        let role = "member";
        let payload = ChannelInvitePayload {
            v: INVITE_VERSION,
            logical_id: "0123456789abcdef0123456789abcdef".into(),
            name: "test".into(),
            backing_channel_id: 123,
            telegram_link: "https://t.me/+abc".into(),
            role: role.into(),
            checksum: "0".repeat(64),
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let err = decode_invite(&format!("{INVITE_SCHEME_PREFIX}{encoded}")).unwrap_err();
        assert!(err.contains("checksum"));
    }

    #[test]
    fn malformed_invite_is_rejected() {
        assert!(decode_invite("terarelay://join/not-valid-@@@").is_err());
        assert!(decode_invite("").is_err());
    }
}
