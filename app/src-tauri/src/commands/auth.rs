use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use grammers_client::Client;
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use grammers_session::Session;
use grammers_tl_types as tl;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::Manager;
use tauri::State;
use tokio::sync::oneshot;
use tokio::time::Duration;

use crate::commands::utils::map_error;
use crate::models::AuthResult;
use crate::TelegramState;
use grammers_client::SignInError;

/// Convert a SOCKS proxy endpoint to a numeric address before Grammers sees it.
/// The pinned Grammers proxy implementation otherwise invokes the vulnerable
/// Hickory 0.25 DNS encoder for a hostname proxy. Only the *proxy endpoint* is
/// resolved locally; SOCKS requests for Telegram destinations remain proxied.
/// This reduces exposure, but does not patch or remove RustSec advisories.
fn proxy_url_with_ip_host(proxy_url: &str, address: std::net::IpAddr) -> Result<String, String> {
    let mut url = reqwest::Url::parse(proxy_url)
        .map_err(|_| "Configured Telegram proxy URL is invalid".to_string())?;
    if url.scheme() != "socks5" || url.host_str().is_none() || url.port().is_none() {
        return Err("Configured Telegram proxy must be SOCKS5 with a port".into());
    }
    let host = match address {
        std::net::IpAddr::V4(ipv4) => ipv4.to_string(),
        std::net::IpAddr::V6(ipv6) => format!("[{ipv6}]"),
    };
    url.set_host(Some(&host))
        .map_err(|_| "Could not encode numeric Telegram proxy endpoint".to_string())?;
    Ok(url.to_string())
}

pub(crate) async fn resolve_telegram_proxy_url(proxy_url: String) -> Result<String, String> {
    let url = reqwest::Url::parse(&proxy_url)
        .map_err(|_| "Configured Telegram proxy URL is invalid".to_string())?;
    if url.scheme() != "socks5" {
        return Err("Configured Telegram proxy must use SOCKS5".into());
    }
    let host = url.host_str().ok_or("Telegram proxy hostname is missing")?;
    let port = url.port().ok_or("Telegram proxy port is missing")?;
    // A numeric proxy address never enters DNS resolution in Grammers.
    if host
        .trim_matches(&['[', ']'][..])
        .parse::<std::net::IpAddr>()
        .is_ok()
    {
        return Ok(proxy_url);
    }

    // Use Tokio's system getaddrinfo resolver rather than Grammers' pinned
    // Hickory resolver; keep it bounded, with no unsafe DNS fallback.
    let addresses = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| "Timed out resolving Telegram proxy endpoint".to_string())?
    .map_err(|_| "Could not resolve Telegram proxy endpoint".to_string())?;
    let addresses: Vec<_> = addresses.collect();
    let address = addresses
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addresses.first())
        .ok_or("Telegram proxy endpoint has no usable IP address")?;
    proxy_url_with_ip_host(&proxy_url, address.ip())
}

