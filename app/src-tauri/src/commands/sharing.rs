use crate::db::DbConnection;
use rand::Rng;
use serde::Serialize;
use tauri::State;

#[derive(Debug, Serialize)]
pub struct ShareInfo {
    pub id: String,
    pub file_name: String,
    pub file_size: i64,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub has_password: bool,
    pub link: String,
}

fn generate_share_token() -> String {
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..16).map(|_| rng.random()).collect();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Hash a password using bcrypt (cost factor 12).
/// bcrypt embeds the salt in the output hash string, so no separate salt storage is needed.
fn hash_password(password: &str) -> Result<String, String> {
    bcrypt::hash(password, 12).map_err(|e| format!("Password hashing failed: {}", e))
}

#[tauri::command]
pub async fn cmd_create_share(
    folder_id: Option<i64>,
    message_id: i32,
    file_name: String,
    file_size: i64,
    password: Option<String>,
    expiry_hours: Option<i64>,
    db_pool: State<'_, DbConnection>,
) -> Result<ShareInfo, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        return Err(
            "Local share links are desktop-only in this release; worldwide sharing is not enabled yet."
                .to_string(),
        );
    }

    let token = generate_share_token();
    let created_at = chrono::Utc::now().timestamp();
    let expires_at = expiry_hours.map(|hours| created_at + hours * 3600);

    let password_hash = if let Some(ref pwd) = password {
        if pwd.is_empty() {
            None
        } else {
            // bcrypt embeds the salt in the hash; password_salt column set to NULL.
            let hash = hash_password(pwd)?;
            Some(hash)
        }
    } else {
        None
    };

    let conn = db_pool.lock().map_err(|e| e.to_string())?;
    crate::commands::logical_channels::ensure_channel_can_upload(&conn, folder_id)?;

    let mut stmt = conn.prepare(
        "INSERT INTO shared_links (id, folder_id, message_id, file_name, file_size, password_hash, password_salt, expires_at, revoked, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?)"
    ).map_err(|e| e.to_string())?;

    stmt.bind((1, token.as_str())).map_err(|e| e.to_string())?;
    stmt.bind((2, folder_id)).map_err(|e| e.to_string())?;
    stmt.bind((3, message_id as i64))
        .map_err(|e| e.to_string())?;
    stmt.bind((4, file_name.as_str()))
        .map_err(|e| e.to_string())?;
    stmt.bind((5, file_size)).map_err(|e| e.to_string())?;
    stmt.bind((6, password_hash.as_deref()))
        .map_err(|e| e.to_string())?;
    stmt.bind::<(usize, Option<&str>)>((7, None))
        .map_err(|e| e.to_string())?;
    stmt.bind((8, expires_at)).map_err(|e| e.to_string())?;
    stmt.bind((9, created_at)).map_err(|e| e.to_string())?;

    stmt.next().map_err(|e| e.to_string())?;

    let link = format!("http://127.0.0.1:{}/d/{}", crate::STREAM_PORT, token);

    Ok(ShareInfo {
        id: token,
        file_name,
        file_size,
        created_at,
        expires_at,
        has_password: password_hash.is_some(),
        link,
    })
}

#[tauri::command]
pub async fn cmd_list_shares(db_pool: State<'_, DbConnection>) -> Result<Vec<ShareInfo>, String> {
    let conn = db_pool.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, folder_id, message_id, file_name, file_size, password_hash, expires_at, created_at
             FROM shared_links WHERE revoked = 0 ORDER BY created_at DESC"
        )
        .map_err(|e| e.to_string())?;

    let mut shares = Vec::new();
    while let sqlite::State::Row = stmt.next().map_err(|e| e.to_string())? {
        let id = stmt.read::<String, _>("id").map_err(|e| e.to_string())?;
        let has_password = stmt
            .read::<Option<String>, _>("password_hash")
            .ok()
            .flatten()
            .is_some();
        let expires_at = stmt.read::<Option<i64>, _>("expires_at").ok().flatten();
        let file_name = stmt
            .read::<String, _>("file_name")
            .map_err(|e| e.to_string())?;
        let file_size = stmt
            .read::<i64, _>("file_size")
            .map_err(|e| e.to_string())?;
        let created_at = stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?;
        let link = format!("http://127.0.0.1:{}/d/{}", crate::STREAM_PORT, id);

        shares.push(ShareInfo {
            id,
            file_name,
            file_size,
            created_at,
            expires_at,
            has_password,
            link,
        });
    }

    Ok(shares)
}

fn ensure_share_can_be_revoked(conn: &sqlite::Connection, id: &str) -> Result<(), String> {
    let folder_id = {
        let mut stmt = conn
            .prepare("SELECT folder_id FROM shared_links WHERE id = ? AND revoked = 0")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, id)).map_err(|e| e.to_string())?;
        if let sqlite::State::Row = stmt.next().map_err(|e| e.to_string())? {
            stmt.read::<Option<i64>, _>("folder_id")
                .map_err(|e| e.to_string())?
        } else {
            return Err("Share link not found or already revoked".to_string());
        }
    };

    crate::commands::logical_channels::ensure_channel_can_upload(conn, folder_id)
}

#[tauri::command]
pub async fn cmd_revoke_share(id: String, db_pool: State<'_, DbConnection>) -> Result<(), String> {
    let conn = db_pool.lock().map_err(|e| e.to_string())?;
    ensure_share_can_be_revoked(&conn, &id)?;

    let mut stmt = conn
        .prepare("UPDATE shared_links SET revoked = 1 WHERE id = ?")
        .map_err(|e| e.to_string())?;
    stmt.bind((1, id.as_str())).map_err(|e| e.to_string())?;
    stmt.next().map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db(role: &str, share_folder_id: Option<i64>) -> sqlite::Connection {
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
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY,
                folder_id INTEGER,
                revoked INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();

        let mut channel = conn
            .prepare(
                "INSERT INTO logical_channels
                 (logical_id, backing_channel_id, name, role, storage_version, created_at, joined_at)
                 VALUES ('0123456789abcdef0123456789abcdef', 123, 'test', ?, 1, 1, 1)",
            )
            .unwrap();
        channel.bind((1, role)).unwrap();
        channel.next().unwrap();
        drop(channel);

        let mut share = conn
            .prepare("INSERT INTO shared_links (id, folder_id, revoked) VALUES ('share-1', ?, 0)")
            .unwrap();
        share.bind((1, share_folder_id)).unwrap();
        share.next().unwrap();
        drop(share);
        conn
    }

    #[test]
    fn member_cannot_revoke_channel_share() {
        let conn = test_db("member", Some(123));
        assert!(ensure_share_can_be_revoked(&conn, "share-1").is_err());
    }

    #[test]
    fn owner_can_revoke_channel_share() {
        let conn = test_db("owner", Some(123));
        ensure_share_can_be_revoked(&conn, "share-1").unwrap();
    }

    #[test]
    fn unscoped_share_can_still_be_revoked() {
        let conn = test_db("member", None);
        ensure_share_can_be_revoked(&conn, "share-1").unwrap();
    }
}
