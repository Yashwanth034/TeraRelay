use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

const SERVICE_NAME: &str = "io.terarelay.desktop.telegram";
#[cfg(target_os = "linux")]
const QA_KERNEL_SERVICE_NAME: &str = "io.terarelay.desktop.telegram.qa-kernel";
const LEGACY_CONFIG_NAME: &str = "config.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoredTelegramApiCredentials {
    pub api_id: i32,
    pub api_hash: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SecureCredentialStatus {
    pub available: bool,
    pub has_credentials: bool,
    pub migrated: bool,
    pub api_id: Option<i32>,
}

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve TeraRelay app data: {e}"))
}

fn legacy_config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join(LEGACY_CONFIG_NAME))
}

fn parse_legacy_credentials(
    app: &tauri::AppHandle,
) -> Result<Option<StoredTelegramApiCredentials>, String> {
    let path = legacy_config_path(app)?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Failed to read TeraRelay settings: {error}")),
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid TeraRelay settings file: {e}"))?;
    let api_id = value
        .get("api_id")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<i32>().ok())
        .or_else(|| {
            value
                .get("api_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
        });
    let api_hash = value
        .get("api_hash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    match (api_id, api_hash) {
        (Some(api_id), Some(api_hash)) => Ok(Some(StoredTelegramApiCredentials {
            api_id,
            api_hash: api_hash.to_string(),
        })),
        _ => Ok(None),
    }
}

