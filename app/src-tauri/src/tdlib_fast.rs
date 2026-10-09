use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures::StreamExt;
use grammers_tl_types as tl;
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as StdCommand, Stdio};
use std::sync::{atomic::AtomicU64, Arc, Mutex};
use tauri::{Manager, State};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;
use walkdir::WalkDir;

const SQLCIPHER_URL: &str =
    "https://archive.ubuntu.com/ubuntu/pool/universe/s/sqlcipher/libsqlcipher1_4.5.6-1build2_amd64.deb";
const SQLCIPHER_SHA256: &str = "30ffc3589facffbd72fc8720fb5ab7448b4dfb6e99929f0f41b4b3321f45a611";
const TDJSON_URL: &str =
    "https://archive.ubuntu.com/ubuntu/pool/universe/t/td/libtdjson1.8.38_1.8.38~git20241021.d321984+dfsg-4_amd64.deb";
const TDJSON_SHA256: &str = "250f2f51b4fae813ab166a0c12b1845e1ac5752cc66d74e43bdcd3e185e6401b";
const WORKER_SOURCE: &str = include_str!("tdlib_fast_worker.py");
// v4 intentionally starts clean from the earlier experimental TDLib attempt.
// Once authorized, these paths remain stable across TeraRelay rebuilds/updates.
const SESSION_DIR_NAME: &str = "tdlib-fast-session-v4";
const FILES_DIR_NAME: &str = "tdlib-fast-files-v4";
const KEY_FILE_NAME: &str = "tdlib-fast-database-key-v4";

#[derive(Debug, Clone)]
struct RuntimeInfo {
    installed: bool,
    library: Option<PathBuf>,
    sqlcipher: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FastTransferStatus {
    pub supported: bool,
    pub runtime_installed: bool,
    pub ready: bool,
    pub auth_state: String,
    pub version: Option<String>,
    pub is_premium: Option<bool>,
    pub backend: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy)]
pub enum FastDestination {
    SavedMessages,
    Channel(i64),
}

impl FastDestination {
    fn as_json(self) -> Value {
        match self {
            Self::SavedMessages => json!({"kind": "saved"}),
            Self::Channel(id) => json!({"kind": "channel", "id": id}),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FastDownloadChunk {
    pub message_id: i64,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FastDownloadOutcome {
    pub bytes_downloaded: u64,
    pub network_seconds: f64,
    pub average_bytes_per_sec: u64,
}

pub struct TdlibFastState {
    worker: Arc<Mutex<Option<NativeWorker>>>,
}

impl Default for TdlibFastState {
    fn default() -> Self {
        Self {
            worker: Arc::new(Mutex::new(None)),
        }
    }
}

struct NativeWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    auth_state: String,
    ready: bool,
    is_premium: Option<bool>,
}

impl Drop for NativeWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl NativeWorker {
    fn spawn(
        runtime: &RuntimeInfo,
        api_id: i32,
        api_hash: String,
        app_data: &Path,
    ) -> Result<Self, String> {
        let tdjson = runtime
            .library
            .as_ref()
            .ok_or_else(|| "TDLib runtime library was not found".to_string())?;

        secure_dir(app_data)?;
        let script = app_data.join("tdlib-fast-worker.py");
        if fs::read_to_string(&script).ok().as_deref() != Some(WORKER_SOURCE) {
            fs::write(&script, WORKER_SOURCE)
                .map_err(|e| format!("Failed to write TDLib worker: {e}"))?;
            secure_file(&script)?;
        }

        let db_dir = app_data.join(SESSION_DIR_NAME);
        let files_dir = app_data.join(FILES_DIR_NAME);
        secure_dir(&db_dir)?;
        secure_dir(&files_dir)?;
        let db_key = load_or_create_database_key(app_data, &db_dir)?;

        let python = resolve_python()?;
        let mut child = StdCommand::new(python)
            .arg("-u")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to start TDLib worker: {e}"))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "TDLib worker stdin was unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "TDLib worker stdout was unavailable".to_string())?;
        let mut stdout = BufReader::new(stdout);

        let config = json!({
            "tdjson": tdjson,
            "sqlcipher": runtime.sqlcipher,
            "db_dir": db_dir,
            "files_dir": files_dir,
            "db_key": db_key,
            "api_id": api_id,
            "api_hash": api_hash,
        });
        writeln!(stdin, "{config}")
            .map_err(|e| format!("Failed to configure TDLib worker: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("Failed to start TDLib worker: {e}"))?;

        let started = read_worker_line(&mut stdout)?;
        if started.get("event").and_then(Value::as_str) == Some("fatal") {
            return Err(started
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("TDLib worker failed to start")
                .to_string());
        }
        if started.get("event").and_then(Value::as_str) != Some("started") {
            return Err(format!(
                "Unexpected TDLib worker startup response: {started}"
            ));
        }

        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            auth_state: "starting".into(),
            ready: false,
            is_premium: None,
        })
    }

    fn request<F, N>(
        &mut self,
        mut request: Value,
        mut on_progress: F,
        mut on_network: N,
        cancel_rx: Option<&watch::Receiver<bool>>,
    ) -> Result<Value, String>
    where
        F: FnMut(u64),
        N: FnMut(u64),
    {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        request["id"] = Value::from(id);

        writeln!(self.stdin, "{request}")
            .map_err(|e| format!("TDLib worker request failed: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("TDLib worker request failed: {e}"))?;

        loop {
            if cancel_rx.map(|rx| *rx.borrow()).unwrap_or(false) {
                let _ = self.child.kill();
                return Err("Transfer cancelled".to_string());
            }

            let response = match read_worker_line(&mut self.stdout) {
                Ok(response) => response,
                Err(error) if error == "TDLib worker stopped unexpectedly" => {
                    let status = match self.child.try_wait() {
                        Ok(Some(status)) => format!(" ({status})"),
                        Ok(None) => " (process still running after stdout closed)".to_string(),
                        Err(status_error) => format!(" (exit status unavailable: {status_error})"),
                    };
                    return Err(format!("{error}{status}"));
                }
                Err(error) => return Err(error),
            };
            let event = response.get("event").and_then(Value::as_str);

            if event == Some("progress") {
                if response.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(delta) = response.get("delta").and_then(Value::as_u64) {
                        if delta > 0 {
                            on_progress(delta);
                        }
                    }
                }
                continue;
            }

            if event == Some("network") {
                if response.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(delta) = response.get("delta").and_then(Value::as_u64) {
                        if delta > 0 {
                            on_network(delta);
                        }
                    }
                }
                continue;
            }

            if event == Some("speed_limit") {
                if response.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(is_upload) = response.get("is_upload").and_then(Value::as_bool) {
                        log::warn!(
                            "TDLib {} speed limited by Telegram (updateSpeedLimitNotification)",
                            if is_upload { "upload" } else { "download" }
                        );
                    }
                }
                continue;
            }

            if event != Some("result") || response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }

            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("TDLib worker request failed")
                    .to_string());
            }

            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn update_auth_from_result(&mut self, result: &Value) {
        if let Some(state) = result.get("auth_state").and_then(Value::as_str) {
            self.auth_state = state.to_string();
        }
        self.ready = result
            .get("ready")
            .and_then(Value::as_bool)
            .unwrap_or(self.auth_state == "ready");
        if let Some(value) = result.get("is_premium") {
            self.is_premium = value.as_bool();
        }
    }