/// Ensures the Telegram client is initialized.
///
/// IMPORTANT: This function properly manages runner lifecycle to prevent stack overflow.
/// Before spawning a new runner, it signals the old runner to shutdown.
pub async fn ensure_client_initialized(
    app_handle: &tauri::AppHandle,
    state: &State<'_, TelegramState>,
    api_id: i32,
) -> Result<Client, String> {
    #[cfg(target_os = "android")]
    {
        let mut count = 0;
        while ndk_context::android_context().vm().is_null()
            || ndk_context::android_context().context().is_null()
        {
            if count >= 200 {
                // 10 seconds timeout
                return Err("Timeout waiting for Android JNI context initialization.".to_string());
            }
            log::info!(
                "Waiting for Android JNI context to initialize ({}ms)...",
                count * 50
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            count += 1;
        }
        log::info!("Android JNI context is ready!");
    }

    let mut client_guard = state.client.lock().await;

    if let Some(client) = client_guard.as_ref() {
        return Ok(client.clone());
    }

    // CRITICAL: Shutdown existing runner before creating a new one
    // This prevents runner task accumulation which causes stack overflow
    let did_shutdown_old_runner = {
        let mut guard = state.runner_shutdown.lock().unwrap();
        if let Some(shutdown_tx) = guard.take() {
            log::info!("Signaling old runner to shutdown...");
            let _ = shutdown_tx.send(());
            true
        } else {
            false
        }
    }; // MutexGuard dropped here — before the await
    if did_shutdown_old_runner {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let runner_num = state.runner_count.fetch_add(1, Ordering::SeqCst) + 1;
    log::info!("Initializing Telegram client #{}.", runner_num);

    // Resolve session path safely
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    if !app_data_dir.exists() {
        std::fs::create_dir_all(&app_data_dir)
            .map_err(|e| format!("Failed to create app data dir: {}", e))?;
    }

    let session_path = app_data_dir.join("telegram.session");
    let session_path_str = session_path.to_string_lossy().to_string();
    log::info!("Opening saved Telegram session.");

    let mut session_open_result = SqliteSession::open(&session_path_str);

    // Retry opening the session database up to 5 times (every 100ms)
    // in case the database is temporarily locked by the old shutting down runner.
    if session_open_result.is_err() {
        for attempt in 1..=5 {
            log::warn!("Failed to open session on attempt {} (database may be locked). Retrying in 100ms...", attempt);
            tokio::time::sleep(Duration::from_millis(100)).await;
            session_open_result = SqliteSession::open(&session_path_str);
            if session_open_result.is_ok() {
                break;
            }
        }
    }

    let session = session_open_result.map_err(|e| {
        let detail = e.to_string();
        log::error!(
            "Session file could not be opened after retries. Refusing to recreate or delete it: {}",
            detail
        );
        format!(
            "Telegram session is temporarily unavailable ({}). Close any other TeraRelay instance using this profile and try again. The saved login was not modified.",
            detail
        )
    })?;

    let net_config = app_handle.state::<Arc<crate::vpn_optimizer::NetworkConfig>>();
    let preferred_dc = {
        let vpn = net_config.vpn.read().unwrap();
        if vpn.enabled {
            vpn.preferred_dc.clone()
        } else {
            "auto".to_string()
        }
    };
    if preferred_dc.starts_with("dc") && preferred_dc.len() > 2 {
        if let Ok(dc_id) = preferred_dc[2..].parse::<i32>() {
            log::info!("Setting preferred home DC ID: {}", dc_id);
            session.set_home_dc_id(dc_id);
        }
    }

    let mut connection_params = grammers_mtsender::ConnectionParams::default();
    if let Some(proxy_url) = net_config.effective_proxy_url_checked()? {
        // Never log URLs with embedded SOCKS5 credentials. Resolve only the
        // proxy host with the OS before giving a URL to pinned Grammers.
        let resolved = resolve_telegram_proxy_url(proxy_url).await?;
        log::info!("Using configured Telegram SOCKS5 proxy");
        connection_params.proxy_url = Some(resolved);
    }

    let session = Arc::new(session);
    *state.session.lock().await = Some(session.clone());
    let pool = SenderPool::with_configuration(session, api_id, connection_params);
    let client = Client::new(&pool);

    // Create shutdown channel for this runner
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    *state.runner_shutdown.lock().unwrap() = Some(shutdown_tx);

    // Spawn the network runner with shutdown support
    let SenderPool { runner, .. } = pool;
    tauri::async_runtime::spawn(async move {
        tokio::select! {
            // Normal runner operation
            _ = runner.run() => {
                log::info!("Runner #{} exited normally", runner_num);
            }
            // Shutdown requested
            _ = shutdown_rx => {
                log::info!("Runner #{} shutdown requested, exiting", runner_num);
            }
        }
    });

    *client_guard = Some(client.clone());
    Ok(client)
}

/// Startup-only session discovery. Never opens, creates, or changes the
/// Telegram SQLite database. A missing or empty file means there is no saved
/// account to restore; temporary network failures must NOT be treated as logout.
fn saved_session_exists(app_handle: &tauri::AppHandle) -> Result<bool, String> {
    let path = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Could not locate Telegram session: {e}"))?
        .join("telegram.session");
    saved_session_file_present(&path)
}

fn saved_session_file_present(path: &std::path::Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Could not inspect saved Telegram session: {error}")),
    }
}

#[tauri::command]
pub fn cmd_saved_telegram_session_exists(app_handle: tauri::AppHandle) -> Result<bool, String> {
    saved_session_exists(&app_handle)
}

/// Both the visible UI and Linux background startup call this same operation.
/// The client initialization lock deduplicates their work; unlike the older
/// cmd_check_connection recovery path this never drops a live client on a
/// transient failed get_me() ping.
#[tauri::command]
pub async fn cmd_restore_saved_telegram_session(
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    api_id: i32,
) -> Result<bool, String> {
    if api_id <= 0 {
        return Err("Invalid saved Telegram API ID".into());
    }
    if !saved_session_exists(&app_handle)? {
        return Ok(false);
    }
    *state.api_id.lock().await = Some(api_id);
    let client = ensure_client_initialized(&app_handle, &state, api_id).await?;
    client
        .get_me()
        .await
        .map(|_| true)
        .map_err(|error| format!("Saved Telegram connection is not ready: {error}"))
}

#[tauri::command]
pub async fn cmd_connect(
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    api_id: i32,
) -> Result<bool, String> {
    // Store API ID for auto-reconnect
    *state.api_id.lock().await = Some(api_id);
    ensure_client_initialized(&app_handle, &state, api_id).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_check_connection(
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
) -> Result<bool, String> {
    // 1. Check if client exists and is responsive
    let client_msg_opt = {
        let guard = state.client.lock().await;
        guard.as_ref().cloned()
    };

    if let Some(client) = client_msg_opt {
        // Ping (e.g., get_me)
        if client.get_me().await.is_ok() {
            return Ok(true);
        }
        // A failed ping does NOT mean logout. Do not discard the client or
        // replace its runner while a separate startup/Drive task is using it.
        return Err(
            "Telegram is temporarily unreachable; the existing saved session was preserved.".into(),
        );
    } else {
        log::warn!("Connection check: No client found. Checking for saved API ID...");
    }

    // 2. Reconnect Logic
    let api_id_opt = *state.api_id.lock().await;
    if let Some(api_id) = api_id_opt {
        // Only initialize when there was no client. Never destroy a live
        // runner in response to transient connectivity or a slow get_me().
        match ensure_client_initialized(&app_handle, &state, api_id).await {
            Ok(c) => {
                // Double check
                if c.get_me().await.is_ok() {
                    log::info!("Auto-reconnect successful.");
                    return Ok(true);
                } else {
                    return Err("Reconnect succeeded but ping failed.".to_string());
                }
            }
            Err(e) => return Err(format!("Auto-reconnect failed: {}", e)),
        }
    }

    Ok(false) // Not connected and no credentials to reconnect
}

#[tauri::command]
pub async fn cmd_reconnect_with_network_settings(
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
) -> Result<bool, String> {
    let api_id = *state.api_id.lock().await;
    let api_id = match api_id {
        Some(id) => id,
        None => return Err("Not authenticated — no API ID saved.".into()),
    };

    log::info!("Reconnecting with updated network settings...");

    // 1. Shutdown existing runner
    {
        let mut shutdown_guard = state.runner_shutdown.lock().unwrap();
        if let Some(shutdown_tx) = shutdown_guard.take() {
            log::info!("Signaling runner shutdown for reconnect...");
            let _ = shutdown_tx.send(());
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Clear old client
    *state.client.lock().await = None;

    // 3. Reinitialize with current network config (reads from NetworkConfig state)
    let client = ensure_client_initialized(&app_handle, &state, api_id).await?;

    // 4. Verify the new connection works
    match client.get_me().await {
        Ok(_me) => {
            log::info!("Reconnect successful — verified via get_me().");
            Ok(true)
        }
        Err(e) => {
            log::error!("Reconnect init succeeded but get_me failed: {}", e);
            Err(format!("Reconnected but ping failed: {}", e))
        }
    }
}

#[tauri::command]
pub async fn cmd_logout(
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
) -> Result<bool, String> {
    log::info!("Logging out...");

    // 1. Shutdown the network runner FIRST to prevent any operations
    {
        let mut shutdown_guard = state.runner_shutdown.lock().unwrap();
        if let Some(shutdown_tx) = shutdown_guard.take() {
            log::info!("Signaling runner shutdown for logout...");
            let _ = shutdown_tx.send(());
        }
    }

    // 2. Try to sign out from Telegram (if connected)
    let client_opt = { state.client.lock().await.clone() };
    if let Some(client) = client_opt {
        // We don't strictly care if this fails (e.g. network down), we just want to clear local state.
        let _ = client.sign_out().await;
    }

    // 3. Clear State
    *state.client.lock().await = None;
    *state.session.lock().await = None;
    *state.login_token.lock().await = None;
    *state.password_token.lock().await = None;
    *state.api_id.lock().await = None;
    *state.ephemeral_api_hash.lock().await = None;
    crate::commands::utils::clear_peer_cache(&state.peer_cache).await;
    state.cancelled_transfers.write().await.clear();

    // 4. Remove Session File
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;
    let session_path = app_data_dir.join("telegram.session");
    let _ = std::fs::remove_file(session_path);
    let _ = std::fs::remove_file(app_data_dir.join("telegram.session-wal"));
    let _ = std::fs::remove_file(app_data_dir.join("telegram.session-shm"));

    log::info!(
        "Logout complete. Runner count: {}",
        state.runner_count.load(Ordering::SeqCst)
    );
    Ok(true)
}

async fn request_login_code_inner(
    app_handle: &tauri::AppHandle,
    phone: &str,
    api_id: i32,
    api_hash: &str,
    state: &State<'_, TelegramState>,
    persist_credentials: bool,
) -> Result<String, String> {
    if api_hash.trim().is_empty() {
        return Err("API Hash cannot be empty.".to_string());
    }

    *state.api_id.lock().await = Some(api_id);
    let client_handle = ensure_client_initialized(app_handle, state, api_id).await?;
    log::info!("Requesting Telegram login code.");

    let mut last_error = String::new();
    for attempt in 1..=2 {
        match client_handle.request_login_code(phone, api_hash).await {
            Ok(token) => {
                if persist_credentials {
                    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                    crate::commands::secure_credentials::save_api_credentials(
                        app_handle, api_id, api_hash,
                    )?;
                }

                let mut token_guard = state.login_token.lock().await;
                *token_guard = Some(token);
                return Ok("code_sent".to_string());
            }
            Err(error) => {
                let message = error.to_string();
                log::warn!(
                    "Telegram login code request failed on attempt {}: {}",
                    attempt,
                    message
                );
                if message.contains("AUTH_RESTART") || message.contains("500") {
                    last_error = message;
                    continue;
                }
                return Err(map_error(error));
            }
        }
    }

    Err(format!("Telegram Error after retry: {}", last_error))
}

#[tauri::command]
pub async fn cmd_auth_request_code(
    app_handle: tauri::AppHandle,
    phone: String,
    api_id: i32,
    api_hash: String,
    state: State<'_, TelegramState>,
) -> Result<String, String> {
    request_login_code_inner(&app_handle, &phone, api_id, &api_hash, &state, true).await
}

pub(crate) fn real_e2e_qa_ephemeral_login_enabled() -> bool {
    cfg!(debug_assertions)
        && std::env::var("TERARELAY_REAL_E2E_QA")
            .map(|value| value == "1")
            .unwrap_or(false)
}

#[tauri::command]
pub async fn cmd_auth_request_code_ephemeral(
    app_handle: tauri::AppHandle,
    phone: String,
    api_id: i32,
    api_hash: String,
    state: State<'_, TelegramState>,
) -> Result<String, String> {
    if !real_e2e_qa_ephemeral_login_enabled() {
        return Err(
            "Ephemeral Telegram login is available only in explicitly enabled debug QA mode."
                .to_string(),
        );
    }
    let result =
        request_login_code_inner(&app_handle, &phone, api_id, &api_hash, &state, false).await?;
    *state.ephemeral_api_hash.lock().await = Some(api_hash.trim().to_string());
    Ok(result)
}

#[tauri::command]
pub async fn cmd_auth_request_code_saved(
    app_handle: tauri::AppHandle,
    phone: String,
    state: State<'_, TelegramState>,
) -> Result<String, String> {
    let credentials = crate::commands::secure_credentials::load_api_credentials(&app_handle)?;
    request_login_code_inner(
        &app_handle,
        &phone,
        credentials.api_id,
        &credentials.api_hash,
        &state,
        false,
    )
    .await
}

#[tauri::command]
pub async fn cmd_auth_sign_in(
    code: String,
    state: State<'_, TelegramState>,
) -> Result<AuthResult, String> {
    log::info!("Signing in with code...");

    let client = {
        let guard = state.client.lock().await;
        guard.as_ref().ok_or("Client not initialized")?.clone()
    };

    let token_guard = state.login_token.lock().await;
    let login_token = token_guard
        .as_ref()
        .ok_or("No login session found (restart flow)")?;

    match client.sign_in(login_token, &code).await {
        Ok(_user) => {
            log::info!("Successfully logged in.");
            Ok(AuthResult {
                success: true,
                next_step: Some("dashboard".to_string()),
                error: None,
            })
        }
        Err(SignInError::PasswordRequired(token)) => {
            let mut pw_guard = state.password_token.lock().await;
            *pw_guard = Some(token);

            Ok(AuthResult {
                success: false,
                next_step: Some("password".to_string()),
                error: None,
            })
        }
        Err(e) => {
            log::error!("Sign in error: {}", e);
            Err(format!("Sign in failed: {}", e))
        }
    }
}

#[tauri::command]
pub async fn cmd_auth_check_password(
    password: String,
    state: State<'_, TelegramState>,
) -> Result<AuthResult, String> {
    let client = {
        let guard = state.client.lock().await;
        guard.as_ref().ok_or("Client not initialized")?.clone()
    };

    let mut pw_guard = state.password_token.lock().await;
    let pw_token = pw_guard.take().ok_or("No password session found")?;

    match client.check_password(pw_token, password.as_str()).await {
        Ok(_user) => {
            log::info!("2FA Success.");
            Ok(AuthResult {
                success: true,
                next_step: Some("dashboard".to_string()),
                error: None,
            })
        }
        Err(e) => Err(format!("2FA Failed: {}", e)),
    }
}

/// QR Login -- Step 1: Export a login token and return the `tg://login?token=...` URL.
/// The frontend renders this as a QR code for the user to scan with their phone.
#[tauri::command]
pub async fn cmd_auth_qr_login(
    app_handle: tauri::AppHandle,
    api_id: i32,
    api_hash: String,
    state: State<'_, TelegramState>,
) -> Result<String, String> {
    if api_hash.trim().is_empty() {
        return Err("API Hash cannot be empty.".to_string());
    }

    // Store API ID
    *state.api_id.lock().await = Some(api_id);

    let client = ensure_client_initialized(&app_handle, &state, api_id).await?;

    log::info!("Requesting QR login token...");

    let result = client
        .invoke(&tl::functions::auth::ExportLoginToken {
            api_id,
            api_hash: api_hash.clone(),
            except_ids: vec![],
        })
        .await
        .map_err(|e| format!("ExportLoginToken failed: {}", e))?;

    match result {
        tl::enums::auth::LoginToken::Token(t) => {
            let encoded = URL_SAFE_NO_PAD.encode(&t.token);
            let url = format!("tg://login?token={}", encoded);
            log::info!("QR login URL generated, expires at {}", t.expires);
            Ok(url)
        }
        tl::enums::auth::LoginToken::Success(_s) => {
            // Already authorized (e.g. from a previous session)
            log::info!("QR login: already authorized");
            Ok("__authorized__".to_string())
        }
        tl::enums::auth::LoginToken::MigrateTo(m) => {
            log::info!("QR login: need to migrate to DC {}", m.dc_id);
            let encoded = URL_SAFE_NO_PAD.encode(&m.token);
            let url = format!("tg://login?token={}", encoded);
            Ok(url)
        }
    }
}

/// QR Login -- Step 2: Poll for scan completion.
/// Checks if the session became authorized after the user scanned the QR code.
///
/// IMPORTANT: We must NOT call auth.exportLoginToken here for polling.
/// Each call to exportLoginToken generates a NEW token and invalidates the
/// previous one, causing the scanned QR code to fail with "Invalid code".
/// Instead, we check is_authorized() which succeeds once the phone app
/// accepts the token via auth.acceptLoginToken.
#[tauri::command]
pub async fn cmd_auth_qr_poll(state: State<'_, TelegramState>) -> Result<AuthResult, String> {
    let client = {
        let guard = state.client.lock().await;
        guard.as_ref().ok_or("Client not initialized")?.clone()
    };

    // Check if the session is now authorized (user scanned QR on phone)
    match client.is_authorized().await {
        Ok(true) => {
            log::info!("QR login: session authorized!");
            Ok(AuthResult {
                success: true,
                next_step: Some("dashboard".to_string()),
                error: None,
            })
        }
        Ok(false) => {
            // Not yet scanned or accepted
            Ok(AuthResult {
                success: false,
                next_step: Some("waiting".to_string()),
                error: None,
            })
        }
        Err(e) => {
            log::warn!("QR poll auth check failed: {}", e);
            Ok(AuthResult {
                success: false,
                next_step: Some("waiting".to_string()),
                error: None,
            })
        }
    }
}

#[cfg(test)]
mod saved_telegram_session_tests {
    use super::saved_session_file_present;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn absent_or_empty_saved_session_cannot_trigger_new_session_creation() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("terarelay-saved-login-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        let session = root.join("telegram.session");

        assert!(!saved_session_file_present(&session).unwrap());
        std::fs::write(&session, []).unwrap();
        assert!(!saved_session_file_present(&session).unwrap());
        std::fs::write(&session, b"synthetic preserved session fixture").unwrap();
        assert!(saved_session_file_present(&session).unwrap());
        assert_eq!(
            std::fs::read(&session).unwrap(),
            b"synthetic preserved session fixture"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod telegram_proxy_security_tests {
    use super::{proxy_url_with_ip_host, resolve_telegram_proxy_url};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn numeric_proxy_rewrite_preserves_auth_port_and_socks_scheme() {
        let original = format!(
            "socks5://{}:{}@my-proxy.example:1080",
            ["ali", "ce"].concat(),
            ["p%40", "ss%3Aword"].concat()
        );
        let numeric =
            proxy_url_with_ip_host(&original, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 22))).unwrap();
        assert_eq!(
            numeric,
            format!(
                "socks5://{}:{}@192.0.2.22:1080",
                "alice",
                ["p%40", "ss%3Aword"].concat()
            )
        );
        assert!(!numeric.contains("my-proxy.example"));

        let ipv6 = proxy_url_with_ip_host(&original, IpAddr::V6(Ipv6Addr::LOCALHOST)).unwrap();
        assert_eq!(
            ipv6,
            format!(
                "socks5://{}:{}@[::1]:1080",
                "alice",
                ["p%40", "ss%3Aword"].concat()
            )
        );
        assert!(!ipv6.contains("my-proxy.example"));
    }

    #[tokio::test]
    async fn hostname_proxy_is_resolved_by_os_before_reaching_grammers() {
        let numeric = resolve_telegram_proxy_url(format!(
            "socks5://{}:{}@localhost:1080",
            "user", "synthetic"
        ))
        .await
        .unwrap();
        let url = reqwest::Url::parse(&numeric).unwrap();
        assert_eq!(url.scheme(), "socks5");
        assert_eq!(url.port(), Some(1080));
        assert_eq!(url.username(), "user");
        assert_eq!(url.password(), Some("synthetic"));
        let ip = url
            .host_str()
            .unwrap()
            .trim_matches(&['[', ']'][..])
            .parse::<IpAddr>()
            .expect("Grammers must receive a numeric proxy host");
        assert!(ip.is_loopback());
        assert!(!numeric.contains("localhost"));
    }

    #[tokio::test]
    async fn malformed_proxy_configuration_fails_closed_without_echoing_credentials() {
        let password = ["only", "a", "test", "value"].join("-");
        for input in [
            format!("http://alice:{password}@proxy.example:1080"),
            format!("socks5://alice:{password}@proxy.example"),
            format!("not-a-url-{password}"),
        ] {
            let error = resolve_telegram_proxy_url(input).await.unwrap_err();
            assert!(
                !error.contains(&password),
                "Proxy errors must not leak passwords"
            );
        }
        let numeric = format!("socks5://{}:{}@127.0.0.1:1080", "alice", "synthetic");
        assert_eq!(
            resolve_telegram_proxy_url(numeric.clone()).await.unwrap(),
            numeric,
        );
    }
}
