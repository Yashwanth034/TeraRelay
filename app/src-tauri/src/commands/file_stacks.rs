use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use crate::models::FileMetadata;
use grammers_client::types::{Media, Peer};
use grammers_client::{Client, InputMessage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

pub const STACK_SCHEMA_VERSION: u32 = 1;
pub const STACK_FILE_PREFIX: &str = ".terarelay-stack-";
pub const STACK_FILE_SUFFIX: &str = ".json";
pub const STACK_CAPTION_PREFIX: &str = "TRS1:";
const MAX_STACK_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_STACK_MEMBERS: usize = 128;
const MAX_STACK_NAME_CHARS: usize = 512;
const MAX_STACK_LABEL_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStackMemberV1 {
    pub file_id: String,
    pub label: Option<String>,
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStackManifestV1 {
    pub schema_version: u32,
    pub stack_id: String,
    pub logical_channel_id: String,
    pub display_name: String,
    pub primary_file_id: String,
    pub anchor_file_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub members: Vec<FileStackMemberV1>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStackMemberView {
    pub file: FileMetadata,
    pub label: Option<String>,
    pub is_primary: bool,
    pub is_anchor: bool,
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStackView {
    pub stack_id: String,
    pub display_name: String,
    pub primary_file_id: String,
    pub anchor_file_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub members: Vec<FileStackMemberView>,
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn valid_hex_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn new_stack_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn stack_file_name(stack_id: &str) -> String {
    format!("{STACK_FILE_PREFIX}{stack_id}{STACK_FILE_SUFFIX}")
}

pub fn stack_caption(stack_id: &str) -> String {
    format!("{STACK_CAPTION_PREFIX}{stack_id}")
}

pub fn stack_manifest_id(name: &str, caption: &str) -> Option<String> {
    let from_name = name
        .strip_prefix(STACK_FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(STACK_FILE_SUFFIX))
        .filter(|id| valid_hex_id(id));
    let from_caption = caption
        .strip_prefix(STACK_CAPTION_PREFIX)
        .filter(|id| valid_hex_id(id));

    match (from_name, from_caption) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        _ => None,
    }
}

pub fn validate_stack_manifest(
    manifest: &FileStackManifestV1,
    expected_stack_id: Option<&str>,
    expected_logical_channel_id: Option<&str>,
) -> Result<(), String> {
    if manifest.schema_version != STACK_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported TeraRelay stack metadata version {}",
            manifest.schema_version
        ));
    }
    if !valid_hex_id(&manifest.stack_id) {
        return Err("Invalid TeraRelay stack ID".to_string());
    }
    if let Some(expected) = expected_stack_id {
        if manifest.stack_id != expected {
            return Err("Stack metadata ID does not match its Telegram document".to_string());
        }
    }
    if manifest.logical_channel_id.is_empty() {
        return Err("Stack metadata has no logical channel ID".to_string());
    }
    if let Some(expected) = expected_logical_channel_id {
        if manifest.logical_channel_id != expected {
            return Err("Stack metadata belongs to a different TeraRelay channel".to_string());
        }
    }
    let display_name = manifest.display_name.trim();
    if display_name.is_empty() {
        return Err("Stack has no display name".to_string());
    }
    if display_name.chars().count() > MAX_STACK_NAME_CHARS {
        return Err("Stack display name is too long".to_string());
    }
    if manifest.members.len() < 2 {
        return Err("A version stack must contain at least two files".to_string());
    }
    if manifest.members.len() > MAX_STACK_MEMBERS {
        return Err(format!(
            "A version stack can contain at most {MAX_STACK_MEMBERS} files"
        ));
    }

    let mut seen = HashSet::new();
    for member in &manifest.members {
        if !valid_hex_id(&member.file_id) {
            return Err("Stack contains an invalid logical file ID".to_string());
        }
        if !seen.insert(member.file_id.as_str()) {
            return Err("Stack contains the same file more than once".to_string());
        }
        if let Some(label) = member.label.as_deref() {
            if label.chars().count() > MAX_STACK_LABEL_CHARS {
                return Err(format!(
                    "Version labels can contain at most {MAX_STACK_LABEL_CHARS} characters"
                ));
            }
        }
    }

    if !seen.contains(manifest.primary_file_id.as_str()) {
        return Err("Stack primary file is not a member of the stack".to_string());
    }
    if !seen.contains(manifest.anchor_file_id.as_str()) {
        return Err("Stack anchor file is not a member of the stack".to_string());
    }
    Ok(())
}

fn encode_stack_manifest(manifest: &FileStackManifestV1) -> Result<Vec<u8>, String> {
    validate_stack_manifest(manifest, None, None)?;
    let bytes = serde_json::to_vec(manifest)
        .map_err(|e| format!("Failed to encode TeraRelay stack metadata: {e}"))?;
    if bytes.len() as u64 > MAX_STACK_MANIFEST_BYTES {
        return Err("TeraRelay stack metadata exceeds the safety limit".to_string());
    }
    Ok(bytes)
}

fn decode_stack_manifest(
    bytes: &[u8],
    expected_stack_id: Option<&str>,
    expected_logical_channel_id: Option<&str>,
) -> Result<FileStackManifestV1, String> {
    if bytes.len() as u64 > MAX_STACK_MANIFEST_BYTES {
        return Err("TeraRelay stack metadata exceeds the safety limit".to_string());
    }
    let manifest: FileStackManifestV1 = serde_json::from_slice(bytes)
        .map_err(|e| format!("Invalid TeraRelay stack metadata JSON: {e}"))?;
    validate_stack_manifest(&manifest, expected_stack_id, expected_logical_channel_id)?;
    Ok(manifest)
}

pub async fn download_stack_manifest(
    client: &Client,
    media: &Media,
    expected_stack_id: Option<&str>,
    expected_logical_channel_id: Option<&str>,
) -> Result<FileStackManifestV1, String> {
    let mut data = Vec::new();
    let mut download = client.iter_download(media).chunk_size(64 * 1024);
    loop {
        match download.next().await {
            Ok(Some(chunk)) => {
                if data.len() as u64 + chunk.len() as u64 > MAX_STACK_MANIFEST_BYTES {
                    return Err("TeraRelay stack metadata exceeds the safety limit".to_string());
                }
                data.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("Failed to download TeraRelay stack metadata: {e}")),
        }
    }
    if data.is_empty() {
        return Err("TeraRelay stack metadata document is empty".to_string());
    }
    decode_stack_manifest(&data, expected_stack_id, expected_logical_channel_id)
}

async fn write_stack_manifest_temp(
    app: &AppHandle,
    manifest: &FileStackManifestV1,
) -> Result<(PathBuf, u64), String> {
    let bytes = encode_stack_manifest(manifest)?;
    let root = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("Failed to resolve TeraRelay cache directory: {e}"))?
        .join("stack-manifest-staging");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| format!("Failed to create stack metadata staging directory: {e}"))?;
    let path = root.join(stack_file_name(&manifest.stack_id));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| format!("Failed to stage TeraRelay stack metadata: {e}"))?;
    Ok((path, bytes.len() as u64))
}

