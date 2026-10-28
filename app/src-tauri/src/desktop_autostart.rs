//! Linux graphical-session autostart, per-user and explicitly opt-in.
//! No systemd service, privileged write, session export or existing-login mutation.
#[cfg(target_os = "linux")]
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
const AUTOSTART_ENTRY: &str = "[Desktop Entry]\nType=Application\nName=TeraRelay Drive\nComment=Keep TeraRelay Drive connected after Linux login\nExec=terarelay --background\nTryExec=terarelay\nTerminal=false\nNoDisplay=true\nX-TeraRelay-Managed=true\n";
#[cfg(target_os = "linux")]
const ENTRY_FILENAME: &str = "terarelay-drive.desktop";

#[cfg(target_os = "linux")]
fn entry_path() -> Result<PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("Cannot find the current user's configuration directory")?;
    Ok(config.join("autostart").join(ENTRY_FILENAME))
}

#[cfg(target_os = "linux")]
fn entry_status(path: &Path) -> Result<bool, String> {
    match fs::read_to_string(path) {
        Ok(contents) if contents == AUTOSTART_ENTRY => Ok(true),
        Ok(_) => Err("An existing TeraRelay autostart entry differs from the one managed by this app. It was left untouched.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot read TeraRelay startup preference: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn update_entry(path: &Path, enabled: bool) -> Result<bool, String> {
    if enabled {
        if entry_status(path)? {
            return Ok(true);
        }
        let parent = path.parent().ok_or("Invalid startup configuration path")?;
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create startup directory: {e}"))?;
        // create_new does not follow an existing symlink or overwrite another
        // startup entry. Never truncate existing user files.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("Cannot create TeraRelay startup entry: {e}"))?;
        if let Err(error) = file
            .write_all(AUTOSTART_ENTRY.as_bytes())
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(format!("Cannot save TeraRelay startup entry: {error}"));
        }
        Ok(true)
    } else if entry_status(path)? {
        fs::remove_file(path).map_err(|e| format!("Cannot disable TeraRelay autostart: {e}"))?;
        Ok(false)
    } else {
        Ok(false)
    }
}

/// Initialize the native Telegram runner when started from the login manager.
/// A hidden WebView may have its timers throttled, so Drive must not depend on
/// React executing to reconnect its existing session. This never logs in with
/// new credentials or creates/replaces the saved session.
#[cfg(target_os = "linux")]
pub fn start_background_telegram_session(app: &tauri::AppHandle) {
    use tauri::Manager;
    use tauri_plugin_store::StoreExt;

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let store = match handle.store("config.json") {
            Ok(store) => store,
            Err(error) => {
                log::warn!("Background Drive: saved settings unavailable: {error}");
                return;
            }
        };
        let api_id = store
            .get("api_id")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|id| id.parse::<i32>().ok())
                    .or_else(|| value.as_i64().and_then(|id| i32::try_from(id).ok()))
            })
            .filter(|id| *id > 0);
        let Some(api_id) = api_id else {
            // No previous Telegram login: don't create a session or display
            // the account setup wizard on behalf of the user.
            return;
        };
        let existing_session = handle
            .path()
            .app_data_dir()
            .ok()
            .map(|dir| dir.join("telegram.session"))
            .is_some_and(|path| path.is_file());
        if !existing_session {
            // SqliteSession::open can create a new empty session. Background
            // startup must never do that when no saved login exists.
            log::info!("Background Drive: no existing Telegram session to restore.");
            return;
        }

        for attempt in 0..6 {
            match crate::commands::cmd_restore_saved_telegram_session(
                handle.clone(),
                handle.state::<crate::commands::TelegramState>(),
                api_id,
            )
            .await
            {
                Ok(true) => {
                    log::info!("Background Drive: verified the existing Telegram session.");
                    return;
                }
                Ok(false) => {
                    // An explicit logout removed the session. Do not create
                    // another session or retry a previously logged-out account.
                    log::info!("Background Drive: no saved session to reconnect.");
                    return;
                }
                Err(error) => {
                    log::warn!(
                        "Background Drive: Telegram reconnect attempt {} unavailable: {error}",
                        attempt + 1
                    );
                    if attempt < 5 {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
        }
    });
}

#[tauri::command]
pub fn cmd_desktop_autostart_status() -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        entry_status(&entry_path()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("Login startup preference is currently supported on Linux only".into())
    }
}

#[tauri::command]
pub fn cmd_desktop_autostart_set(enabled: bool) -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        update_entry(&entry_path()?, enabled)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = enabled;
        Err("Login startup preference is currently supported on Linux only".into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn opt_in_and_disable_only_touch_owned_entry() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("terarelay-autostart-{unique}"));
        let entry = root.join("autostart").join(ENTRY_FILENAME);

        assert!(!entry_status(&entry).unwrap());
        assert!(update_entry(&entry, true).unwrap());
        assert_eq!(fs::read_to_string(&entry).unwrap(), AUTOSTART_ENTRY);
        assert!(update_entry(&entry, true).unwrap());
        assert!(!update_entry(&entry, false).unwrap());
        assert!(!entry.exists());

        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "[Desktop Entry]\nHidden=true\n").unwrap();
        assert!(update_entry(&entry, true).is_err());
        assert!(update_entry(&entry, false).is_err());
        assert_eq!(
            fs::read_to_string(&entry).unwrap(),
            "[Desktop Entry]\nHidden=true\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
