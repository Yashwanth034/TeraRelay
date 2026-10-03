use crate::bandwidth::BandwidthManager;
use grammers_client::types::Peer;
use grammers_client::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::State;
use tokio::sync::RwLock;

/// Resolve a folder_id to a Telegram Peer, using the cache for O(1) lookups.
///
/// - `folder_id == None` → returns the user's own peer (Saved Messages)
/// - Cache hit → returns immediately without any network call
/// - Cache miss → scans all dialogs, populates the cache, and returns
pub async fn resolve_peer(
    client: &Client,
    folder_id: Option<i64>,
    peer_cache: &Arc<RwLock<HashMap<i64, Peer>>>,
) -> Result<Peer, String> {
    if let Some(fid) = folder_id {
        // Fast path: check cache
        {
            let cache = peer_cache.read().await;
            if let Some(peer) = cache.get(&fid) {
                return Ok(peer.clone());
            }
        }

        // Slow path: scan dialogs and populate cache
        log::debug!("Peer cache miss for folder_id={}, scanning dialogs...", fid);
        let mut found: Option<Peer> = None;
        let mut dialogs = client.iter_dialogs();
        let mut discovered = HashMap::new();
        while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
            let peer_id = match &dialog.peer {
                Peer::Channel(c) => Some(c.raw.id),
                Peer::User(u) => Some(u.raw.id()),
                _ => None,
            };
            if let Some(id) = peer_id {
                discovered.insert(id, dialog.peer.clone());
                if id == fid {
                    found = Some(dialog.peer.clone());
                    // Don't break — keep scanning to warm the cache
                }
            }
        }

        {
            let mut cache = peer_cache.write().await;
            cache.extend(discovered);
        }

        found.ok_or_else(|| format!("Folder/Chat {} not found", fid))
    } else {
        match client.get_me().await {
            Ok(me) => Ok(Peer::User(me)),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Clear the peer cache (called on logout)
pub async fn clear_peer_cache(peer_cache: &Arc<RwLock<HashMap<i64, Peer>>>) {
    peer_cache.write().await.clear();
}

#[tauri::command]
pub fn cmd_log(message: String) {
    log::info!("[FRONTEND] {}", message);
}

#[tauri::command]
pub fn cmd_get_bandwidth(
    bw_state: State<'_, Arc<BandwidthManager>>,
) -> crate::bandwidth::BandwidthStats {
    bw_state.get_stats()
}

/// TDLib represents ordinary server message IDs as the Telegram server
/// message ID shifted left by 20 bits. Grammers and Telegram MTProto APIs use
/// the canonical server-side 32-bit message ID directly.
///
/// Only call this for a final message returned by TDLib after
/// updateMessageSendSucceeded, never for a pending/local TDLib message ID.
pub fn tdlib_message_id_to_server_id(tdlib_message_id: i64) -> Result<i64, String> {
    const SHIFT: u32 = 20;
    const LOW_MASK: i64 = (1_i64 << SHIFT) - 1;

    if tdlib_message_id <= 0 {
        return Err("TDLib returned an invalid message ID".to_string());
    }
    if tdlib_message_id & LOW_MASK != 0 {
        return Err(format!(
            "TDLib returned a non-server message ID: {}",
            tdlib_message_id
        ));
    }

    let server_id = tdlib_message_id >> SHIFT;
    if server_id <= 0 || server_id > i32::MAX as i64 {
        return Err(format!(
            "TDLib message ID {} could not be converted to a Telegram server message ID",
            tdlib_message_id
        ));
    }
    Ok(server_id)
}

#[cfg(test)]
mod message_id_tests {
    use super::tdlib_message_id_to_server_id;

    #[test]
    fn final_tdlib_message_id_converts_to_server_id() {
        assert_eq!(tdlib_message_id_to_server_id(123_i64 << 20).unwrap(), 123);
    }

    #[test]
    fn tdlib_message_id_conversion_rejects_local_or_invalid_ids() {
        assert!(tdlib_message_id_to_server_id(0).is_err());
        assert!(tdlib_message_id_to_server_id(-1).is_err());
        assert!(tdlib_message_id_to_server_id((123_i64 << 20) + 1).is_err());
    }
}

pub fn map_error(e: impl std::fmt::Display) -> String {
    let err_str = e.to_string();
    if err_str.contains("FLOOD_WAIT") || err_str.contains("FLOOD_PREMIUM_WAIT") {
        // Grammers normally renders Telegram RPC wait errors with
        // "(value: N)". Normalize both ordinary and premium waits so the
        // transfer scheduler applies the same bounded backoff.
        if let Some(start) = err_str.find("(value: ") {
            let rest = &err_str[start + 8..];
            if let Some(end) = rest.find(')') {
                if let Ok(seconds) = rest[..end].parse::<i64>() {
                    return format!("FLOOD_WAIT_{}", seconds);
                }
            }
        }
        return "FLOOD_WAIT_60".to_string();
    }
    err_str
}