async fn upload_stack_manifest_document(
    client: &Client,
    app: &AppHandle,
    manifest: &FileStackManifestV1,
    peer: &Peer,
) -> Result<i64, String> {
    let (temp_path, size) = write_stack_manifest_temp(app, manifest).await?;
    let result = async {
        let mut file = tokio::fs::File::open(&temp_path)
            .await
            .map_err(|e| format!("Failed to open staged stack metadata: {e}"))?;
        let uploaded = client
            .upload_stream(
                &mut file,
                size as usize,
                stack_file_name(&manifest.stack_id),
            )
            .await
            .map_err(map_error)?;
        let message = InputMessage::new()
            .text(stack_caption(&manifest.stack_id))
            .file(uploaded);
        let sent = client
            .send_message(peer, message)
            .await
            .map_err(map_error)?;
        Ok::<i64, String>(sent.id() as i64)
    }
    .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    result
}

async fn find_stack_document_ids(
    client: &Client,
    peer: &Peer,
    stack_id: &str,
) -> Result<Vec<i32>, String> {
    let mut ids = Vec::new();
    let mut messages = client.iter_messages(peer);
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        let Some(Media::Document(document)) = message.media() else {
            continue;
        };
        if stack_manifest_id(document.name(), message.text()).as_deref() == Some(stack_id) {
            ids.push(message.id());
        }
    }
    Ok(ids)
}

async fn load_current_stack_manifest(
    client: &Client,
    peer: &Peer,
    stack_id: &str,
    logical_channel_id: &str,
) -> Result<FileStackManifestV1, String> {
    let mut messages = client.iter_messages(peer);
    let mut last_error: Option<String> = None;
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        let Some(media) = message.media() else {
            continue;
        };
        let Media::Document(document) = &media else {
            continue;
        };
        if stack_manifest_id(document.name(), message.text()).as_deref() != Some(stack_id) {
            continue;
        }
        match download_stack_manifest(client, &media, Some(stack_id), Some(logical_channel_id))
            .await
        {
            Ok(manifest) => return Ok(manifest),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        "Version stack was not found. Refresh the channel and retry.".to_string()
    }))
}

/// Read the newest valid manifest for every stack in a channel. Telegram history
/// is newest-first, so a successfully parsed ID claims that stack and older
/// stale metadata copies are ignored.
pub async fn load_current_stack_manifests(
    client: &Client,
    peer: &Peer,
    logical_channel_id: &str,
) -> Result<Vec<FileStackManifestV1>, String> {
    let mut seen = HashSet::new();
    let mut manifests = Vec::new();
    let mut messages = client.iter_messages(peer);
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        let Some(media) = message.media() else {
            continue;
        };
        let Media::Document(document) = &media else {
            continue;
        };
        let Some(stack_id) = stack_manifest_id(document.name(), message.text()) else {
            continue;
        };
        if seen.contains(&stack_id) {
            continue;
        }
        match download_stack_manifest(client, &media, Some(&stack_id), Some(logical_channel_id))
            .await
        {
            Ok(manifest) => {
                seen.insert(stack_id);
                manifests.push(manifest);
            }
            Err(error) => {
                log::warn!(
                    "Ignoring invalid TeraRelay stack metadata {}: {}",
                    stack_id,
                    error
                );
            }
        }
    }
    Ok(manifests)
}

