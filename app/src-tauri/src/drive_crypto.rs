use crate::db::DbConnection;
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tauri::{AppHandle, Manager, State};
use zeroize::{Zeroize, Zeroizing};

pub const DRIVE_ENCRYPTION_VERSION: u32 = 1;
pub const DRIVE_PLAINTEXT_CHUNK_SIZE: u64 = 16 * 1024 * 1024;
const DRIVE_KEYRING_SERVICE: &str = "io.terarelay.desktop.drive";
const KEYRING_VALUE_VERSION: u32 = 1;
const WRAP_AAD: &[u8] = b"TeraRelay Drive master key v1";
const MASTER_KEY_CHECK_AAD: &[u8] = b"TeraRelay Drive master key check v1";
const CHUNK_MAGIC: &[u8; 5] = b"TRDC1";
const ARGON_MEMORY_KIB: u32 = 64 * 1024;
const ARGON_ITERATIONS: u32 = 3;
const ARGON_PARALLELISM: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveCryptoEnvelope {
    pub version: u32,
    pub updated_at: i64,
    pub salt_b64: String,
    pub nonce_b64: String,
    pub wrapped_master_key_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_check_b64: Option<String>,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveCryptoStatus {
    pub configured: bool,
    pub unlocked: bool,
    pub version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDriveKey {
    version: u32,
    master_key_b64: String,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn init_crypto_schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_crypto_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            envelope_json TEXT,
            updated_at INTEGER NOT NULL DEFAULT 0
        );
        INSERT OR IGNORE INTO drive_crypto_state(singleton, envelope_json, updated_at)
            VALUES (1, NULL, 0);",
    )
    .map_err(|e: sqlite::Error| e.to_string())
}

pub fn load_crypto_envelope(
    conn: &sqlite::Connection,
) -> Result<Option<DriveCryptoEnvelope>, String> {
    init_crypto_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT envelope_json FROM drive_crypto_state WHERE singleton = 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if !matches!(
        stmt.next().map_err(|e: sqlite::Error| e.to_string())?,
        sqlite::State::Row
    ) {
        return Ok(None);
    }
    let raw = stmt
        .read::<Option<String>, _>(0)
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty());
    raw.map(|value| {
        serde_json::from_str(&value)
            .map_err(|e| format!("Invalid TeraRelay Drive encryption metadata: {e}"))
    })
    .transpose()
}

