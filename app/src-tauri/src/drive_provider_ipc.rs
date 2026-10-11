#![cfg(target_os = "macos")]

use crate::commands::TelegramState;
use crate::db::DbConnection;
use crate::drive_fs_core::{
    decode_provider_identity, encode_provider_identity, DriveFsCore, DriveFsError, DriveNodeId,
    DriveNodeKind, DriveNodeMetadata,
};
use crate::drive_mount::{build_drive_fs_core, DriveMountState};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use block2::RcBlock;
use objc2::rc::autoreleasepool;
use objc2::AnyThread;
use objc2_file_provider::{
    NSFileProviderDomain, NSFileProviderManager, NSFileProviderWorkingSetContainerItemIdentifier,
};
use objc2_foundation::{NSError, NSFileManager, NSString};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::AppHandle;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONTENT_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LIST_PAGE_ITEMS: usize = 512;
const MAX_CONCURRENT_CLIENTS: usize = 16;
const APP_GROUP_ID: &str = "group.io.terarelay.drive";
const ENDPOINT_FILE: &str = "provider-endpoint.json";
const CAPABILITY_FILE: &str = "provider-capability";
const PROVIDER_HOST_LOCK_FILE: &str = "provider-host.lock";
const FILE_PROVIDER_DOMAIN_ID: &str = "io.terarelay.drive";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEndpoint {
    pub protocol: u32,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
struct ProviderRequest {
    protocol: u32,
    capability: String,
    op: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    data_b64: Option<String>,
    #[serde(default)]
    content_version: Option<String>,
    #[serde(default)]
    cursor: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderItem {
    id: String,
    parent: String,
    name: String,
    kind: String,
    size: u64,
    created_at_ms: i64,
    updated_at_ms: i64,
    content_version: String,
    pending: bool,
}

#[derive(Debug, Serialize)]
struct ProviderResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<ProviderItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<ProviderItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<u64>,
}

impl ProviderResponse {
    fn empty(revision: Option<u64>) -> Self {
        Self {
            ok: true,
            error: None,
            revision,
            item: None,
            items: None,
            data_b64: None,
            written: None,
            next_cursor: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            revision: None,
            item: None,
            items: None,
            data_b64: None,
            written: None,
            next_cursor: None,
        }
    }
}

struct ProviderIpcServer {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    client_workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    endpoint_path: PathBuf,
    host_lock: File,
    port: u16,
}

impl ProviderIpcServer {
    fn stop(mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Wake the nonblocking accept loop promptly without opening any external
        // socket. The server listens only on 127.0.0.1.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Accepted requests own Drive-core state and may still be flushing data.
        // Join them before publishing shutdown or releasing the cross-process lock,
        // otherwise a replacement host could race an old request against the same
        // App Group endpoint/capability files.
        let client_workers = self
            .client_workers
            .lock()
            .map(|mut workers| workers.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for worker in client_workers {
            let _ = worker.join();
        }
        // Remove this host's endpoint while the cross-process lock is still held,
        // so a newly launched TeraRelay process can never lose its endpoint to a
        // retiring instance.
        let _ = std::fs::remove_file(&self.endpoint_path);
        // SAFETY: host_lock is a valid file descriptor opened by this process.
        let _ = unsafe { libc::flock(self.host_lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

static SERVER: OnceLock<Mutex<Option<ProviderIpcServer>>> = OnceLock::new();

fn server_slot() -> &'static Mutex<Option<ProviderIpcServer>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

struct ActiveClientGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveClientGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn try_reserve_client(active: &AtomicUsize) -> bool {
    active
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            (current < MAX_CONCURRENT_CLIENTS).then_some(current + 1)
        })
        .is_ok()
}

fn reap_finished_client_workers(workers: &Arc<Mutex<Vec<JoinHandle<()>>>>) {
    let finished = {
        let Ok(mut workers) = workers.lock() else {
            return;
        };
        let mut finished = Vec::new();
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                finished.push(workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        finished
    };
    for worker in finished {
        let _ = worker.join();
    }
}

fn group_container() -> Result<PathBuf, String> {
    // Apple explicitly requires resolving App Group containers through
    // FileManager rather than constructing a filesystem location by hand.
    // This keeps the host and the sandboxed File Provider extension on the
    // exact same system-managed container and fails closed if provisioning is
    // missing or invalid.
    let manager = NSFileManager::defaultManager();
    let group = NSString::from_str(APP_GROUP_ID);
    let url = unsafe { manager.containerURLForSecurityApplicationGroupIdentifier(&group) }
        .ok_or_else(|| "TeraRelay File Provider App Group container is unavailable".to_string())?;
    let path_string = url.path().ok_or_else(|| {
        "TeraRelay File Provider App Group URL has no filesystem path".to_string()
    })?;
    let path = PathBuf::from(path_string.to_string());
    let metadata = std::fs::symlink_metadata(&path).map_err(|e| {
        format!("TeraRelay File Provider App Group is not provisioned or accessible: {e}")
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("TeraRelay File Provider group container is unsafe".to_string());
    }
    Ok(path)
}

fn acquire_provider_host_lock(container: &Path) -> Result<File, String> {
    let path = container.join(PROVIDER_HOST_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("Could not open TeraRelay File Provider host lock: {e}"))?;

    // SAFETY: file owns a valid descriptor for the duration of the call. The
    // returned File is kept in ProviderIpcServer so the advisory lock remains
    // held for the full lifetime of this host bridge.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(file);
    }

    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        return Err(
            "Another TeraRelay process already owns the macOS File Provider bridge".to_string(),
        );
    }
    Err(format!(
        "Could not lock the macOS File Provider bridge: {error}"
    ))
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private File Provider state has no parent directory".to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("Private File Provider state target is unsafe".to_string());
        }
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Private File Provider state filename is invalid".to_string())?;
    let mut temporary_path = None;
    let mut temporary_file = None;
    for _ in 0..16 {
        let mut random = [0u8; 12];
        rand::rng().fill_bytes(&mut random);
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = parent.join(format!(".{file_name}.{suffix}.tmp"));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&candidate)
        {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create private File Provider state: {error}"
                ))
            }
        }
    }

    let temporary_path = temporary_path
        .ok_or_else(|| "Could not allocate private File Provider state file".to_string())?;
    let mut file = temporary_file
        .ok_or_else(|| "Could not open private File Provider state file".to_string())?;
    let write_result = (|| -> Result<(), String> {
        file.write_all(bytes)
            .map_err(|e| format!("Could not persist private File Provider state: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("Could not sync private File Provider state: {e}"))?;
        std::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Could not secure private File Provider state: {e}"))?;
        std::fs::rename(&temporary_path, path)
            .map_err(|e| format!("Could not publish private File Provider state: {e}"))?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    write_result
}

fn load_or_create_capability(container: &Path) -> Result<String, String> {
    let path = container.join(CAPABILITY_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("File Provider capability state is unsafe".to_string())
        }
        Ok(_) => {
            if let Ok(existing) = std::fs::read_to_string(&path) {
                let value = existing.trim();
                if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|e| format!("Could not secure File Provider capability: {e}"))?;
                    return Ok(value.to_string());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect File Provider capability: {error}"
            ))
        }
    }
    let mut random = [0u8; 32];
    rand::rng().fill_bytes(&mut random);
    let capability = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    private_write(&path, capability.as_bytes())?;
    Ok(capability)
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn encode_id(id: &DriveNodeId) -> Result<String, String> {
    encode_provider_identity(id)
        .map(|bytes| BASE64.encode(bytes))
        .map_err(|e| e.to_string())
}

fn decode_id(value: &str) -> Result<DriveNodeId, String> {
    let bytes = BASE64
        .decode(value)
        .map_err(|_| "File Provider item identifier is invalid base64".to_string())?;
    decode_provider_identity(&bytes).map_err(|e| e.to_string())
}

fn wire_item(item: DriveNodeMetadata) -> Result<ProviderItem, String> {
    Ok(ProviderItem {
        id: encode_id(&item.id)?,
        parent: encode_id(&item.parent)?,
        name: item.name,
        kind: match item.kind {
            DriveNodeKind::Directory => "directory".to_string(),
            DriveNodeKind::File => "file".to_string(),
        },
        size: item.size,
        created_at_ms: item.created_at_ms,
        updated_at_ms: item.updated_at_ms,
        content_version: item.content_version,
        pending: item.pending,
    })
}

fn required_id(value: &Option<String>, field: &str) -> Result<DriveNodeId, String> {
    let value = value
        .as_deref()
        .ok_or_else(|| format!("File Provider request is missing {field}"))?;
    decode_id(value)
}

fn required_name(value: &Option<String>) -> Result<&str, String> {
    value
        .as_deref()
        .ok_or_else(|| "File Provider request is missing name".to_string())
}

fn handle_operation(
    core: &DriveFsCore,
    request: &ProviderRequest,
) -> Result<ProviderResponse, String> {
    match request.op.as_str() {
        "revision" => Ok(ProviderResponse::empty(Some(
            core.revision().map_err(|e| e.to_string())?,
        ))),
        "metadata" => {
            let id = required_id(&request.id, "id")?;
            let item = wire_item(core.metadata(&id).map_err(|e| e.to_string())?)?;
            let mut response =
                ProviderResponse::empty(Some(core.revision().map_err(|e| e.to_string())?));
            response.item = Some(item);
            Ok(response)
        }
        "lookup" => {
            let parent = required_id(&request.parent, "parent")?;
            let item = core
                .lookup(&parent, required_name(&request.name)?)
                .map_err(|e| e.to_string())?;
            let mut response =
                ProviderResponse::empty(Some(core.revision().map_err(|e| e.to_string())?));
            response.item = Some(wire_item(item)?);
            Ok(response)
        }
        "list" => {
            let parent = required_id(&request.parent, "parent")?;
            let cursor = usize::try_from(request.cursor.unwrap_or(0))
                .map_err(|_| "File Provider list cursor is too large".to_string())?;
            let revision_before = core.revision().map_err(|e| e.to_string())?;
            let all_items = core.list_directory(&parent).map_err(|e| e.to_string())?;
            let revision_after = core.revision().map_err(|e| e.to_string())?;
            if revision_before != revision_after {
                return Err("Drive metadata changed during File Provider enumeration".to_string());
            }
            if cursor > all_items.len() {
                return Err("File Provider list cursor is invalid".to_string());
            }
            let end = cursor
                .saturating_add(MAX_LIST_PAGE_ITEMS)
                .min(all_items.len());
            let items = all_items[cursor..end]
                .iter()
                .cloned()
                .map(wire_item)
                .collect::<Result<Vec<_>, _>>()?;
            let mut response = ProviderResponse::empty(Some(revision_before));
            response.items = Some(items);
            if end < all_items.len() {
                response.next_cursor = Some(
                    u64::try_from(end)
                        .map_err(|_| "File Provider list cursor overflow".to_string())?,
                );
            }
            Ok(response)
        }
        "read" => {
            let id = required_id(&request.id, "id")?;
            let offset = request.offset.unwrap_or(0);
            let length = request
                .length
                .ok_or_else(|| "File Provider read is missing length".to_string())?;
            if length > MAX_CONTENT_CHUNK_BYTES {
                return Err("File Provider read exceeds the bounded content chunk".to_string());
            }
            let expected_content_version = request
                .content_version
                .as_deref()
                .ok_or_else(|| "File Provider read is missing content version".to_string())?;
            let data = core
                .read_versioned(&id, expected_content_version, offset, length)
                .map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.data_b64 = Some(BASE64.encode(data));
            Ok(response)
        }
        "createFile" => {
            let parent = required_id(&request.parent, "parent")?;
            let item = core
                .create_file(&parent, required_name(&request.name)?)
                .map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.item = Some(wire_item(item)?);
            Ok(response)
        }
        "createDirectory" => {
            let parent = required_id(&request.parent, "parent")?;
            let item = core
                .create_directory(&parent, required_name(&request.name)?)
                .map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.item = Some(wire_item(item)?);
            Ok(response)
        }
        "beginWrite" => {
            let id = required_id(&request.id, "id")?;
            let item = core.begin_write(&id).map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.item = Some(wire_item(item)?);
            Ok(response)
        }
        "write" => {
            let id = required_id(&request.id, "id")?;
            let data = BASE64
                .decode(
                    request
                        .data_b64
                        .as_deref()
                        .ok_or_else(|| "File Provider write is missing data".to_string())?,
                )
                .map_err(|_| "File Provider write data is invalid base64".to_string())?;
            if data.len() as u64 > MAX_CONTENT_CHUNK_BYTES {
                return Err("File Provider write exceeds the bounded content chunk".to_string());
            }
            let written = core
                .write(&id, request.offset.unwrap_or(0), &data)
                .map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.written = Some(written);
            Ok(response)
        }
        "resize" => {
            let id = required_id(&request.id, "id")?;
            core.resize(
                &id,
                request
                    .size
                    .ok_or_else(|| "File Provider resize is missing size".to_string())?,
            )
            .map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        "flush" => {
            let id = required_id(&request.id, "id")?;
            core.flush(&id).map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        "closeWrite" => {
            let id = required_id(&request.id, "id")?;
            core.close_write(&id).map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        "abortWrite" => {
            let id = required_id(&request.id, "id")?;
            core.abort_write(&id).map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        "rename" => {
            let id = required_id(&request.id, "id")?;
            let parent = required_id(&request.parent, "parent")?;
            let item = core
                .rename(&id, &parent, required_name(&request.name)?)
                .map_err(|e| e.to_string())?;
            let mut response = ProviderResponse::empty(None);
            response.item = Some(wire_item(item)?);
            Ok(response)
        }
        "remove" => {
            let id = required_id(&request.id, "id")?;
            let metadata = core.metadata(&id).map_err(|e| e.to_string())?;
            match metadata.kind {
                DriveNodeKind::File => core.remove_file(&id),
                DriveNodeKind::Directory => core.remove_directory(&id),
            }
            .map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        "removeRecursive" => {
            let id = required_id(&request.id, "id")?;
            core.remove_recursive(&id).map_err(|e| e.to_string())?;
            Ok(ProviderResponse::empty(None))
        }
        _ => Err("File Provider requested an unsupported operation".to_string()),
    }
}

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut length_bytes = [0u8; 4];
    stream
        .read_exact(&mut length_bytes)
        .map_err(|e| format!("Could not read File Provider request length: {e}"))?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 || length > MAX_REQUEST_BYTES {
        return Err("File Provider request length is invalid".to_string());
    }
    let mut payload = vec![0u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|e| format!("Could not read File Provider request: {e}"))?;
    Ok(payload)
}

fn write_frame(stream: &mut TcpStream, response: &ProviderResponse) -> Result<(), String> {
    let payload = serde_json::to_vec(response)
        .map_err(|e| format!("Could not serialize File Provider response: {e}"))?;
    if payload.len() > MAX_RESPONSE_BYTES {
        return Err("File Provider response exceeded the bounded IPC size".to_string());
    }
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&payload))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("Could not write File Provider response: {e}"))
}

fn handle_client(
    mut stream: TcpStream,
    core: &DriveFsCore,
    capability: &str,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Could not bound File Provider read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Could not bound File Provider write timeout: {e}"))?;
    let payload = read_frame(&mut stream)?;
    let request: ProviderRequest = serde_json::from_slice(&payload)
        .map_err(|e| format!("File Provider request JSON is invalid: {e}"))?;
    let response = if request.protocol != PROTOCOL_VERSION {
        ProviderResponse::error("File Provider IPC protocol version mismatch")
    } else if !constant_time_equal(&request.capability, capability) {
        ProviderResponse::error("File Provider IPC capability rejected")
    } else {
        handle_operation(core, &request).unwrap_or_else(ProviderResponse::error)
    };
    write_frame(&mut stream, &response)
}

#[derive(Clone, Copy)]
enum FileProviderControlAction {
    Register,
    SignalWorkingSet,
}

fn file_provider_domain() -> objc2::rc::Retained<NSFileProviderDomain> {
    let identifier = NSString::from_str(FILE_PROVIDER_DOMAIN_ID);
    let display_name = NSString::from_str("TeraRelay");
    unsafe {
        NSFileProviderDomain::initWithIdentifier_displayName(
            NSFileProviderDomain::alloc(),
            &identifier,
            &display_name,
        )
    }
}

fn file_provider_completion(action: &'static str) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            log::debug!("macOS File Provider {action} completed");
        } else {
            // Do not dereference/log NSError contents here: this completion can
            // execute on an Apple-managed queue after the caller has returned.
            log::warn!("macOS File Provider {action} failed");
        }
    })
}