async fn publish_stack_manifest(
    client: &Client,
    app: &AppHandle,
    peer: &Peer,
    manifest: &FileStackManifestV1,
) -> Result<(), String> {
    validate_stack_manifest(
        manifest,
        Some(&manifest.stack_id),
        Some(&manifest.logical_channel_id),
    )?;
    let old_ids = find_stack_document_ids(client, peer, &manifest.stack_id).await?;
    let new_id = upload_stack_manifest_document(client, app, manifest, peer).await? as i32;
    let stale: Vec<i32> = old_ids.into_iter().filter(|id| *id != new_id).collect();
    if !stale.is_empty() {
        if let Err(error) = client.delete_messages(peer, &stale).await {
            // Newest metadata is authoritative. An old copy can remain without
            // changing behavior because listing always accepts the newest valid copy.
            log::warn!(
                "Published version stack {} but could not remove {} stale metadata document(s): {}",
                manifest.stack_id,
                stale.len(),
                error
            );
        }
    }
    Ok(())
}

async fn delete_stack_documents(
    client: &Client,
    peer: &Peer,
    stack_id: &str,
) -> Result<(), String> {
    let ids = find_stack_document_ids(client, peer, stack_id).await?;
    if !ids.is_empty() {
        client
            .delete_messages(peer, &ids)
            .await
            .map_err(|e| format!("Failed to remove version-stack metadata: {e}"))?;
    }
    Ok(())
}

fn logical_scope_for_folder(conn: &sqlite::Connection, folder_id: i64) -> Result<String, String> {
    crate::commands::logical_channels::logical_channel_id_for_backing(conn, folder_id)?.ok_or_else(
        || {
            "File Versions are available in TeraRelay channels. Sync the channel and retry."
                .to_string()
        },
    )
}

fn file_metadata_by_id(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    file_id: &str,
    folder_id: i64,
) -> Result<Option<FileMetadata>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT first_message_id, display_name, total_size, mime_type, chunk_count, created_at
             FROM logical_files
             WHERE logical_channel_id = ? AND file_id = ? AND status = 'complete'",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, logical_channel_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, file_id))
        .map_err(|e: sqlite::Error| e.to_string())?;

    if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        let name = stmt
            .read::<String, _>("display_name")
            .map_err(|e| e.to_string())?;
        let created_at = stmt
            .read::<i64, _>("created_at")
            .map_err(|e| e.to_string())?;
        let created = chrono::DateTime::<chrono::Utc>::from_timestamp(created_at, 0)
            .map(|v| v.to_rfc3339())
            .unwrap_or_else(|| created_at.to_string());
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_string());

        return Ok(Some(FileMetadata {
            id: stmt
                .read::<i64, _>("first_message_id")
                .map_err(|e| e.to_string())?,
            folder_id: Some(folder_id),
            name,
            size: stmt
                .read::<i64, _>("total_size")
                .map_err(|e| e.to_string())? as u64,
            mime_type: stmt.read::<Option<String>, _>("mime_type").ok().flatten(),
            file_ext: ext,
            created_at: created,
            icon_type: "file".to_string(),
            is_split: stmt
                .read::<i64, _>("chunk_count")
                .map_err(|e| e.to_string())?
                > 1,
            logical_file_id: Some(file_id.to_string()),
            stack_id: None,
            stack_name: None,
            stack_version_count: 0,
            stack_label: None,
        }));
    }
    Ok(None)
}

fn file_message_ids(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    file_ids: &[String],
) -> Result<Vec<i32>, String> {
    let mut ids = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        let mut stmt = conn
            .prepare(
                "SELECT first_message_id FROM logical_files
                 WHERE logical_channel_id = ? AND file_id = ? AND status = 'complete'",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, logical_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            let id = stmt
                .read::<i64, _>("first_message_id")
                .map_err(|e| e.to_string())?;
            ids.push(
                i32::try_from(id)
                    .map_err(|_| "Stored Telegram message ID is out of range".to_string())?,
            );
        } else {
            return Err(format!(
                "A stack member ({file_id}) is no longer available in this channel"
            ));
        }
    }
    Ok(ids)
}

fn ensure_files_exist(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    folder_id: i64,
    file_ids: &[&str],
) -> Result<Vec<FileMetadata>, String> {
    let mut files = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        files.push(
            file_metadata_by_id(conn, logical_channel_id, file_id, folder_id)?.ok_or_else(
                || {
                    "One of the selected files is no longer available. Refresh and retry."
                        .to_string()
                },
            )?,
        );
    }
    Ok(files)
}