pub fn save_crypto_envelope(
    conn: &sqlite::Connection,
    envelope: Option<&DriveCryptoEnvelope>,
) -> Result<(), String> {
    init_crypto_schema(conn)?;
    let encoded = envelope
        .map(|value| {
            serde_json::to_string(value)
                .map_err(|e| format!("Could not encode Drive encryption metadata: {e}"))
        })
        .transpose()?;
    let updated_at = envelope.map(|value| value.updated_at).unwrap_or(0);
    let mut stmt = conn
        .prepare(
            "UPDATE drive_crypto_state
             SET envelope_json = ?, updated_at = ?
             WHERE singleton = 1",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, encoded.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, updated_at))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn drive_profile_key(app: &AppHandle) -> Result<String, String> {
    use sha2::Digest;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve TeraRelay app data: {e}"))?;
    let digest = Sha256::digest(app_data.to_string_lossy().as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("drive-master-{suffix}"))
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn drive_keyring_entry(app: &AppHandle) -> Result<keyring::Entry, String> {
    keyring::Entry::new(DRIVE_KEYRING_SERVICE, &drive_profile_key(app)?)
        .map_err(|e| format!("OS credential storage is unavailable: {e}"))
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn save_master_key(app: &AppHandle, master_key: &[u8; 32]) -> Result<(), String> {
    let stored = StoredDriveKey {
        version: KEYRING_VALUE_VERSION,
        master_key_b64: base64::engine::general_purpose::STANDARD.encode(master_key),
    };
    let encoded = serde_json::to_string(&stored)
        .map_err(|e| format!("Could not encode Drive encryption key: {e}"))?;
    drive_keyring_entry(app)?
        .set_password(&encoded)
        .map_err(|e| format!("Could not save Drive encryption key securely: {e}"))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn save_master_key(_app: &AppHandle, _master_key: &[u8; 32]) -> Result<(), String> {
    Err("OS credential storage is unavailable on this platform".to_string())
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn load_master_key(app: &AppHandle) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    let raw = match drive_keyring_entry(app)?.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(format!("Could not read Drive encryption key: {error}")),
    };
    let stored: StoredDriveKey = serde_json::from_str(&raw)
        .map_err(|e| format!("Saved Drive encryption key is invalid: {e}"))?;
    if stored.version != KEYRING_VALUE_VERSION {
        return Err("Saved Drive encryption key uses an unsupported version".to_string());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stored.master_key_b64)
        .map_err(|e| format!("Saved Drive encryption key is invalid: {e}"))?;
    let key: [u8; 32] = decoded
        .try_into()
        .map_err(|_| "Saved Drive encryption key has an invalid length".to_string())?;
    Ok(Some(Zeroizing::new(key)))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn load_master_key(_app: &AppHandle) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    Ok(None)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn delete_master_key(app: &AppHandle) -> Result<(), String> {
    match drive_keyring_entry(app)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Could not lock TeraRelay Drive: {error}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn delete_master_key(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

fn master_key_check_b64(master_key: &[u8; 32]) -> Result<String, String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(master_key)
        .map_err(|_| "Could not initialize Drive master-key verifier".to_string())?;
    mac.update(MASTER_KEY_CHECK_AAD);
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn master_key_matches_envelope(
    master_key: &[u8; 32],
    envelope: &DriveCryptoEnvelope,
) -> Result<bool, String> {
    let Some(expected) = envelope.key_check_b64.as_deref() else {
        // Early unreleased encryption metadata did not carry a verifier. It is
        // still unlockable via the wrapped key; a subsequent passphrase change
        // upgrades the envelope automatically.
        return Ok(true);
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .map_err(|e| format!("Drive master-key verifier is invalid: {e}"))?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(master_key)
        .map_err(|_| "Could not initialize Drive master-key verifier".to_string())?;
    mac.update(MASTER_KEY_CHECK_AAD);
    Ok(mac.verify_slice(&expected).is_ok())
}

pub fn load_verified_master_key(
    app: &AppHandle,
    envelope: &DriveCryptoEnvelope,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    let Some(key) = load_master_key(app)? else {
        return Ok(None);
    };
    if master_key_matches_envelope(&*key, envelope)? {
        Ok(Some(key))
    } else {
        Ok(None)
    }
}

pub fn load_verified_master_key_for_db(
    app: &AppHandle,
    conn: &sqlite::Connection,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    let Some(envelope) = load_crypto_envelope(conn)? else {
        return Ok(None);
    };
    load_verified_master_key(app, &envelope)
}

fn derive_wrap_key(
    passphrase: &str,
    envelope: &DriveCryptoEnvelope,
) -> Result<Zeroizing<[u8; 32]>, String> {
    if passphrase.len() < 12 {
        return Err("Drive passphrase must contain at least 12 characters".to_string());
    }
    let salt = base64::engine::general_purpose::STANDARD
        .decode(&envelope.salt_b64)
        .map_err(|e| format!("Drive encryption salt is invalid: {e}"))?;
    let params = Params::new(
        envelope.memory_kib,
        envelope.iterations,
        envelope.parallelism,
        Some(32),
    )
    .map_err(|e| format!("Drive encryption parameters are invalid: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut output = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(passphrase.as_bytes(), &salt, output.as_mut())
        .map_err(|e| format!("Could not derive Drive encryption key: {e}"))?;
    Ok(output)
}

fn new_envelope(passphrase: &str, master_key: &[u8; 32]) -> Result<DriveCryptoEnvelope, String> {
    if passphrase.len() < 12 {
        return Err("Drive passphrase must contain at least 12 characters".to_string());
    }
    let mut rng = rand::rng();
    let salt: [u8; 16] = rng.random();
    let nonce: [u8; 24] = rng.random();
    let mut envelope = DriveCryptoEnvelope {
        version: DRIVE_ENCRYPTION_VERSION,
        updated_at: now_ms(),
        salt_b64: base64::engine::general_purpose::STANDARD.encode(salt),
        nonce_b64: base64::engine::general_purpose::STANDARD.encode(nonce),
        wrapped_master_key_b64: String::new(),
        key_check_b64: Some(master_key_check_b64(master_key)?),
        memory_kib: ARGON_MEMORY_KIB,
        iterations: ARGON_ITERATIONS,
        parallelism: ARGON_PARALLELISM,
    };
    let wrap_key = derive_wrap_key(passphrase, &envelope)?;
    let cipher = XChaCha20Poly1305::new_from_slice(wrap_key.as_ref())
        .map_err(|_| "Could not initialize Drive encryption".to_string())?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: master_key,
                aad: WRAP_AAD,
            },
        )
        .map_err(|_| "Could not wrap Drive encryption key".to_string())?;
    envelope.wrapped_master_key_b64 = base64::engine::general_purpose::STANDARD.encode(ciphertext);
    Ok(envelope)
}

fn unwrap_master_key(
    passphrase: &str,
    envelope: &DriveCryptoEnvelope,
) -> Result<Zeroizing<[u8; 32]>, String> {
    if envelope.version != DRIVE_ENCRYPTION_VERSION {
        return Err(format!(
            "Unsupported TeraRelay Drive encryption version {}",
            envelope.version
        ));
    }
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(&envelope.nonce_b64)
        .map_err(|e| format!("Drive encryption nonce is invalid: {e}"))?;
    let nonce: [u8; 24] = nonce
        .try_into()
        .map_err(|_| "Drive encryption nonce has an invalid length".to_string())?;
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(&envelope.wrapped_master_key_b64)
        .map_err(|e| format!("Drive wrapped key is invalid: {e}"))?;
    let wrap_key = derive_wrap_key(passphrase, envelope)?;
    let cipher = XChaCha20Poly1305::new_from_slice(wrap_key.as_ref())
        .map_err(|_| "Could not initialize Drive encryption".to_string())?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: WRAP_AAD,
            },
        )
        .map_err(|_| "Drive passphrase is incorrect".to_string())?;
    let key: [u8; 32] = plaintext
        .try_into()
        .map_err(|_| "Drive master key has an invalid length".to_string())?;
    Ok(Zeroizing::new(key))
}

pub fn new_crypto_id() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn chunk_aad(crypto_id: &str, chunk_index: u64, plaintext_len: u64) -> Vec<u8> {
    format!("TeraRelayDriveChunkV1:{crypto_id}:{chunk_index}:{plaintext_len}").into_bytes()
}

pub fn encoded_chunk_len(encryption_version: u32, plaintext_len: u64) -> Result<u64, String> {
    match encryption_version {
        0 => Ok(plaintext_len),
        DRIVE_ENCRYPTION_VERSION => plaintext_len
            .checked_add((CHUNK_MAGIC.len() + 24 + 16) as u64)
            .ok_or_else(|| "Encrypted Drive chunk size overflow".to_string()),
        version => Err(format!(
            "Unsupported TeraRelay Drive encryption version {version}"
        )),
    }
}

pub fn encrypt_chunk(
    master_key: &[u8; 32],
    crypto_id: &str,
    chunk_index: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 24];
    rand::rng().fill(&mut nonce);
    let cipher = XChaCha20Poly1305::new_from_slice(master_key)
        .map_err(|_| "Could not initialize Drive encryption".to_string())?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &chunk_aad(crypto_id, chunk_index, plaintext.len() as u64),
            },
        )
        .map_err(|_| "Could not encrypt Drive chunk".to_string())?;
    let mut encoded = Vec::with_capacity(CHUNK_MAGIC.len() + nonce.len() + ciphertext.len());
    encoded.extend_from_slice(CHUNK_MAGIC);
    encoded.extend_from_slice(&nonce);
    encoded.extend_from_slice(&ciphertext);
    Ok(encoded)
}

pub fn decrypt_chunk(
    master_key: &[u8; 32],
    crypto_id: &str,
    chunk_index: u64,
    plaintext_len: u64,
    encoded: &[u8],
) -> Result<Vec<u8>, String> {
    if encoded.len() < CHUNK_MAGIC.len() + 24 + 16 || &encoded[..CHUNK_MAGIC.len()] != CHUNK_MAGIC {
        return Err("Encrypted Drive chunk header is invalid".to_string());
    }
    let nonce_start = CHUNK_MAGIC.len();
    let nonce_end = nonce_start + 24;
    let nonce: [u8; 24] = encoded[nonce_start..nonce_end]
        .try_into()
        .map_err(|_| "Encrypted Drive chunk nonce is invalid".to_string())?;
    let cipher = XChaCha20Poly1305::new_from_slice(master_key)
        .map_err(|_| "Could not initialize Drive encryption".to_string())?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &encoded[nonce_end..],
                aad: &chunk_aad(crypto_id, chunk_index, plaintext_len),
            },
        )
        .map_err(|_| "Encrypted Drive chunk authentication failed".to_string())?;
    if plaintext.len() as u64 != plaintext_len {
        return Err("Encrypted Drive chunk plaintext length is invalid".to_string());
    }
    Ok(plaintext)
}