    fn status(&self, runtime_installed: bool) -> FastTransferStatus {
        log::info!(
            "TDLib account capability: telegram_premium={}",
            match self.is_premium {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            }
        );
        FastTransferStatus {
            supported: platform_supported(),
            runtime_installed,
            ready: self.ready,
            auth_state: self.auth_state.clone(),
            version: None,
            is_premium: self.is_premium,
            backend: "TDLib/C++".to_string(),
            detail: if self.ready {
                format!(
                    "TDLib/C++ transfer engine is active. Telegram Premium: {}. The saved TDLib session will be reused after app restarts.",
                    match self.is_premium {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => "unknown",
                    }
                )
            } else {
                format!("TDLib authorization state: {}", self.auth_state)
            },
        }
    }
}

fn read_worker_line(reader: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|e| format!("Failed reading TDLib worker response: {e}"))?;
    if bytes == 0 {
        return Err("TDLib worker stopped unexpectedly".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("Invalid TDLib worker response: {e}"))
}

fn platform_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

fn is_tdlib_unauthorized(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("tdlib error 401") || normalized.contains("unauthorized")
}

async fn reset_tdlib_authorization(
    app: &tauri::AppHandle,
    state: &TdlibFastState,
) -> Result<(), String> {
    abort_worker(state).await;

    let session_dir = app_data_dir(app)?.join(SESSION_DIR_NAME);
    match tokio::fs::remove_dir_all(&session_dir).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to reset stale TDLib authorization: {error}"
            ));
        }
    }

    Ok(())
}

fn secure_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to secure {}: {e}", path.display()))?;
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to secure {}: {e}", path.display()))?;
    }
    Ok(())
}

fn directory_has_state(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some()
}

fn load_or_create_database_key(app_data: &Path, db_dir: &Path) -> Result<String, String> {
    let key_path = app_data.join(KEY_FILE_NAME);
    if let Ok(existing) = fs::read_to_string(&key_path) {
        let key = existing.trim();
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }

    // Earlier experimental builds could leave an unencrypted TDLib database.
    // Preserve that database long enough to let the user reuse it. New databases
    // always get a random local key with 0600 permissions.
    if directory_has_state(db_dir) {
        return Ok(String::new());
    }

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let key = STANDARD.encode(bytes);
    fs::write(&key_path, &key).map_err(|e| format!("Failed to persist TDLib database key: {e}"))?;
    secure_file(&key_path)?;
    Ok(key)
}

fn find_named_library(root: &Path, prefix: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    WalkDir::new(root)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .find_map(|entry| {
            if !entry.file_type().is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with(prefix) {
                Some(entry.path().to_path_buf())
            } else {
                None
            }
        })
}

fn runtime_info(app_data: &Path) -> RuntimeInfo {
    let root = app_data.join("tdlib");
    let mut library = find_named_library(&root, "libtdjson.so");
    let mut sqlcipher = find_named_library(&root, "libsqlcipher.so");

    if library.is_none() {
        for candidate in [
            "/usr/lib/x86_64-linux-gnu/TDLib/libtdjson.so",
            "/usr/lib/x86_64-linux-gnu/libtdjson.so",
            "/usr/local/lib/libtdjson.so",
            "/usr/lib/libtdjson.so",
        ] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                library = Some(path);
                sqlcipher = None;
                break;
            }
        }
    }

    let bundled = library
        .as_ref()
        .map(|p| p.starts_with(&root))
        .unwrap_or(false);
    let installed = library.is_some() && (!bundled || sqlcipher.is_some());
    RuntimeInfo {
        installed,
        library,
        sqlcipher,
    }
}