fn remove_legacy_api_hash(app: &tauri::AppHandle) -> Result<(), String> {
    let path = legacy_config_path(app)?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Failed to read TeraRelay settings: {error}")),
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid TeraRelay settings file: {e}"))?;
    let Some(object) = value.as_object_mut() else {
        return Err("Invalid TeraRelay settings format".to_string());
    };
    if object.remove("api_hash").is_none() {
        return Ok(());
    }

    let serialized = serde_json::to_vec_pretty(&value)
        .map_err(|e| format!("Failed to serialize TeraRelay settings: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serialized).map_err(|e| format!("Failed to stage TeraRelay settings: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("Failed to secure TeraRelay settings: {e}"))?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn profile_key(app: &tauri::AppHandle) -> Result<String, String> {
    let app_data = app_data_dir(app)?;
    let digest = Sha256::digest(app_data.to_string_lossy().as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("telegram-api-{suffix}"))
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn keyring_entry(app: &tauri::AppHandle) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE_NAME, &profile_key(app)?)
        .map_err(|e| format!("OS credential storage is unavailable: {e}"))
}

#[cfg(target_os = "linux")]
fn qa_kernel_keyring_entry(app: &tauri::AppHandle) -> Result<keyring::Entry, String> {
    let user = format!("{}-qa-kernel", profile_key(app)?);
    let credential =
        keyring::keyutils::KeyutilsCredential::new_with_target(None, QA_KERNEL_SERVICE_NAME, &user)
            .map_err(|e| format!("Linux kernel credential storage is unavailable: {e}"))?;
    Ok(keyring::Entry::new_with_credential(Box::new(credential)))
}

#[cfg(target_os = "linux")]
pub fn save_qa_kernel_api_credentials(
    app: &tauri::AppHandle,
    api_id: i32,
    api_hash: &str,
) -> Result<(), String> {
    if api_id <= 0 || api_hash.trim().is_empty() {
        return Err("Telegram API credentials are incomplete.".to_string());
    }
    let credentials = StoredTelegramApiCredentials {
        api_id,
        api_hash: api_hash.trim().to_string(),
    };
    let encoded = serde_json::to_string(&credentials)
        .map_err(|e| format!("Failed to encode Telegram API credentials: {e}"))?;
    let entry = qa_kernel_keyring_entry(app)?;
    entry
        .set_password(&encoded)
        .map_err(|e| format!("Could not save QA credentials in the Linux kernel keyring: {e}"))?;
    let verified = entry
        .get_password()
        .map_err(|e| format!("Could not verify QA credentials in the Linux kernel keyring: {e}"))?;
    let verified: StoredTelegramApiCredentials = serde_json::from_str(&verified)
        .map_err(|e| format!("Saved QA Telegram API credentials are invalid: {e}"))?;
    if verified != credentials {
        return Err("QA kernel credential verification failed after save.".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn save_qa_kernel_api_credentials(
    _app: &tauri::AppHandle,
    _api_id: i32,
    _api_hash: &str,
) -> Result<(), String> {
    Err("QA kernel credential storage is available only on Linux.".to_string())
}

#[cfg(target_os = "linux")]
pub fn load_qa_kernel_api_credentials(
    app: &tauri::AppHandle,
) -> Result<Option<StoredTelegramApiCredentials>, String> {
    let entry = qa_kernel_keyring_entry(app)?;
    match entry.get_password() {
        Ok(raw) => serde_json::from_str::<StoredTelegramApiCredentials>(&raw)
            .map(Some)
            .map_err(|e| format!("Saved QA Telegram API credentials are invalid: {e}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "Could not read QA credentials from the Linux kernel keyring: {error}"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn load_qa_kernel_api_credentials(
    _app: &tauri::AppHandle,
) -> Result<Option<StoredTelegramApiCredentials>, String> {
    Ok(None)
}

#[cfg(target_os = "linux")]
pub fn delete_qa_kernel_api_credentials(app: &tauri::AppHandle) -> Result<(), String> {
    let entry = qa_kernel_keyring_entry(app)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Could not clear QA kernel credentials: {error}")),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn delete_qa_kernel_api_credentials(_app: &tauri::AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn secure_store_available(app: &tauri::AppHandle) -> bool {
    let Ok(entry) = keyring_entry(app) else {
        return false;
    };
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => true,
        Err(error) => {
            log::warn!("OS credential storage probe unavailable: {error}");
            false
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn secure_store_available(_app: &tauri::AppHandle) -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn read_secure(app: &tauri::AppHandle) -> Result<Option<StoredTelegramApiCredentials>, String> {
    let entry = keyring_entry(app)?;
    match entry.get_password() {
        Ok(raw) => serde_json::from_str::<StoredTelegramApiCredentials>(&raw)
            .map(Some)
            .map_err(|e| format!("Saved Telegram API credentials are invalid: {e}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("Could not read OS credential storage: {error}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn read_secure(_app: &tauri::AppHandle) -> Result<Option<StoredTelegramApiCredentials>, String> {
    Ok(None)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn save_api_credentials(
    app: &tauri::AppHandle,
    api_id: i32,
    api_hash: &str,
) -> Result<(), String> {
    if api_id <= 0 || api_hash.trim().is_empty() {
        return Err("Telegram API credentials are incomplete.".to_string());
    }
    let credentials = StoredTelegramApiCredentials {
        api_id,
        api_hash: api_hash.trim().to_string(),
    };
    let encoded = serde_json::to_string(&credentials)
        .map_err(|e| format!("Failed to encode Telegram API credentials: {e}"))?;
    let entry = keyring_entry(app)?;
    entry
        .set_password(&encoded)
        .map_err(|e| format!("Could not save Telegram API credentials securely: {e}"))?;

    let verified = read_secure(app)?
        .ok_or_else(|| "Secure credential verification failed after save.".to_string())?;
    if verified != credentials {
        return Err("Secure credential verification failed after save.".to_string());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn save_api_credentials(
    _app: &tauri::AppHandle,
    _api_id: i32,
    _api_hash: &str,
) -> Result<(), String> {
    Err("OS credential storage is unavailable on this platform.".to_string())
}

pub fn load_api_credentials(
    app: &tauri::AppHandle,
) -> Result<StoredTelegramApiCredentials, String> {
    match read_secure(app) {
        Ok(Some(credentials)) => return Ok(credentials),
        Ok(None) => {}
        Err(error) => {
            log::warn!("Secure Telegram API credential lookup failed: {error}");
        }
    }

    parse_legacy_credentials(app)?.ok_or_else(|| {
        "Saved Telegram API credentials are unavailable. Sign in to TeraRelay again.".to_string()
    })
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn delete_api_credentials(app: &tauri::AppHandle) -> Result<(), String> {
    let entry = keyring_entry(app)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "Could not clear saved Telegram API credentials: {error}"
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn delete_api_credentials(_app: &tauri::AppHandle) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub fn cmd_migrate_legacy_api_credentials(
    app_handle: tauri::AppHandle,
) -> Result<SecureCredentialStatus, String> {
    let secure_existing = read_secure(&app_handle).ok().flatten();
    if let Some(credentials) = secure_existing {
        if parse_legacy_credentials(&app_handle)?.is_some() {
            remove_legacy_api_hash(&app_handle)?;
        }
        return Ok(SecureCredentialStatus {
            available: secure_store_available(&app_handle),
            has_credentials: true,
            migrated: false,
            api_id: Some(credentials.api_id),
        });
    }

    let Some(legacy) = parse_legacy_credentials(&app_handle)? else {
        return Ok(SecureCredentialStatus {
            available: secure_store_available(&app_handle),
            has_credentials: false,
            migrated: false,
            api_id: None,
        });
    };

    if !secure_store_available(&app_handle) {
        return Ok(SecureCredentialStatus {
            available: false,
            has_credentials: true,
            migrated: false,
            api_id: Some(legacy.api_id),
        });
    }

    save_api_credentials(&app_handle, legacy.api_id, &legacy.api_hash)?;
    let verified = read_secure(&app_handle)?
        .ok_or_else(|| "Secure credential migration could not be verified.".to_string())?;
    if verified != legacy {
        return Err("Secure credential migration could not be verified.".to_string());
    }
    remove_legacy_api_hash(&app_handle)?;

    Ok(SecureCredentialStatus {
        available: true,
        has_credentials: true,
        migrated: true,
        api_id: Some(legacy.api_id),
    })
}

#[tauri::command]
pub fn cmd_secure_credential_status(
    app_handle: tauri::AppHandle,
) -> Result<SecureCredentialStatus, String> {
    let secure = read_secure(&app_handle).ok().flatten();
    let legacy = parse_legacy_credentials(&app_handle)?;
    let api_id = secure
        .as_ref()
        .map(|credentials| credentials.api_id)
        .or_else(|| legacy.as_ref().map(|credentials| credentials.api_id));

    Ok(SecureCredentialStatus {
        available: secure_store_available(&app_handle),
        has_credentials: secure.is_some() || legacy.is_some(),
        migrated: false,
        api_id,
    })
}

#[tauri::command]
pub fn cmd_delete_saved_api_credentials(app_handle: tauri::AppHandle) -> Result<(), String> {
    delete_api_credentials(&app_handle)
}