pub fn public_content_fingerprint(total_size: u64, plaintext_chunk_hashes: &[[u8; 32]]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(b"TeraRelayDriveFingerprintV1");
    hasher.update(total_size.to_le_bytes());
    hasher.update((plaintext_chunk_hashes.len() as u64).to_le_bytes());
    for (index, hash) in plaintext_chunk_hashes.iter().enumerate() {
        hasher.update((index as u64).to_le_bytes());
        hasher.update(hash);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn keyed_content_fingerprint(
    master_key: &[u8; 32],
    total_size: u64,
    plaintext_chunk_hashes: &[[u8; 32]],
) -> Result<String, String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(master_key)
        .map_err(|_| "Could not initialize Drive content fingerprint".to_string())?;
    mac.update(b"TeraRelayDriveFingerprintV1");
    mac.update(&total_size.to_le_bytes());
    mac.update(&(plaintext_chunk_hashes.len() as u64).to_le_bytes());
    for (index, hash) in plaintext_chunk_hashes.iter().enumerate() {
        mac.update(&(index as u64).to_le_bytes());
        mac.update(hash);
    }
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[tauri::command]
pub fn cmd_drive_crypto_status(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveCryptoStatus, String> {
    let envelope = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        load_crypto_envelope(&conn)?
    };
    let unlocked = match envelope.as_ref() {
        Some(envelope) => load_verified_master_key(&app_handle, envelope)?.is_some(),
        None => false,
    };
    Ok(DriveCryptoStatus {
        configured: envelope.is_some(),
        unlocked,
        version: envelope.as_ref().map(|value| value.version),
    })
}

#[tauri::command]
pub async fn cmd_drive_crypto_setup(
    passphrase: String,
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveCryptoStatus, String> {
    // Never generate a new master key solely from an unsynced local cache.
    // A second computer may already have configured encrypted Drive data in
    // Saved Messages. Require a successful remote reconciliation first;
    // no Telegram connection means setup must fail closed, not fork the key.
    crate::commands::drive_metadata::cmd_drive_sync_metadata(
        app_handle.clone(),
        app_handle.state::<crate::commands::TelegramState>(),
        app_handle.state::<DbConnection>(),
    )
    .await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        if load_crypto_envelope(&conn)?.is_some() {
            return Err("TeraRelay Drive encryption is already configured".to_string());
        }
    }

    let mut master_key = [0u8; 32];
    rand::rng().fill(&mut master_key);
    let envelope = new_envelope(&passphrase, &master_key)?;
    save_master_key(&app_handle, &master_key)?;
    master_key.zeroize();

    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        save_crypto_envelope(&conn, Some(&envelope))?;
        crate::commands::drive_metadata::mark_drive_dirty(&conn)?;
    }

    crate::commands::drive_metadata::cmd_drive_sync_metadata(
        app_handle.clone(),
        app_handle.state::<crate::commands::TelegramState>(),
        app_handle.state::<DbConnection>(),
    )
    .await
    .map_err(|error| {
        format!(
            "Encryption was created locally, but it could not be synced to Telegram: {error}. Retry Drive sync before storing private files."
        )
    })?;

    cmd_drive_crypto_status(app_handle, db_pool)
}

#[tauri::command]
pub fn cmd_drive_crypto_unlock(
    passphrase: String,
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveCryptoStatus, String> {
    let envelope = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        load_crypto_envelope(&conn)?
            .ok_or_else(|| "TeraRelay Drive encryption is not configured".to_string())?
    };
    let master_key = unwrap_master_key(&passphrase, &envelope)?;
    save_master_key(&app_handle, &*master_key)?;
    cmd_drive_crypto_status(app_handle, db_pool)
}

#[tauri::command]
pub async fn cmd_drive_crypto_change_passphrase(
    current_passphrase: String,
    new_passphrase: String,
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveCryptoStatus, String> {
    crate::commands::drive_metadata::cmd_drive_sync_metadata(
        app_handle.clone(),
        app_handle.state::<crate::commands::TelegramState>(),
        app_handle.state::<DbConnection>(),
    )
    .await?;
    let envelope = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        load_crypto_envelope(&conn)?
            .ok_or_else(|| "TeraRelay Drive encryption is not configured".to_string())?
    };
    let master_key = unwrap_master_key(&current_passphrase, &envelope)?;
    let new_envelope = new_envelope(&new_passphrase, &*master_key)?;
    save_master_key(&app_handle, &*master_key)?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        save_crypto_envelope(&conn, Some(&new_envelope))?;
        crate::commands::drive_metadata::mark_drive_dirty(&conn)?;
    }
    crate::commands::drive_metadata::cmd_drive_sync_metadata(
        app_handle.clone(),
        app_handle.state::<crate::commands::TelegramState>(),
        app_handle.state::<DbConnection>(),
    )
    .await
    .map_err(|error| {
        format!(
            "Passphrase changed locally, but Telegram sync failed: {error}. Retry Drive sync before using the new passphrase on another device."
        )
    })?;
    cmd_drive_crypto_status(app_handle, db_pool)
}