async fn download_checked(
    url: &str,
    expected_sha256: &str,
    destination: &Path,
) -> Result<(), String> {
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("TDLib runtime download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "TDLib runtime download returned HTTP {}",
            response.status()
        ));
    }

    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|e| format!("Failed to create {}: {e}", destination.display()))?;
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("TDLib runtime download failed: {e}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Failed writing {}: {e}", destination.display()))?;
    }
    file.flush()
        .await
        .map_err(|e| format!("Failed writing {}: {e}", destination.display()))?;

    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        let _ = tokio::fs::remove_file(destination).await;
        return Err(format!(
            "TDLib runtime checksum mismatch for {}",
            destination.display()
        ));
    }
    Ok(())
}

async fn install_runtime(app_data: &Path) -> Result<RuntimeInfo, String> {
    let current = runtime_info(app_data);
    if current.installed {
        return Ok(current);
    }
    if !platform_supported() {
        return Err("Automatic TDLib setup is currently supported on Linux x86_64.".to_string());
    }
    if !Path::new("/usr/bin/dpkg-deb").is_file() {
        return Err(
            "/usr/bin/dpkg-deb is required to install the pinned TDLib runtime.".to_string(),
        );
    }

    secure_dir(app_data)?;
    let nonce = rand::random::<u64>();
    let staging = app_data.join(format!("tdlib-install-{nonce}"));
    let extracted = staging.join("root");
    tokio::fs::create_dir_all(&extracted)
        .await
        .map_err(|e| format!("Failed to create TDLib staging directory: {e}"))?;
    let sql_deb = staging.join("libsqlcipher1.deb");
    let td_deb = staging.join("libtdjson.deb");

    let install_result: Result<(), String> = async {
        download_checked(SQLCIPHER_URL, SQLCIPHER_SHA256, &sql_deb).await?;
        download_checked(TDJSON_URL, TDJSON_SHA256, &td_deb).await?;

        for package in [&sql_deb, &td_deb] {
            let output = Command::new("/usr/bin/dpkg-deb")
                .arg("-x")
                .arg(package)
                .arg(&extracted)
                .output()
                .await
                .map_err(|e| format!("Failed to run dpkg-deb: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "dpkg-deb failed while unpacking TDLib runtime: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }

        let staged_library = find_named_library(&extracted, "libtdjson.so");
        let staged_sqlcipher = find_named_library(&extracted, "libsqlcipher.so");
        if staged_library.is_none() || staged_sqlcipher.is_none() {
            return Err(
                "Pinned TDLib packages did not contain the expected runtime libraries.".to_string(),
            );
        }

        let runtime_root = app_data.join("tdlib");
        if runtime_root.exists() {
            tokio::fs::remove_dir_all(&runtime_root)
                .await
                .map_err(|e| format!("Failed to replace old TDLib runtime: {e}"))?;
        }
        tokio::fs::rename(&extracted, &runtime_root)
            .await
            .map_err(|e| format!("Failed to install TDLib runtime: {e}"))?;
        Ok(())
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&staging).await;
    install_result?;

    let installed = runtime_info(app_data);
    if !installed.installed {
        return Err(
            "TDLib runtime install finished but the library could not be loaded.".to_string(),
        );
    }
    Ok(installed)
}

fn resolve_python() -> Result<PathBuf, String> {
    for candidate in ["/usr/bin/python3", "/usr/local/bin/python3"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("Python 3 is required for TeraRelay's isolated TDLib worker.".to_string())
}

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get TeraRelay app data directory: {e}"))
}

fn decode_qr_login_token(link: &str) -> Result<Vec<u8>, String> {
    let token = link
        .split_once("token=")
        .map(|(_, token)| token)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "TDLib QR login link did not contain a token".to_string())?;
    URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|e| format!("TDLib QR login token was invalid: {e}"))
}

async fn authorize_tdlib_from_existing_session(
    tdlib_state: &TdlibFastState,
    telegram_state: &crate::commands::TelegramState,
) -> Result<Option<FastTransferStatus>, String> {
    let client = {
        let guard = telegram_state.client.lock().await;
        guard.clone()
    };
    let Some(client) = client else {
        return Ok(None);
    };

    if client.get_me().await.is_err() {
        return Ok(None);
    }

    let worker_arc = tdlib_state.worker.clone();
    let qr_result = tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| "TDLib setup has not been started.".to_string())?;
        let result = worker.request(json!({"action": "qr_start"}), |_| {}, |_| {}, None)?;
        worker.update_auth_from_result(&result);
        Ok::<Value, String>(result)
    })
    .await
    .map_err(|e| format!("TDLib QR authorization task failed: {e}"))??;

    if qr_result
        .get("ready")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let worker_arc = tdlib_state.worker.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let guard = worker_arc
                .lock()
                .map_err(|_| "TDLib worker lock poisoned".to_string())?;
            let worker = guard
                .as_ref()
                .ok_or_else(|| "TDLib setup has not been started.".to_string())?;
            Ok::<Option<FastTransferStatus>, String>(Some(worker.status(true)))
        })
        .await
        .map_err(|e| format!("TDLib QR authorization task failed: {e}"))?;
    }

    let link = qr_result
        .get("link")
        .and_then(Value::as_str)
        .ok_or_else(|| "TDLib QR authorization did not return a login link".to_string())?;
    let token = decode_qr_login_token(link)?;

    client
        .invoke(&tl::functions::auth::AcceptLoginToken { token })
        .await
        .map_err(|e| format!("Telegram could not approve TDLib device login: {e}"))?;

    let worker_arc = tdlib_state.worker.clone();
    let linked = tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| "TDLib setup has not been started.".to_string())?;
        let result = worker.request(json!({"action": "qr_wait"}), |_| {}, |_| {}, None)?;
        worker.update_auth_from_result(&result);
        Ok::<FastTransferStatus, String>(worker.status(true))
    })
    .await
    .map_err(|e| format!("TDLib QR authorization task failed: {e}"))??;

    Ok(Some(linked))
}

