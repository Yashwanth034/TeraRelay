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

fn validate_share_password(password: Option<&str>) -> Result<(), String> {
    if password.is_some_and(|value| value.trim().is_empty()) {
        return Err("Password protection requires a non-empty password".to_string());
    }
    Ok(())
}

fn share_expiration(created_at: i64, expiry_hours: Option<i64>) -> Result<Option<i64>, String> {
    expiry_hours
        .map(|hours| {
            if hours <= 0 {
                return Err("Share expiry must be a positive number of hours".to_string());
            }
            hours
                .checked_mul(3600)
                .and_then(|duration| created_at.checked_add(duration))
                .ok_or_else(|| "Share expiry is out of range".to_string())
        })
        .transpose()
}

#[tauri::command]
#[expect(
    clippy::too_many_arguments,
    reason = "Tauri share IPC parameters and injected states are frontend API contracts"
)]
pub async fn cmd_create_share(
    folder_id: Option<i64>,
    message_id: i32,
    file_name: String,
    file_size: i64,
    password: Option<String>,
    expiry_hours: Option<i64>,
    stream_config: State<'_, crate::commands::streaming::StreamConfig>,
    db_pool: State<'_, DbConnection>,
) -> Result<ShareInfo, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        return Err(
            "Local share links are desktop-only in this release; worldwide sharing is not enabled yet."
                .to_string(),
        );
    }

    validate_share_password(password.as_deref())?;
    let created_at = chrono::Utc::now().timestamp();
    let expires_at = share_expiration(created_at, expiry_hours)?;
    let token = generate_share_token();

    // A supplied password must always produce a hash. Never silently downgrade
    // a requested protected share to an unprotected one.
    let password_hash = password.as_deref().map(hash_password).transpose()?;

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

    let link = format!("http://127.0.0.1:{}/d/{}", stream_config.port, token);

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
pub async fn cmd_list_shares(
    db_pool: State<'_, DbConnection>,
    stream_config: State<'_, crate::commands::streaming::StreamConfig>,
) -> Result<Vec<ShareInfo>, String> {
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
        // A database decoding failure is not evidence that a share is
        // unprotected or non-expiring; propagate it instead of defaulting.
        let has_password = match stmt
            .read::<sqlite::Value, _>("password_hash")
            .map_err(|e| format!("Invalid share password metadata: {e}"))?
        {
            sqlite::Value::Null => false,
            sqlite::Value::String(_) => true,
            _ => return Err("Invalid share password metadata type".to_string()),
        };
        let expires_at = match stmt
            .read::<sqlite::Value, _>("expires_at")
            .map_err(|e| format!("Invalid share expiration metadata: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::Integer(expiry) => Some(expiry),
            _ => return Err("Invalid share expiration metadata type".to_string()),
        };
        let file_name = stmt
            .read::<String, _>("file_name")
            .map_err(|e| e.to_string())?;
        let file_size = stmt
            .read::<i64, _>("file_size")
            .map_err(|e| e.to_string())?;
        let created_at = stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?;
        let link = format!("http://127.0.0.1:{}/d/{}", stream_config.port, id);

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
            // The share download path only accepts an actual INTEGER or NULL
            // folder. Revocation must not coerce malformed channel identity.
            match stmt
                .read::<sqlite::Value, _>("folder_id")
                .map_err(|e| format!("Invalid share folder ID: {e}"))?
            {
                sqlite::Value::Null => None,
                sqlite::Value::Integer(folder_id) => Some(folder_id),
                _ => return Err("Invalid share folder ID type".to_string()),
            }
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

    #[test]
    fn protected_share_requires_a_nonblank_password() {
        assert!(validate_share_password(Some("")).is_err());
        assert!(validate_share_password(Some("  \t ")).is_err());
        assert!(validate_share_password(Some("valid password")).is_ok());
        assert!(validate_share_password(None).is_ok());
    }

    #[test]
    fn share_expiration_rejects_nonpositive_and_overflowing_hours() {
        assert_eq!(share_expiration(1_000, None).unwrap(), None);
        assert_eq!(share_expiration(1_000, Some(24)).unwrap(), Some(87_400));
        assert!(share_expiration(1_000, Some(0)).is_err());
        assert!(share_expiration(1_000, Some(-1)).is_err());
        assert!(share_expiration(1_000, Some(i64::MAX)).is_err());
        assert!(share_expiration(i64::MAX - 100, Some(1)).is_err());
    }

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

    #[test]
    fn revocation_refuses_coerced_or_corrupt_share_folder_identity() {
        // Even a channel owner must not authorize a share whose folder has
        // stopped being an actual SQLite INTEGER or NULL.
        let conn = test_db("owner", Some(123));
        assert!(ensure_share_can_be_revoked(&conn, "share-1").is_ok());
        // An INTEGER-affinity SQLite column normalizes numeric text into an
        // integer on write. That legitimate normalization is not corruption.
        conn.execute("UPDATE shared_links SET folder_id = '123'")
            .unwrap();
        ensure_share_can_be_revoked(&conn, "share-1").unwrap();

        for update in [
            "UPDATE shared_links SET folder_id = X'313233'",
            "UPDATE shared_links SET folder_id = 'not-a-folder'",
            "UPDATE shared_links SET folder_id = 123.5",
        ] {
            conn.execute(update).unwrap();
            assert!(
                ensure_share_can_be_revoked(&conn, "share-1").is_err(),
                "{update}: invalid type must not pass authorization"
            );
            conn.execute("UPDATE shared_links SET folder_id = 123")
                .unwrap();
            ensure_share_can_be_revoked(&conn, "share-1").unwrap();
        }
    }

    #[test]
    fn revocation_requires_an_existing_nonrevoked_share() {
        let conn = test_db("owner", Some(123));
        assert!(ensure_share_can_be_revoked(&conn, "missing-share").is_err());
        conn.execute("UPDATE shared_links SET revoked = 1 WHERE id = 'share-1'")
            .unwrap();
        assert!(ensure_share_can_be_revoked(&conn, "share-1").is_err());
    }
}