fn member_label<'a>(manifest: &'a FileStackManifestV1, file_id: &str) -> Option<&'a str> {
    manifest
        .members
        .iter()
        .find(|m| m.file_id == file_id)
        .and_then(|m| m.label.as_deref())
}

/// Collapse current logical files into stable stack rows. The anchor controls
/// where the stack appears in the chronological feed; changing the primary
/// version never changes that position.
pub fn collapse_files(
    files: Vec<FileMetadata>,
    manifests: &[FileStackManifestV1],
) -> Vec<FileMetadata> {
    let by_file_id: HashMap<String, FileMetadata> = files
        .iter()
        .filter_map(|file| {
            file.logical_file_id
                .as_ref()
                .map(|id| (id.clone(), file.clone()))
        })
        .collect();

    #[derive(Clone)]
    struct ActiveStack {
        manifest: FileStackManifestV1,
        member_ids: HashSet<String>,
        primary_id: String,
    }

    let mut claimed = HashSet::<String>::new();
    let mut anchor_map = HashMap::<String, ActiveStack>::new();

    for manifest in manifests {
        let available: Vec<String> = manifest
            .members
            .iter()
            .filter(|member| by_file_id.contains_key(&member.file_id))
            .map(|member| member.file_id.clone())
            .collect();
        if available.len() < 2 {
            continue;
        }
        if available.iter().any(|id| claimed.contains(id)) {
            // The newest manifest wins if corrupt/stale metadata associates the
            // same file with two stacks.
            continue;
        }

        let primary_id = if available.contains(&manifest.primary_file_id) {
            manifest.primary_file_id.clone()
        } else {
            available[0].clone()
        };
        let anchor_id = if available.contains(&manifest.anchor_file_id) {
            manifest.anchor_file_id.clone()
        } else {
            primary_id.clone()
        };
        let member_ids: HashSet<String> = available.into_iter().collect();
        claimed.extend(member_ids.iter().cloned());
        anchor_map.insert(
            anchor_id.clone(),
            ActiveStack {
                manifest: manifest.clone(),
                member_ids,
                primary_id,
            },
        );
    }

    let mut result = Vec::with_capacity(files.len());
    for file in files {
        let Some(file_id) = file.logical_file_id.as_ref() else {
            result.push(file);
            continue;
        };

        if let Some(stack) = anchor_map.get(file_id) {
            if let Some(mut primary) = by_file_id.get(&stack.primary_id).cloned() {
                // Preserve stack position/date from its anchor while keeping the
                // primary file's real name/size/id for preview and download.
                primary.created_at = file.created_at.clone();
                primary.stack_id = Some(stack.manifest.stack_id.clone());
                primary.stack_name = Some(stack.manifest.display_name.clone());
                primary.stack_version_count = stack.member_ids.len() as u32;
                primary.stack_label =
                    member_label(&stack.manifest, &stack.primary_id).map(str::to_string);
                result.push(primary);
            }
            continue;
        }

        if claimed.contains(file_id) {
            continue;
        }
        result.push(file);
    }
    result
}

async fn current_context(
    folder_id: i64,
    state: &TelegramState,
    db_pool: &DbConnection,
) -> Result<(Client, Peer, String), String> {
    let logical_channel_id = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        logical_scope_for_folder(&conn, folder_id)?
    };
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;
    let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
    Ok((client, peer, logical_channel_id))
}