async fn prepare_worker(
    app: &tauri::AppHandle,
    state: &TdlibFastState,
    api_id: i32,
    api_hash: String,
    install: bool,
) -> Result<FastTransferStatus, String> {
    if api_hash.trim().is_empty() {
        return Err("Saved Telegram API hash is unavailable.".to_string());
    }

    let app_data = app_data_dir(app)?;
    let runtime = if install {
        install_runtime(&app_data).await?
    } else {
        runtime_info(&app_data)
    };
    if !runtime.installed {
        return Ok(FastTransferStatus {
            supported: platform_supported(),
            runtime_installed: false,
            ready: false,
            auth_state: "runtime-missing".to_string(),
            version: None,
            is_premium: None,
            backend: "TDLib/C++".to_string(),
            detail: "TDLib runtime is not installed yet.".to_string(),
        });
    }

    let worker_arc = state.worker.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(NativeWorker::spawn(&runtime, api_id, api_hash, &app_data)?);
        }

        let worker = guard.as_mut().expect("worker initialized");
        let result = worker.request(
            json!({"action": "prepare", "timeout": 15}),
            |_| {},
            |_| {},
            None,
        )?;
        worker.update_auth_from_result(&result);
        Ok::<FastTransferStatus, String>(worker.status(true))
    })
    .await
    .map_err(|e| format!("TDLib worker task failed: {e}"))?
}

#[tauri::command]
pub async fn cmd_fast_transfer_status(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
) -> Result<FastTransferStatus, String> {
    let app_data = app_data_dir(&app_handle)?;
    let runtime = runtime_info(&app_data);
    let worker_arc = state.worker.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        if let Some(worker) = guard.as_mut() {
            match worker.request(
                json!({"action": "status", "timeout": 5}),
                |_| {},
                |_| {},
                None,
            ) {
                Ok(result) => {
                    worker.update_auth_from_result(&result);
                    return Ok(worker.status(runtime.installed));
                }
                Err(error) if is_tdlib_unauthorized(&error) => {
                    log::warn!(
                        "TDLib saved authorization is no longer valid; it will be relinked automatically."
                    );
                    worker.ready = false;
                    worker.auth_state = "unauthorized".to_string();
                    return Ok(worker.status(runtime.installed));
                }
                Err(error) => return Err(error),
            }
        }

        Ok(FastTransferStatus {
            supported: platform_supported(),
            runtime_installed: runtime.installed,
            ready: false,
            auth_state: if runtime.installed {
                "stopped".to_string()
            } else {
                "runtime-missing".to_string()
            },
            version: None,
            is_premium: None,
            backend: "TDLib/C++".to_string(),
            detail: if runtime.installed {
                "TDLib is installed. A saved authorization will resume automatically when an upload starts."
                    .to_string()
            } else {
                "TDLib runtime needs one-time setup.".to_string()
            },
        })
    })
    .await
    .map_err(|e| format!("TDLib status task failed: {e}"))?
}

async fn prepare_with_existing_session(
    app_handle: &tauri::AppHandle,
    state: &TdlibFastState,
    telegram_state: &crate::commands::TelegramState,
    api_id: i32,
    api_hash: String,
    install: bool,
) -> Result<FastTransferStatus, String> {
    let mut status = match prepare_worker(app_handle, state, api_id, api_hash.clone(), install)
        .await
    {
        Ok(status) => status,
        Err(error) if is_tdlib_unauthorized(&error) => {
            log::warn!(
                "TDLib saved authorization was rejected; rebuilding it from the active TeraRelay login."
            );
            reset_tdlib_authorization(app_handle, state).await?;
            prepare_worker(app_handle, state, api_id, api_hash.clone(), false).await?
        }
        Err(error) => return Err(error),
    };

    // Do not erase TDLib authorization just because startup has not reached
    // Ready yet. Password/code states are valid authorization continuations,
    // and transient startup/closing states must be allowed to settle. Only a
    // confirmed 401 above, or an explicit manual-login fallback, resets the DB.
    if status.auth_state == "closed" {
        abort_worker(state).await;
        status = prepare_worker(app_handle, state, api_id, api_hash.clone(), false).await?;
    }

    if status.ready
        || matches!(status.auth_state.as_str(), "password" | "code")
        || status.auth_state != "phone"
    {
        return Ok(status);
    }

    match authorize_tdlib_from_existing_session(state, telegram_state).await {
        Ok(Some(linked)) => {
            if linked.ready {
                log::info!(
                    "TDLib authorized automatically from the existing TeraRelay Telegram session."
                );
            }
            Ok(linked)
        }
        Ok(None) => Ok(status),
        Err(error) => {
            log::warn!(
                "Automatic TDLib device authorization was unavailable: {}",
                error
            );
            Ok(status)
        }
    }
}

#[tauri::command]
pub async fn cmd_fast_transfer_prepare(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
    telegram_state: State<'_, crate::commands::TelegramState>,
    api_id: i32,
    api_hash: String,
    install: bool,
) -> Result<FastTransferStatus, String> {
    prepare_with_existing_session(
        &app_handle,
        state.inner(),
        telegram_state.inner(),
        api_id,
        api_hash,
        install,
    )
    .await
}