fn run_file_provider_control(action: FileProviderControlAction) -> Result<(), String> {
    autoreleasepool(|_| {
        let domain = file_provider_domain();
        match action {
            FileProviderControlAction::Register => {
                let completion = file_provider_completion("domain registration");
                // This API is asynchronous. Never wait here: Drive startup runs
                // during Tauri setup, and a main-queue completion would otherwise
                // deadlock the app before its event loop starts.
                unsafe { NSFileProviderManager::addDomain_completionHandler(&domain, &completion) };
                Ok(())
            }
            FileProviderControlAction::SignalWorkingSet => {
                let Some(manager) = (unsafe { NSFileProviderManager::managerForDomain(&domain) })
                else {
                    // Registration may still be completing. The initial domain
                    // enumeration and the next continuity sync will converge state.
                    return Ok(());
                };
                let completion = file_provider_completion("working-set signal");
                unsafe {
                    manager.signalEnumeratorForContainerItemIdentifier_completionHandler(
                        NSFileProviderWorkingSetContainerItemIdentifier,
                        &completion,
                    )
                };
                Ok(())
            }
        }
    })
}

pub(crate) fn start(
    app: &AppHandle,
    mount_state: &DriveMountState,
    db_pool: &DbConnection,
    telegram_state: &TelegramState,
) -> Result<ProviderEndpoint, String> {
    let mut slot = server_slot()
        .lock()
        .map_err(|_| "File Provider IPC server state is poisoned".to_string())?;
    if let Some(server) = slot.as_ref() {
        let endpoint = ProviderEndpoint {
            protocol: PROTOCOL_VERSION,
            host: "127.0.0.1".to_string(),
            port: server.port,
        };
        let endpoint_path = group_container()?.join(ENDPOINT_FILE);
        let endpoint_bytes = serde_json::to_vec(&endpoint)
            .map_err(|e| format!("Could not serialize current File Provider endpoint: {e}"))?;
        private_write(&endpoint_path, &endpoint_bytes)?;
        return Ok(endpoint);
    }

    let container = group_container()?;
    let host_lock = acquire_provider_host_lock(&container)?;
    let capability = load_or_create_capability(&container)?;
    let core = build_drive_fs_core(app, mount_state, db_pool, telegram_state)?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| format!("Could not bind File Provider IPC to 127.0.0.1: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Could not configure File Provider IPC listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Could not inspect File Provider IPC endpoint: {e}"))?
        .port();
    let endpoint = ProviderEndpoint {
        protocol: PROTOCOL_VERSION,
        host: "127.0.0.1".to_string(),
        port,
    };
    let endpoint_path = container.join(ENDPOINT_FILE);
    let endpoint_bytes = serde_json::to_vec(&endpoint)
        .map_err(|e| format!("Could not serialize File Provider endpoint: {e}"))?;
    private_write(&endpoint_path, &endpoint_bytes)?;

    let running = Arc::new(AtomicBool::new(true));
    let thread_running = running.clone();
    let worker_capability = capability.clone();
    let active_clients = Arc::new(AtomicUsize::new(0));
    let worker_active_clients = active_clients.clone();
    let client_workers = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
    let worker_client_workers = client_workers.clone();
    let worker = match std::thread::Builder::new()
        .name("terarelay-file-provider-ipc".to_string())
        .spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                reap_finished_client_workers(&worker_client_workers);
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if !thread_running.load(Ordering::SeqCst) {
                            break;
                        }
                        if !peer.ip().is_loopback() {
                            continue;
                        }
                        if !try_reserve_client(&worker_active_clients) {
                            log::warn!(
                                "File Provider IPC concurrency limit reached; rejecting one request"
                            );
                            continue;
                        }

                        let client_core = core.clone();
                        let client_capability = worker_capability.clone();
                        let client_active = worker_active_clients.clone();
                        let client = std::thread::Builder::new()
                            .name("terarelay-file-provider-client".to_string())
                            .spawn(move || {
                                let _slot = ActiveClientGuard {
                                    active: client_active,
                                };
                                if let Err(error) =
                                    handle_client(stream, &client_core, &client_capability)
                                {
                                    log::warn!("File Provider IPC request failed: {error}");
                                }
                            });
                        match client {
                            Ok(handle) => {
                                if let Ok(mut workers) = worker_client_workers.lock() {
                                    workers.push(handle);
                                } else {
                                    // The worker list should never poison during normal
                                    // operation. Joining here preserves shutdown safety.
                                    let _ = handle.join();
                                }
                            }
                            Err(error) => {
                                worker_active_clients.fetch_sub(1, Ordering::SeqCst);
                                log::error!(
                                    "Could not start File Provider IPC client worker: {error}"
                                );
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    Err(error) => {
                        log::error!("File Provider IPC accept failed: {error}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            reap_finished_client_workers(&worker_client_workers);
        }) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = std::fs::remove_file(&endpoint_path);
            return Err(format!("Could not start File Provider IPC thread: {error}"));
        }
    };

    *slot = Some(ProviderIpcServer {
        running,
        worker: Some(worker),
        client_workers,
        endpoint_path,
        host_lock,
        port,
    });
    drop(slot);

    // Domain registration runs inside the signed containing TeraRelay app so
    // FileProvider sees the correct host identity and App Group entitlement.
    // Adding the same explicit replicated domain is idempotent.
    run_file_provider_control(FileProviderControlAction::Register)?;
    Ok(endpoint)
}

pub(crate) fn signal_changes(_app: &AppHandle) -> Result<(), String> {
    let running = server_slot()
        .lock()
        .map_err(|_| "File Provider IPC server state is poisoned".to_string())?
        .is_some();
    if !running {
        return Ok(());
    }
    run_file_provider_control(FileProviderControlAction::SignalWorkingSet)
}

pub(crate) fn stop() {
    let server = server_slot().lock().ok().and_then(|mut slot| slot.take());
    if let Some(server) = server {
        server.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_comparison_rejects_different_lengths_and_bytes() {
        assert!(constant_time_equal("abcd", "abcd"));
        assert!(!constant_time_equal("abcd", "abce"));
        assert!(!constant_time_equal("abcd", "abc"));
    }

    #[test]
    fn endpoint_is_loopback_only() {
        let endpoint = ProviderEndpoint {
            protocol: PROTOCOL_VERSION,
            host: "127.0.0.1".to_string(),
            port: 1234,
        };
        assert_eq!(endpoint.host, "127.0.0.1");
    }
}