fn stack_available_member_count(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    manifest: &FileStackManifestV1,
) -> Result<usize, String> {
    let mut count = 0usize;
    for member in &manifest.members {
        let mut stmt = conn
            .prepare(
                "SELECT 1 FROM logical_files
                 WHERE logical_channel_id = ? AND file_id = ? AND status = 'complete'
                 LIMIT 1",
            )
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((1, logical_channel_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        stmt.bind((2, member.file_id.as_str()))
            .map_err(|e: sqlite::Error| e.to_string())?;
        if let sqlite::State::Row = stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
            count += 1;
        }
    }
    Ok(count)
}

fn ensure_not_in_any_active_stack(
    conn: &sqlite::Connection,
    logical_channel_id: &str,
    manifests: &[FileStackManifestV1],
    file_id: &str,
    except_stack_id: Option<&str>,
) -> Result<(), String> {
    for stack in manifests {
        if Some(stack.stack_id.as_str()) == except_stack_id
            || !stack.members.iter().any(|member| member.file_id == file_id)
        {
            continue;
        }

        // A stale metadata document whose stack has fewer than two surviving
        // files no longer represents an active stack. Do not permanently trap
        // its surviving file; the user may safely place that file in a new stack.
        if stack_available_member_count(conn, logical_channel_id, stack)? >= 2 {
            return Err("This file already belongs to another version stack".to_string());
        }
    }
    Ok(())
}

fn qa_stack_view(
    db_pool: &DbConnection,
    folder_id: i64,
    stack_id: &str,
) -> Result<FileStackView, String> {
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
    let manifest =
        crate::commands::qa_feature_a::load_manifest(&conn, stack_id, &logical_channel_id)?;
    let mut members = Vec::new();
    for member in &manifest.members {
        let Some(mut file) =
            file_metadata_by_id(&conn, &logical_channel_id, &member.file_id, folder_id)?
        else {
            continue;
        };
        file.stack_id = Some(manifest.stack_id.clone());
        file.stack_name = Some(manifest.display_name.clone());
        file.stack_label = member.label.clone();
        members.push(FileStackMemberView {
            file,
            label: member.label.clone(),
            is_primary: member.file_id == manifest.primary_file_id,
            is_anchor: member.file_id == manifest.anchor_file_id,
            added_at: member.added_at,
        });
    }
    Ok(FileStackView {
        stack_id: manifest.stack_id,
        display_name: manifest.display_name,
        primary_file_id: manifest.primary_file_id,
        anchor_file_id: manifest.anchor_file_id,
        created_at: manifest.created_at,
        updated_at: manifest.updated_at,
        members,
    })
}

#[tauri::command]
pub async fn cmd_get_file_stack(
    stack_id: String,
    folder_id: i64,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<FileStackView, String> {
    if crate::commands::qa_feature_a::enabled() {
        return qa_stack_view(db_pool.inner(), folder_id, &stack_id);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    let manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;

    let mut members = Vec::new();
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        for member in &manifest.members {
            let Some(mut file) =
                file_metadata_by_id(&conn, &logical_channel_id, &member.file_id, folder_id)?
            else {
                continue;
            };
            file.stack_id = Some(manifest.stack_id.clone());
            file.stack_name = Some(manifest.display_name.clone());
            file.stack_label = member.label.clone();
            members.push(FileStackMemberView {
                file,
                label: member.label.clone(),
                is_primary: member.file_id == manifest.primary_file_id,
                is_anchor: member.file_id == manifest.anchor_file_id,
                added_at: member.added_at,
            });
        }
    }

    Ok(FileStackView {
        stack_id: manifest.stack_id,
        display_name: manifest.display_name,
        primary_file_id: manifest.primary_file_id,
        anchor_file_id: manifest.anchor_file_id,
        created_at: manifest.created_at,
        updated_at: manifest.updated_at,
        members,
    })
}

#[tauri::command]
pub async fn cmd_create_file_stack(
    folder_id: i64,
    base_file_id: String,
    version_file_id: String,
    primary_file_id: String,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<String, String> {
    if base_file_id == version_file_id {
        return Err("Choose a different file as the new version".to_string());
    }
    if primary_file_id != base_file_id && primary_file_id != version_file_id {
        return Err("Primary version must be one of the selected files".to_string());
    }

    let logical_channel_id = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        ensure_files_exist(
            &conn,
            &logical_channel_id,
            folder_id,
            &[&base_file_id, &version_file_id],
        )?;
        logical_channel_id
    };

    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let existing = crate::commands::qa_feature_a::load_manifests(&conn, &logical_channel_id)?;
        ensure_not_in_any_active_stack(&conn, &logical_channel_id, &existing, &base_file_id, None)?;
        ensure_not_in_any_active_stack(
            &conn,
            &logical_channel_id,
            &existing,
            &version_file_id,
            None,
        )?;
        let display_name =
            file_metadata_by_id(&conn, &logical_channel_id, &base_file_id, folder_id)?
                .ok_or_else(|| "Base file is no longer available".to_string())?
                .name;
        let now = unix_now();
        let manifest = FileStackManifestV1 {
            schema_version: STACK_SCHEMA_VERSION,
            stack_id: new_stack_id(),
            logical_channel_id,
            display_name,
            primary_file_id,
            anchor_file_id: base_file_id.clone(),
            created_at: now,
            updated_at: now,
            members: vec![
                FileStackMemberV1 {
                    file_id: base_file_id,
                    label: None,
                    added_at: now,
                },
                FileStackMemberV1 {
                    file_id: version_file_id,
                    label: None,
                    added_at: now,
                },
            ],
        };
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(manifest.stack_id);
    }

    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;
    let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
    let existing = load_current_stack_manifests(&client, &peer, &logical_channel_id).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        ensure_not_in_any_active_stack(&conn, &logical_channel_id, &existing, &base_file_id, None)?;
        ensure_not_in_any_active_stack(
            &conn,
            &logical_channel_id,
            &existing,
            &version_file_id,
            None,
        )?;
    }

    let display_name = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        file_metadata_by_id(&conn, &logical_channel_id, &base_file_id, folder_id)?
            .ok_or_else(|| "Base file is no longer available".to_string())?
            .name
    };
    let now = unix_now();
    let manifest = FileStackManifestV1 {
        schema_version: STACK_SCHEMA_VERSION,
        stack_id: new_stack_id(),
        logical_channel_id,
        display_name,
        primary_file_id,
        anchor_file_id: base_file_id.clone(),
        created_at: now,
        updated_at: now,
        members: vec![
            FileStackMemberV1 {
                file_id: base_file_id,
                label: None,
                added_at: now,
            },
            FileStackMemberV1 {
                file_id: version_file_id,
                label: None,
                added_at: now,
            },
        ],
    };
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(manifest.stack_id)
}