fn select_fast_transfer_credentials(
    saved: Result<crate::commands::secure_credentials::StoredTelegramApiCredentials, String>,
    allow_ephemeral: bool,
    qa_persisted: Option<crate::commands::secure_credentials::StoredTelegramApiCredentials>,
    ephemeral: Option<crate::commands::secure_credentials::StoredTelegramApiCredentials>,
) -> Result<crate::commands::secure_credentials::StoredTelegramApiCredentials, String> {
    let valid = |credentials: crate::commands::secure_credentials::StoredTelegramApiCredentials| {
        (credentials.api_id > 0 && !credentials.api_hash.trim().is_empty()).then_some(credentials)
    };
    match saved {
        Ok(credentials) => Ok(credentials),
        Err(saved_error) if allow_ephemeral => qa_persisted
            .and_then(valid)
            .or_else(|| ephemeral.and_then(valid))
            .ok_or(saved_error),
        Err(saved_error) => Err(saved_error),
    }
}

async fn fast_transfer_credentials(
    app_handle: &tauri::AppHandle,
    telegram_state: &crate::commands::TelegramState,
) -> Result<crate::commands::secure_credentials::StoredTelegramApiCredentials, String> {
    let saved = crate::commands::secure_credentials::load_api_credentials(app_handle);
    let allow_ephemeral = crate::commands::auth::real_e2e_qa_ephemeral_login_enabled();
    let qa_persisted = if allow_ephemeral {
        match crate::commands::secure_credentials::load_qa_kernel_api_credentials(app_handle) {
            Ok(credentials) => credentials,
            Err(error) => {
                log::warn!("QA kernel credential lookup was unavailable: {error}");
                None
            }
        }
    } else {
        None
    };
    let ephemeral = if allow_ephemeral {
        let api_id = *telegram_state.api_id.lock().await;
        let api_hash = telegram_state.ephemeral_api_hash.lock().await.clone();
        match (api_id, api_hash) {
            (Some(api_id), Some(api_hash)) => Some(
                crate::commands::secure_credentials::StoredTelegramApiCredentials {
                    api_id,
                    api_hash,
                },
            ),
            _ => None,
        }
    } else {
        None
    };

    let credentials =
        select_fast_transfer_credentials(saved, allow_ephemeral, qa_persisted, ephemeral)?;
    if allow_ephemeral {
        log::debug!("TDLib credentials resolved for explicit real-E2E QA mode.");
    }
    Ok(credentials)
}

#[tauri::command]
pub async fn cmd_fast_transfer_prepare_qa_credentials(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
    telegram_state: State<'_, crate::commands::TelegramState>,
    api_hash: String,
) -> Result<FastTransferStatus, String> {
    if !crate::commands::auth::real_e2e_qa_ephemeral_login_enabled() {
        return Err(
            "TDLib QA credential setup is available only in explicitly enabled debug QA mode."
                .to_string(),
        );
    }
    let api_hash = api_hash.trim().to_string();
    if api_hash.is_empty() {
        return Err("Telegram API Hash cannot be empty.".to_string());
    }
    let api_id = telegram_state
        .api_id
        .lock()
        .await
        .ok_or_else(|| "The active Telegram session has no API ID.".to_string())?;

    crate::commands::secure_credentials::save_qa_kernel_api_credentials(
        &app_handle,
        api_id,
        &api_hash,
    )?;
    *telegram_state.ephemeral_api_hash.lock().await = Some(api_hash.clone());

    prepare_with_existing_session(
        &app_handle,
        state.inner(),
        telegram_state.inner(),
        api_id,
        api_hash,
        true,
    )
    .await
}

#[tauri::command]
pub async fn cmd_fast_transfer_prepare_saved(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
    telegram_state: State<'_, crate::commands::TelegramState>,
    install: bool,
) -> Result<FastTransferStatus, String> {
    let credentials = fast_transfer_credentials(&app_handle, telegram_state.inner()).await?;
    prepare_with_existing_session(
        &app_handle,
        state.inner(),
        telegram_state.inner(),
        credentials.api_id,
        credentials.api_hash,
        install,
    )
    .await
}

#[tauri::command]
pub async fn cmd_fast_transfer_prepare_manual_saved(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
    telegram_state: State<'_, crate::commands::TelegramState>,
    install: bool,
) -> Result<FastTransferStatus, String> {
    let credentials = fast_transfer_credentials(&app_handle, telegram_state.inner()).await?;

    reset_tdlib_authorization(&app_handle, state.inner()).await?;

    prepare_worker(
        &app_handle,
        state.inner(),
        credentials.api_id,
        credentials.api_hash,
        install,
    )
    .await
}

async fn auth_action(
    state: &TdlibFastState,
    action: &'static str,
    field: &'static str,
    value: String,
) -> Result<FastTransferStatus, String> {
    let worker_arc = state.worker.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| "TDLib setup has not been started.".to_string())?;
        let result = worker.request(
            json!({"action": action, field: value}),
            |_| {},
            |_| {},
            None,
        )?;
        worker.update_auth_from_result(&result);
        Ok::<FastTransferStatus, String>(worker.status(true))
    })
    .await
    .map_err(|e| format!("TDLib authorization task failed: {e}"))?
}

#[tauri::command]
pub async fn cmd_fast_transfer_logout_saved(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
) -> Result<bool, String> {
    let credentials = crate::commands::secure_credentials::load_api_credentials(&app_handle).ok();
    let (api_id, api_hash) = credentials
        .map(|value| (value.api_id, value.api_hash))
        .unwrap_or((0, String::new()));
    cmd_fast_transfer_logout(app_handle, state, api_id, api_hash).await
}

#[tauri::command]
pub async fn cmd_fast_transfer_phone(
    state: State<'_, TdlibFastState>,
    phone: String,
) -> Result<FastTransferStatus, String> {
    auth_action(state.inner(), "phone", "phone", phone).await
}