#[tauri::command]
pub fn cmd_drive_crypto_lock(
    app_handle: AppHandle,
    db_pool: State<'_, DbConnection>,
) -> Result<DriveCryptoStatus, String> {
    delete_master_key(&app_handle)?;
    cmd_drive_crypto_status(app_handle, db_pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn passphrase_wrap_round_trip_and_wrong_password_rejected() {
        let master: [u8; 32] = [7; 32];
        let envelope = new_envelope("correct horse battery staple", &master).unwrap();
        let unwrapped = unwrap_master_key("correct horse battery staple", &envelope).unwrap();
        assert_eq!(unwrapped.as_ref(), &master);
        assert!(unwrap_master_key("this password is definitely wrong", &envelope).is_err());
    }

    #[test]
    fn envelope_verifier_rejects_a_stale_master_key() {
        let master: [u8; 32] = [4; 32];
        let stale: [u8; 32] = [5; 32];
        let envelope = new_envelope("correct horse battery staple", &master).unwrap();
        assert!(master_key_matches_envelope(&master, &envelope).unwrap());
        assert!(!master_key_matches_envelope(&stale, &envelope).unwrap());

        let mut legacy_json = serde_json::to_value(&envelope).unwrap();
        legacy_json.as_object_mut().unwrap().remove("key_check_b64");
        let legacy: DriveCryptoEnvelope = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy.key_check_b64.is_none());
        assert!(master_key_matches_envelope(&master, &legacy).unwrap());
    }

    #[test]
    fn encoded_chunk_length_matches_plain_and_encrypted_layouts() {
        assert_eq!(encoded_chunk_len(0, 123).unwrap(), 123);
        assert_eq!(
            encoded_chunk_len(DRIVE_ENCRYPTION_VERSION, 123).unwrap(),
            123 + CHUNK_MAGIC.len() as u64 + 24 + 16
        );
        assert!(encoded_chunk_len(99, 123).is_err());
        assert!(encoded_chunk_len(DRIVE_ENCRYPTION_VERSION, u64::MAX).is_err());
    }

    #[test]
    fn encrypted_chunks_are_authenticated_and_randomized() {
        let master: [u8; 32] = [9; 32];
        let plaintext = b"private drive payload";
        let first = encrypt_chunk(&master, "abc", 2, plaintext).unwrap();
        let second = encrypt_chunk(&master, "abc", 2, plaintext).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            decrypt_chunk(&master, "abc", 2, plaintext.len() as u64, &first).unwrap(),
            plaintext
        );

        let mut tampered = first.clone();
        *tampered.last_mut().unwrap() ^= 0x01;
        assert!(decrypt_chunk(&master, "abc", 2, plaintext.len() as u64, &tampered).is_err());
    }

    #[test]
    fn keyed_fingerprint_depends_on_plaintext_chunks_without_exposing_raw_hash() {
        let master: [u8; 32] = [3; 32];
        let a = Sha256::digest(b"one");
        let b = Sha256::digest(b"two");
        let a: [u8; 32] = a.into();
        let b: [u8; 32] = b.into();
        let fingerprint = keyed_content_fingerprint(&master, 6, &[a, b]).unwrap();
        assert_eq!(fingerprint.len(), 64);
        let raw_hash = a
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_ne!(fingerprint, raw_hash);
    }
}