#[tauri::command]
pub async fn cmd_add_file_to_stack(
    folder_id: i64,
    stack_id: String,
    file_id: String,
    make_primary: bool,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let logical_channel_id = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
        let scope = logical_scope_for_folder(&conn, folder_id)?;
        ensure_files_exist(&conn, &scope, folder_id, &[&file_id])?;
        scope
    };

    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let all = crate::commands::qa_feature_a::load_manifests(&conn, &logical_channel_id)?;
        ensure_not_in_any_active_stack(
            &conn,
            &logical_channel_id,
            &all,
            &file_id,
            Some(&stack_id),
        )?;
        let mut manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        if !manifest
            .members
            .iter()
            .any(|member| member.file_id == file_id)
        {
            if manifest.members.len() >= MAX_STACK_MEMBERS {
                return Err(format!(
                    "A version stack can contain at most {MAX_STACK_MEMBERS} files"
                ));
            }
            manifest.members.push(FileStackMemberV1 {
                file_id: file_id.clone(),
                label: None,
                added_at: unix_now(),
            });
        }
        if make_primary {
            manifest.primary_file_id = file_id;
        }
        manifest.updated_at = unix_now();
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;
    let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
    let all = load_current_stack_manifests(&client, &peer, &logical_channel_id).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        ensure_not_in_any_active_stack(
            &conn,
            &logical_channel_id,
            &all,
            &file_id,
            Some(&stack_id),
        )?;
    }

    let mut manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    if !manifest
        .members
        .iter()
        .any(|member| member.file_id == file_id)
    {
        if manifest.members.len() >= MAX_STACK_MEMBERS {
            return Err(format!(
                "A version stack can contain at most {MAX_STACK_MEMBERS} files"
            ));
        }
        manifest.members.push(FileStackMemberV1 {
            file_id: file_id.clone(),
            label: None,
            added_at: unix_now(),
        });
    }
    if make_primary {
        manifest.primary_file_id = file_id;
    }
    manifest.updated_at = unix_now();
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_set_file_stack_primary(
    folder_id: i64,
    stack_id: String,
    file_id: String,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        let mut manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        if !manifest
            .members
            .iter()
            .any(|member| member.file_id == file_id)
        {
            return Err("Selected file is not a member of this version stack".to_string());
        }
        manifest.primary_file_id = file_id;
        manifest.updated_at = unix_now();
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    let mut manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    if !manifest
        .members
        .iter()
        .any(|member| member.file_id == file_id)
    {
        return Err("Selected file is not a member of this version stack".to_string());
    }
    manifest.primary_file_id = file_id;
    manifest.updated_at = unix_now();
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_rename_file_stack(
    folder_id: i64,
    stack_id: String,
    display_name: String,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let name = display_name.trim();
    if name.is_empty() {
        return Err("Stack name cannot be empty".to_string());
    }
    if name.chars().count() > MAX_STACK_NAME_CHARS {
        return Err("Stack name is too long".to_string());
    }
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        let mut manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        manifest.display_name = name.to_string();
        manifest.updated_at = unix_now();
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    let mut manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    manifest.display_name = name.to_string();
    manifest.updated_at = unix_now();
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_update_file_stack_label(
    folder_id: i64,
    stack_id: String,
    file_id: String,
    label: Option<String>,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let cleaned = label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if cleaned
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_STACK_LABEL_CHARS)
    {
        return Err(format!(
            "Version labels can contain at most {MAX_STACK_LABEL_CHARS} characters"
        ));
    }
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        let mut manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        let member = manifest
            .members
            .iter_mut()
            .find(|member| member.file_id == file_id)
            .ok_or_else(|| "Selected file is not a member of this version stack".to_string())?;
        member.label = cleaned;
        manifest.updated_at = unix_now();
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    let mut manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    let member = manifest
        .members
        .iter_mut()
        .find(|member| member.file_id == file_id)
        .ok_or_else(|| "Selected file is not a member of this version stack".to_string())?;
    member.label = cleaned;
    manifest.updated_at = unix_now();
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_remove_file_from_stack(
    folder_id: i64,
    stack_id: String,
    file_id: String,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        let mut manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        let before = manifest.members.len();
        manifest.members.retain(|member| member.file_id != file_id);
        if manifest.members.len() == before {
            return Err("Selected file is not a member of this version stack".to_string());
        }
        if manifest.members.len() < 2 {
            crate::commands::qa_feature_a::delete_stack(&conn, &stack_id)?;
            return Ok(true);
        }
        if manifest.primary_file_id == file_id {
            manifest.primary_file_id = if manifest
                .members
                .iter()
                .any(|member| member.file_id == manifest.anchor_file_id)
            {
                manifest.anchor_file_id.clone()
            } else {
                manifest.members[0].file_id.clone()
            };
        }
        if manifest.anchor_file_id == file_id {
            manifest.anchor_file_id = manifest.primary_file_id.clone();
        }
        manifest.updated_at = unix_now();
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    let mut manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    let before = manifest.members.len();
    manifest.members.retain(|member| member.file_id != file_id);
    if manifest.members.len() == before {
        return Err("Selected file is not a member of this version stack".to_string());
    }
    if manifest.members.len() < 2 {
        delete_stack_documents(&client, &peer, &stack_id).await?;
        return Ok(true);
    }
    if manifest.primary_file_id == file_id {
        manifest.primary_file_id = if manifest
            .members
            .iter()
            .any(|member| member.file_id == manifest.anchor_file_id)
        {
            manifest.anchor_file_id.clone()
        } else {
            manifest.members[0].file_id.clone()
        };
    }
    if manifest.anchor_file_id == file_id {
        manifest.anchor_file_id = manifest.primary_file_id.clone();
    }
    manifest.updated_at = unix_now();
    publish_stack_manifest(&client, &app_handle, &peer, &manifest).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_unstack_file_stack(
    folder_id: i64,
    stack_id: String,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::qa_feature_a::delete_stack(&conn, &stack_id)?;
        return Ok(true);
    }

    let (client, peer, _logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    delete_stack_documents(&client, &peer, &stack_id).await?;
    Ok(true)
}

#[tauri::command]
pub async fn cmd_delete_file_stack_all(
    folder_id: i64,
    stack_id: String,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<usize, String> {
    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let logical_channel_id = logical_scope_for_folder(&conn, folder_id)?;
        let manifest =
            crate::commands::qa_feature_a::load_manifest(&conn, &stack_id, &logical_channel_id)?;
        crate::commands::qa_feature_a::delete_stack(&conn, &stack_id)?;
        return crate::commands::qa_feature_a::delete_manifest_files(&conn, &manifest);
    }

    let (client, peer, logical_channel_id) =
        current_context(folder_id, state.inner(), db_pool.inner()).await?;
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, Some(folder_id))?;
    }
    let manifest =
        load_current_stack_manifest(&client, &peer, &stack_id, &logical_channel_id).await?;
    let file_ids: Vec<String> = manifest
        .members
        .iter()
        .map(|member| member.file_id.clone())
        .collect();
    let message_ids = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        file_message_ids(&conn, &logical_channel_id, &file_ids)?
    };

    // Remove grouping metadata first. If a later physical deletion fails, the
    // surviving files remain ordinary visible files rather than a broken stack.
    delete_stack_documents(&client, &peer, &stack_id).await?;

    let mut deleted = 0usize;
    let mut failures = Vec::new();
    for message_id in message_ids {
        match crate::commands::fs::delete_file_inner(
            message_id,
            Some(folder_id),
            state.inner(),
            db_pool.inner(),
        )
        .await
        {
            Ok(_) => deleted += 1,
            Err(error) => failures.push(error),
        }
    }

    if failures.is_empty() {
        Ok(deleted)
    } else {
        Err(format!(
            "Deleted {deleted} version(s), but {} version(s) could not be deleted. Remaining files were safely unstacked. {}",
            failures.len(),
            failures.join(" | ")
        ))
    }
}