#[tauri::command]
pub async fn cmd_fast_transfer_code(
    state: State<'_, TdlibFastState>,
    code: String,
) -> Result<FastTransferStatus, String> {
    auth_action(state.inner(), "code", "code", code).await
}

#[tauri::command]
pub async fn cmd_fast_transfer_password(
    state: State<'_, TdlibFastState>,
    password: String,
) -> Result<FastTransferStatus, String> {
    auth_action(state.inner(), "password", "password", password).await
}

#[tauri::command]
pub async fn cmd_fast_transfer_logout(
    app_handle: tauri::AppHandle,
    state: State<'_, TdlibFastState>,
    api_id: i32,
    api_hash: String,
) -> Result<bool, String> {
    if !api_hash.trim().is_empty() {
        match prepare_worker(&app_handle, state.inner(), api_id, api_hash, false).await {
            Ok(status) if status.ready => {
                let worker_arc = state.inner().worker.clone();
                let logout_result = tauri::async_runtime::spawn_blocking(move || {
                    let mut guard = worker_arc
                        .lock()
                        .map_err(|_| "TDLib worker lock poisoned".to_string())?;
                    if let Some(worker) = guard.as_mut() {
                        let result =
                            worker.request(json!({"action": "logout"}), |_| {}, |_| {}, None);
                        guard.take();
                        result.map(|_| ())
                    } else {
                        Ok(())
                    }
                })
                .await
                .map_err(|e| format!("TDLib logout task failed: {e}"))?;
                if let Err(error) = logout_result {
                    log::warn!("TDLib server logout did not complete cleanly: {}", error);
                }
            }
            Ok(_) => {}
            Err(error) => {
                log::warn!(
                    "TDLib session could not be reopened for server logout; clearing local authorization: {}",
                    error
                );
            }
        }
    }

    // Always destroy the in-memory worker before removing its on-disk
    // authorization database. A stale worker must never survive logout and
    // be reused by the next Telegram login.
    abort_worker(state.inner()).await;

    let app_data = app_data_dir(&app_handle)?;
    for path in [
        app_data.join(SESSION_DIR_NAME),
        app_data.join(FILES_DIR_NAME),
    ] {
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("Failed to clear TDLib logout data: {error}"));
            }
        }
    }
    match tokio::fs::remove_file(app_data.join(KEY_FILE_NAME)).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Failed to clear TDLib logout key: {error}")),
    }

    Ok(true)
}

pub async fn upload_document(
    state: &TdlibFastState,
    destination: FastDestination,
    path: String,
    caption: String,
    bytes_counter: Arc<AtomicU64>,
    network_counter: Arc<AtomicU64>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<i64, String> {
    let worker_arc = state.worker.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| "TDLIB_SETUP_REQUIRED".to_string())?;
        if !worker.ready {
            return Err("TDLIB_SETUP_REQUIRED".to_string());
        }

        log::info!(
            "TDLib upload request starting: destination={:?}, path={}",
            destination,
            path
        );
        let started = std::time::Instant::now();
        let mut sample_started = started;
        let mut sample_network_bytes = 0u64;
        let mut acknowledged_total = 0u64;
        let progress_log_counter = bytes_counter.clone();
        let result = worker.request(
            json!({
                "action": "upload",
                "destination": destination.as_json(),
                "path": path,
                "caption": caption,
                "timeout": 7200,
            }),
            |delta| {
                bytes_counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
                acknowledged_total = acknowledged_total.saturating_add(delta);
            },
            |delta| {
                network_counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
                sample_network_bytes = sample_network_bytes.saturating_add(delta);
                let elapsed = sample_started.elapsed().as_secs_f64();
                if elapsed >= 1.0 {
                    log::info!(
                        "TDLib upload acknowledged: file_progress={} bytes, speed={:.2} MiB/s",
                        progress_log_counter.load(std::sync::atomic::Ordering::Relaxed),
                        sample_network_bytes as f64 / elapsed / (1024.0 * 1024.0)
                    );
                    sample_started = std::time::Instant::now();
                    sample_network_bytes = 0;
                }
            },
            Some(&cancel_rx),
        );

        if let Err(error) = &result {
            log::error!(
                "TDLib upload request failed after {:.2}s: {}",
                started.elapsed().as_secs_f64(),
                error
            );
        }

        if result.as_ref().err().map(String::as_str) == Some("Transfer cancelled") {
            guard.take();
            return Err("Transfer cancelled".to_string());
        }

        let result = result?;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        log::info!(
            "TDLib upload completed: acknowledged={} bytes in {:.2}s, average={:.2} MiB/s",
            acknowledged_total,
            elapsed,
            acknowledged_total as f64 / elapsed / (1024.0 * 1024.0)
        );
        let tdlib_message_id = result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "TDLib upload succeeded without a final message ID".to_string())?;
        let server_message_id =
            crate::commands::utils::tdlib_message_id_to_server_id(tdlib_message_id)?;
        log::info!(
            "TDLib message ID normalized: tdlib_id={} -> server_id={}",
            tdlib_message_id,
            server_message_id
        );
        Ok(server_message_id)
    })
    .await
    .map_err(|e| format!("TDLib upload task failed: {e}"))?
}