#[tauri::command]
pub async fn cmd_move_file_stack(
    stack_id: String,
    source_folder_id: i64,
    target_folder_id: i64,
    app_handle: AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    if source_folder_id == target_folder_id {
        return Ok(true);
    }

    if crate::commands::qa_feature_a::enabled() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let source_logical_channel_id = logical_scope_for_folder(&conn, source_folder_id)?;
        let mut manifest = crate::commands::qa_feature_a::load_manifest(
            &conn,
            &stack_id,
            &source_logical_channel_id,
        )?;
        crate::commands::qa_feature_a::move_stack_members(
            &conn,
            source_folder_id,
            target_folder_id,
            &mut manifest,
        )?;
        crate::commands::qa_feature_a::publish_manifest(&conn, &manifest)?;
        return Ok(true);
    }

    let source_logical_channel_id = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(
            &conn,
            Some(source_folder_id),
        )?;
        logical_scope_for_folder(&conn, source_folder_id)?
    };
    let target_logical_channel_id = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(
            &conn,
            Some(target_folder_id),
        )?;
        logical_scope_for_folder(&conn, target_folder_id)?
    };
    let client = state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Client not connected".to_string())?;
    let source_peer = resolve_peer(&client, Some(source_folder_id), &state.peer_cache).await?;
    let mut manifest =
        load_current_stack_manifest(&client, &source_peer, &stack_id, &source_logical_channel_id)
            .await?;
    let file_ids: Vec<String> = manifest
        .members
        .iter()
        .map(|member| member.file_id.clone())
        .collect();
    let source_message_ids = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        file_message_ids(&conn, &source_logical_channel_id, &file_ids)?
    };

    crate::commands::fs::move_files_inner(
        source_message_ids,
        Some(source_folder_id),
        Some(target_folder_id),
        app_handle.clone(),
        state.inner(),
        db_pool.inner(),
    )
    .await?;

    let target_peer = resolve_peer(&client, Some(target_folder_id), &state.peer_cache).await?;
    manifest.logical_channel_id = target_logical_channel_id.clone();
    manifest.updated_at = unix_now();

    if let Err(publish_error) =
        publish_stack_manifest(&client, &app_handle, &target_peer, &manifest).await
    {
        // The physical files already moved. Their stable logical file IDs let us
        // resolve the target message IDs and attempt a full rollback.
        let target_ids = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            file_message_ids(&conn, &target_logical_channel_id, &file_ids)
        };
        match target_ids {
            Ok(target_ids) => {
                if let Err(rollback_error) = crate::commands::fs::move_files_inner(
                    target_ids,
                    Some(target_folder_id),
                    Some(source_folder_id),
                    app_handle.clone(),
                    state.inner(),
                    db_pool.inner(),
                )
                .await
                {
                    return Err(format!(
                        "Files moved but stack metadata could not be published ({publish_error}); automatic rollback also failed ({rollback_error}). Refresh both channels before making further changes."
                    ));
                }
            }
            Err(rollback_error) => {
                return Err(format!(
                    "Files moved but stack metadata could not be published ({publish_error}); rollback could not resolve the moved files ({rollback_error}). Refresh both channels before making further changes."
                ));
            }
        }
        return Err(format!(
            "Stack move was rolled back because its metadata could not be published: {publish_error}"
        ));
    }

    if let Err(error) = delete_stack_documents(&client, &source_peer, &stack_id).await {
        log::warn!(
            "Stack {} moved successfully but stale source metadata could not be removed: {}",
            stack_id,
            error
        );
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> FileStackManifestV1 {
        FileStackManifestV1 {
            schema_version: STACK_SCHEMA_VERSION,
            stack_id: "a".repeat(32),
            logical_channel_id: "channel".to_string(),
            display_name: "Movie".to_string(),
            primary_file_id: "1".repeat(32),
            anchor_file_id: "1".repeat(32),
            created_at: 1,
            updated_at: 2,
            members: vec![
                FileStackMemberV1 {
                    file_id: "1".repeat(32),
                    label: Some("1080p".to_string()),
                    added_at: 1,
                },
                FileStackMemberV1 {
                    file_id: "2".repeat(32),
                    label: Some("4K".to_string()),
                    added_at: 2,
                },
            ],
        }
    }

    fn metadata(file_id: &str, id: i64, created: &str) -> FileMetadata {
        FileMetadata {
            id,
            folder_id: Some(7),
            name: format!("{file_id}.mkv"),
            size: id as u64,
            mime_type: Some("video/x-matroska".to_string()),
            file_ext: Some("mkv".to_string()),
            created_at: created.to_string(),
            icon_type: "file".to_string(),
            is_split: false,
            logical_file_id: Some(file_id.to_string()),
            stack_id: None,
            stack_name: None,
            stack_version_count: 0,
            stack_label: None,
        }
    }

    #[test]
    fn stack_manifest_round_trips() {
        let manifest = sample_manifest();
        let encoded = encode_stack_manifest(&manifest).unwrap();
        let decoded =
            decode_stack_manifest(&encoded, Some(&manifest.stack_id), Some("channel")).unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn stack_manifest_requires_two_unique_members() {
        let mut manifest = sample_manifest();
        manifest.members.pop();
        assert!(validate_stack_manifest(&manifest, None, None).is_err());

        let mut duplicate = sample_manifest();
        duplicate.members[1].file_id = duplicate.members[0].file_id.clone();
        assert!(validate_stack_manifest(&duplicate, None, None).is_err());
    }

    #[test]
    fn collapse_uses_primary_but_preserves_anchor_position_and_date() {
        let one = "1".repeat(32);
        let two = "2".repeat(32);
        let files = vec![
            metadata(&two, 20, "2026-10-06T12:00:00Z"),
            metadata(&one, 10, "2026-10-01T12:00:00Z"),
        ];
        let mut manifest = sample_manifest();
        manifest.primary_file_id = two.clone();
        manifest.anchor_file_id = one.clone();

        let collapsed = collapse_files(files, &[manifest]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].logical_file_id.as_deref(), Some(two.as_str()));
        assert_eq!(collapsed[0].created_at, "2026-10-01T12:00:00Z");
        assert_eq!(collapsed[0].stack_version_count, 2);
        assert_eq!(collapsed[0].stack_label.as_deref(), Some("4K"));
    }

    #[test]
    fn metadata_filename_and_caption_identify_same_stack() {
        let id = "b".repeat(32);
        assert_eq!(
            stack_manifest_id(&stack_file_name(&id), &stack_caption(&id)).as_deref(),
            Some(id.as_str())
        );
    }
}