pub async fn download_documents(
    state: &TdlibFastState,
    destination: FastDestination,
    path: String,
    chunks: Vec<FastDownloadChunk>,
    bytes_counter: Arc<AtomicU64>,
    network_counter: Arc<AtomicU64>,
    cancel_rx: watch::Receiver<bool>,
    force: bool,
) -> Result<FastDownloadOutcome, String> {
    let worker_arc = state.worker.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut guard = worker_arc
            .lock()
            .map_err(|_| "TDLib worker lock poisoned".to_string())?;
        let worker = guard
            .as_mut()
            .ok_or_else(|| "TDLIB_SETUP_REQUIRED".to_string())?;
        if !worker.ready {
            return Err("TDLIB_SETUP_REQUIRED".to_string());
        }

        let total_expected = chunks.iter().map(|chunk| chunk.size).sum::<u64>();
        log::info!(
            "TDLib download request starting: destination={:?}, chunks={}, bytes={}, path={}",
            destination,
            chunks.len(),
            total_expected,
            path
        );
        let started = std::time::Instant::now();
        let mut sample_started = started;
        let mut sample_network_bytes = 0u64;
        let mut downloaded_total = 0u64;
        let progress_log_counter = bytes_counter.clone();

        let result = worker.request(
            json!({
                "action": "download",
                "destination": destination.as_json(),
                "path": path,
                "chunks": chunks,
                "force": force,
                "inactivity_timeout": 7200,
            }),
            |delta| {
                bytes_counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
                downloaded_total = downloaded_total.saturating_add(delta);
            },
            |delta| {
                network_counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
                sample_network_bytes = sample_network_bytes.saturating_add(delta);
                let elapsed = sample_started.elapsed().as_secs_f64();
                if elapsed >= 1.0 {
                    log::info!(
                        "TDLib download network: file_progress={} bytes, speed={:.2} MiB/s",
                        progress_log_counter.load(std::sync::atomic::Ordering::Relaxed),
                        sample_network_bytes as f64 / elapsed / (1024.0 * 1024.0)
                    );
                    sample_started = std::time::Instant::now();
                    sample_network_bytes = 0;
                }
            },
            Some(&cancel_rx),
        );

        if result.as_ref().err().map(String::as_str) == Some("Transfer cancelled") {
            guard.take();
            return Err("Transfer cancelled".to_string());
        }

        let result = result?;
        let completed = result
            .get("bytes_downloaded")
            .and_then(Value::as_u64)
            .ok_or_else(|| "TDLib download completed without a byte count".to_string())?;
        if completed != total_expected {
            return Err(format!(
                "TDLib reconstructed {} bytes but expected {} bytes",
                completed, total_expected
            ));
        }

        let elapsed = result
            .get("network_seconds")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| started.elapsed().as_secs_f64())
            .max(0.001);
        let average_bytes_per_sec = result
            .get("average_bytes_per_sec")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| (completed as f64 / elapsed) as u64);
        log::info!(
            "TDLib download completed: {} bytes in {:.2}s, network average={:.2} MiB/s",
            completed,
            elapsed,
            average_bytes_per_sec as f64 / (1024.0 * 1024.0)
        );
        Ok(FastDownloadOutcome {
            bytes_downloaded: completed,
            network_seconds: elapsed,
            average_bytes_per_sec,
        })
    })
    .await
    .map_err(|e| format!("TDLib download task failed: {e}"))?
}

pub async fn abort_worker(state: &TdlibFastState) {
    let worker_arc = state.worker.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if let Ok(mut guard) = worker_arc.lock() {
            guard.take();
        }
    })
    .await;
}

/// Gracefully close TDLib on normal application exit without logging the
/// Telegram account out. This lets TDLib flush its persistent authorization
/// database so the next TeraRelay start can silently resume the same session.
pub fn shutdown_worker(state: &TdlibFastState) {
    // Never make window/application exit wait behind a long in-flight upload.
    // When idle, close cleanly so TDLib flushes its DB. If an upload owns the
    // worker lock, process teardown will stop it and the already-persisted auth
    // database remains available on the next launch.
    let Ok(mut guard) = state.worker.try_lock() else {
        return;
    };
    if let Some(worker) = guard.as_mut() {
        let _ = worker.request(json!({"action": "shutdown"}), |_| {}, |_| {}, None);
    }
    guard.take();
}

pub fn tdlib_is_ready(state: &TdlibFastState) -> bool {
    state
        .worker
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|worker| worker.ready))
        .unwrap_or(false)
}

pub async fn create_split_temp(
    app: &tauri::AppHandle,
    source_path: &str,
    offset: u64,
    len: u64,
    file_name: &str,
) -> Result<(PathBuf, String), String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let app_data = app_data_dir(app)?;
    let temp_root = app_data.join("tdlib-upload-parts");
    secure_dir(&temp_root)?;

    let unique_dir = temp_root.join(format!("{}-{}", std::process::id(), rand::random::<u64>()));
    secure_dir(&unique_dir)?;
    // An I/O failure must not retain a partial multi-gigabyte staging file.
    struct FailedSplitCleanup {
        directory: PathBuf,
        keep: bool,
    }
    impl Drop for FailedSplitCleanup {
        fn drop(&mut self) {
            if !self.keep {
                let _ = std::fs::remove_dir_all(&self.directory);
            }
        }
    }
    let mut cleanup = FailedSplitCleanup {
        directory: unique_dir.clone(),
        keep: false,
    };
    // Keep the exact logical Telegram document name as the basename. TDLib uses
    // the local basename as the uploaded document filename.
    let destination = unique_dir.join(file_name);

    let mut source = tokio::fs::File::open(source_path)
        .await
        .map_err(|e| format!("Failed to open split upload source: {e}"))?;
    source
        .seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("Failed to seek split upload source: {e}"))?;
    let mut output = tokio::fs::File::create(&destination)
        .await
        .map_err(|e| format!("Failed to create TDLib split part: {e}"))?;

    log::info!(
        "Preparing TDLib split part: source={}, offset={}, len={}, destination={}",
        source_path,
        offset,
        len,
        destination.display()
    );
    let split_started = std::time::Instant::now();
    let mut remaining = len;
    let mut copied = 0u64;
    let mut next_log = 512u64 * 1024 * 1024;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hasher = Sha256::new();

    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        source
            .read_exact(&mut buffer[..take])
            .await
            .map_err(|e| format!("Failed reading split upload part: {e}"))?;
        output
            .write_all(&buffer[..take])
            .await
            .map_err(|e| format!("Failed writing TDLib split part: {e}"))?;
        hasher.update(&buffer[..take]);
        remaining -= take as u64;
        copied = copied.saturating_add(take as u64);
        if copied >= next_log {
            log::info!(
                "TDLib split preparation progress: copied={} bytes, speed={:.2} MiB/s",
                copied,
                copied as f64
                    / split_started.elapsed().as_secs_f64().max(0.001)
                    / (1024.0 * 1024.0)
            );
            next_log = next_log.saturating_add(512u64 * 1024 * 1024);
        }
    }
    output
        .flush()
        .await
        .map_err(|e| format!("Failed writing TDLib split part: {e}"))?;
    log::info!(
        "TDLib split part prepared: {} bytes in {:.2}s ({:.2} MiB/s)",
        copied,
        split_started.elapsed().as_secs_f64(),
        copied as f64 / split_started.elapsed().as_secs_f64().max(0.001) / (1024.0 * 1024.0)
    );

    cleanup.keep = true;
    Ok((destination, format!("{:x}", hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_sideband_notices_do_not_change_result_or_byte_counters() {
        let script = r#"
import json, sys
request = json.loads(sys.stdin.readline())
i = request["id"]
events = [
    {"event": "progress", "id": i + 1, "delta": 4096},
    {"event": "network", "id": i + 1, "delta": 4096},
    {"event": "speed_limit", "id": i, "is_upload": True},
    {"event": "heartbeat", "id": i},
    {"event": "progress", "id": i, "delta": 12},
    {"event": "network", "id": i, "delta": 0},
    {"event": "network", "id": i, "delta": 12},
    {"event": "speed_limit", "id": i + 1, "is_upload": False},
    {"event": "speed_limit", "id": i, "is_upload": "invalid"},
    {"event": "result", "id": i + 1, "ok": True, "result": {}},
    {"event": "result", "id": i, "ok": True,
     "result": {"message_id": 700, "bytes_uploaded": 12}},
]
for event in events:
    print(json.dumps(event), flush=True)
"#;
        let mut child = std::process::Command::new("python3")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("Python transfer fixture starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut worker = NativeWorker {
            child,
            stdin,
            stdout,
            next_id: 0,
            auth_state: "ready".to_string(),
            ready: true,
            is_premium: Some(false),
        };
        let mut progress = 0;
        let mut network = 0;
        let result = worker
            .request(
                json!({"action": "upload"}),
                |bytes| progress += bytes,
                |bytes| network += bytes,
                None,
            )
            .unwrap();
        assert_eq!(result, json!({"message_id": 700, "bytes_uploaded": 12}));
        assert_eq!((progress, network), (12, 12));
        assert_eq!(worker.auth_state, "ready");
        assert!(worker.ready);
        assert_eq!(worker.is_premium, Some(false));
        worker.child.wait().unwrap();
    }

    #[test]
    fn destination_json_is_stable() {
        assert_eq!(
            FastDestination::SavedMessages.as_json(),
            json!({"kind": "saved"})
        );
        assert_eq!(
            FastDestination::Channel(123).as_json(),
            json!({"kind": "channel", "id": 123})
        );
    }

    #[test]
    fn pinned_hashes_have_expected_length() {
        assert_eq!(SQLCIPHER_SHA256.len(), 64);
        assert_eq!(TDJSON_SHA256.len(), 64);
    }

    #[test]
    fn detects_expired_tdlib_authorization() {
        assert!(is_tdlib_unauthorized("TDLib error 401: Unauthorized"));
        assert!(is_tdlib_unauthorized("unauthorized"));
        assert!(!is_tdlib_unauthorized("TDLib error 500: Internal"));
    }

    #[test]
    fn qa_ephemeral_credentials_only_fallback_when_explicitly_allowed() {
        let ephemeral = crate::commands::secure_credentials::StoredTelegramApiCredentials {
            api_id: 12345,
            api_hash: "ephemeral-hash".to_string(),
        };

        let persisted = crate::commands::secure_credentials::StoredTelegramApiCredentials {
            api_id: 12345,
            api_hash: "kernel-keyring-hash".to_string(),
        };

        let selected = select_fast_transfer_credentials(
            Err("secure store unavailable".to_string()),
            true,
            Some(persisted.clone()),
            Some(ephemeral.clone()),
        )
        .expect("explicit QA mode prefers restart-safe kernel credentials");
        assert_eq!(selected, persisted);

        let blocked = select_fast_transfer_credentials(
            Err("secure store unavailable".to_string()),
            false,
            Some(persisted),
            Some(
                crate::commands::secure_credentials::StoredTelegramApiCredentials {
                    api_id: 12345,
                    api_hash: "ephemeral-hash".to_string(),
                },
            ),
        );
        assert!(blocked.is_err());
    }

    #[test]
    fn qr_login_token_decodes_from_tdlib_link() {
        let raw = b"terarelay-device-token";
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        let decoded = decode_qr_login_token(&format!("tg://login?token={encoded}")).unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn qr_login_token_rejects_missing_token() {
        assert!(decode_qr_login_token("tg://login").is_err());
    }
}
