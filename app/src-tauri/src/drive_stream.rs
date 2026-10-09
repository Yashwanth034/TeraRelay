use crate::commands::drive_metadata::{
    assign_drive_pending_backing_channel, drive_pending_by_id, drive_pending_chunks,
    drive_pending_parts, finalize_drive_streamed_pending, finalize_drive_v2_metadata,
    find_drive_content_match, upsert_drive_pending_chunk, upsert_drive_pending_part,
    DrivePendingChunkRecord, DrivePendingPartRecord, DrivePendingRecord,
};
use crate::commands::logical_files::{LogicalFileManifestV1, ManifestChunkV1};
use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::db::DbConnection;
use grammers_client::InputMessage;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncRead, ReadBuf};

pub const DRIVE_DATA_FILE_PREFIX: &str = ".terarelay-drive-data-";
pub const DRIVE_DATA_CAPTION_PREFIX: &str = "TRD2:data:";
pub const DRIVE_BACKING_FILE_PREFIX: &str = ".terarelay-drive-file-";
pub const DRIVE_ENCRYPTED_CHUNK_OVERHEAD: u64 = 5 + 24 + 16;
/// 15 × 16 MiB = 240 MiB plaintext. Even with per-block authenticated
/// encryption overhead the remote scratch object stays below the mount's
/// existing 256 MiB staging budget, while reducing Telegram message count 15×.
pub const DRIVE_REMOTE_PART_MAX_BLOCKS: u64 = 15;

pub fn remote_part_index_for_chunk(chunk_index: u64) -> u64 {
    chunk_index / DRIVE_REMOTE_PART_MAX_BLOCKS
}

pub fn remote_part_count(total_chunks: u64) -> u64 {
    if total_chunks == 0 {
        0
    } else {
        total_chunks.div_ceil(DRIVE_REMOTE_PART_MAX_BLOCKS)
    }
}

pub fn remote_part_chunk_range(part_index: u64, total_chunks: u64) -> Option<(u64, u64)> {
    let first = part_index.checked_mul(DRIVE_REMOTE_PART_MAX_BLOCKS)?;
    if first >= total_chunks {
        return None;
    }
    Some((
        first,
        total_chunks.min(first.saturating_add(DRIVE_REMOTE_PART_MAX_BLOCKS)),
    ))
}

/// Return remote parts that are safe to publish from the current logical size.
/// While a file is open we publish only complete 15-block parts so later writes
/// never require rewriting a partial part. Closing makes the final tail part
/// publishable, including the empty-file sentinel block.
pub fn ready_remote_parts(logical_size: u64, closed: bool) -> Vec<(u64, u64, u64)> {
    let block = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
    let full_blocks = logical_size / block;
    let total_blocks = if logical_size == 0 {
        if closed {
            1
        } else {
            0
        }
    } else {
        logical_size.div_ceil(block)
    };
    let ready_blocks = if closed {
        total_blocks
    } else {
        (full_blocks / DRIVE_REMOTE_PART_MAX_BLOCKS) * DRIVE_REMOTE_PART_MAX_BLOCKS
    };
    if ready_blocks == 0 {
        return Vec::new();
    }
    let part_count = remote_part_count(ready_blocks);
    (0..part_count)
        .filter_map(|part_index| {
            remote_part_chunk_range(part_index, ready_blocks)
                .map(|(first, end)| (part_index, first, end))
        })
        .collect()
}

struct EncodedPartReader<'a> {
    blocks: &'a [Vec<u8>],
    block_index: usize,
    offset: usize,
}

impl<'a> EncodedPartReader<'a> {
    fn new(blocks: &'a [Vec<u8>]) -> Self {
        Self {
            blocks,
            block_index: 0,
            offset: 0,
        }
    }
}

impl AsyncRead for EncodedPartReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        while buf.remaining() > 0 && self.block_index < self.blocks.len() {
            let index = self.block_index;
            let offset = self.offset;
            if offset >= self.blocks[index].len() {
                self.block_index += 1;
                self.offset = 0;
                continue;
            }
            let take = buf
                .remaining()
                .min(self.blocks[index].len().saturating_sub(offset));
            buf.put_slice(&self.blocks[index][offset..offset + take]);
            self.offset += take;
        }
        Poll::Ready(Ok(()))
    }
}

pub fn is_drive_data_document(file_name: &str, caption: &str) -> bool {
    file_name.starts_with(DRIVE_DATA_FILE_PREFIX) || caption.starts_with(DRIVE_DATA_CAPTION_PREFIX)
}

pub fn is_drive_backing_logical_name(name: &str) -> bool {
    name.starts_with(DRIVE_BACKING_FILE_PREFIX)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_sha256(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("Drive plaintext chunk hash has an invalid length".to_string());
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| "Drive plaintext chunk hash is invalid".to_string())?;
    }
    Ok(out)
}

fn staging_dir(pending: &DrivePendingRecord) -> PathBuf {
    Path::new(&pending.staging_path)
        .parent()
        .unwrap_or_else(|| Path::new(&pending.staging_path))
        .to_path_buf()
}

pub fn local_chunk_path(pending: &DrivePendingRecord, chunk_index: u64) -> PathBuf {
    staging_dir(pending).join(format!("plain-{chunk_index:016x}.part"))
}

#[derive(Debug)]
pub struct EncodedDriveBlock {
    pub chunk_index: u64,
    pub encoded: Vec<u8>,
    pub encoded_sha256: String,
    pub plaintext_sha256: String,
}

pub fn encode_pending_plaintext_block(
    app: &AppHandle,
    db: &DbConnection,
    pending: &DrivePendingRecord,
    chunk_index: u64,
    plaintext: Vec<u8>,
) -> Result<EncodedDriveBlock, String> {
    let plaintext_sha256 = hex_sha256(&plaintext);
    let encoded = if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
        let key = {
            let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::drive_crypto::load_verified_master_key_for_db(app, &conn)?
        }
        .ok_or_else(|| {
            "TeraRelay Drive is locked. Unlock Drive encryption before writing files.".to_string()
        })?;
        let crypto_id = pending
            .crypto_id
            .as_deref()
            .ok_or_else(|| "Encrypted Drive write is missing its crypto identity".to_string())?;
        crate::drive_crypto::encrypt_chunk(&*key, crypto_id, chunk_index, &plaintext)?
    } else if pending.encryption_version == 0 {
        if plaintext.is_empty() {
            vec![0u8]
        } else {
            plaintext
        }
    } else {
        return Err(format!(
            "Unsupported TeraRelay Drive encryption version {}",
            pending.encryption_version
        ));
    };
    let encoded_sha256 = hex_sha256(&encoded);
    Ok(EncodedDriveBlock {
        chunk_index,
        encoded,
        encoded_sha256,
        plaintext_sha256,
    })
}

async fn client_for_state(state: &TelegramState) -> Result<grammers_client::Client, String> {
    state
        .client
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Telegram client is not connected".to_string())
}

async fn delete_messages_best_effort(state: &TelegramState, backing_channel_id: i64, ids: &[i32]) {
    if ids.is_empty() || backing_channel_id <= 0 {
        return;
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let Ok(client) = client_for_state(state).await else {
        return;
    };
    let Ok(peer) = resolve_peer(&client, Some(backing_channel_id), &state.peer_cache).await else {
        return;
    };
    if let Err(error) = client.delete_messages(&peer, &ids).await {
        log::warn!(
            "Could not clean {} superseded TeraRelay Drive object(s): {}",
            ids.len(),
            error
        );
    }
}

async fn ensure_pending_storage(
    app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
) -> Result<DrivePendingRecord, String> {
    let mut pending = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_by_id(&conn, pending_id)?
            .ok_or_else(|| "Drive pending write no longer exists".to_string())?
    };
    if pending.backing_channel_id != 0 {
        return Ok(pending);
    }
    let storage = app.state::<crate::drive_storage::DriveStorageState>();
    let channel =
        crate::drive_storage::ensure_active_storage_channel(app, state, db, storage.inner())
            .await?;
    pending = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        assign_drive_pending_backing_channel(&conn, pending_id, channel.backing_channel_id)?
    };
    Ok(pending)
}

pub async fn upload_plaintext_chunk(
    app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
    chunk_index: u64,
    plaintext: Vec<u8>,
) -> Result<DrivePendingChunkRecord, String> {
    let pending = ensure_pending_storage(app, state, db, pending_id).await?;

    let plaintext_sha256 = hex_sha256(&plaintext);
    let existing = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_chunks(&conn, pending_id)?
            .into_iter()
            .find(|chunk| chunk.chunk_index == chunk_index)
    };
    if let Some(existing) = existing.as_ref() {
        if existing.plaintext_sha256 == plaintext_sha256 {
            return Ok(existing.clone());
        }
    }

    let encoded = if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
        let key = {
            let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::drive_crypto::load_verified_master_key_for_db(app, &conn)?
        }
        .ok_or_else(|| {
            "TeraRelay Drive is locked. Unlock Drive encryption before writing files.".to_string()
        })?;
        let crypto_id = pending
            .crypto_id
            .as_deref()
            .ok_or_else(|| "Encrypted Drive write is missing its crypto identity".to_string())?;
        crate::drive_crypto::encrypt_chunk(&*key, crypto_id, chunk_index, &plaintext)?
    } else if pending.encryption_version == 0 {
        // Telegram/logical manifests need a non-empty backing document.
        // The Drive entry keeps plaintext_size=0, so this one-byte sentinel is
        // never exposed through FUSE for an empty user file.
        if plaintext.is_empty() {
            vec![0u8]
        } else {
            plaintext
        }
    } else {
        return Err(format!(
            "Unsupported TeraRelay Drive encryption version {}",
            pending.encryption_version
        ));
    };

    let encoded_sha256 = hex_sha256(&encoded);
    let temp_dir = staging_dir(&pending);
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|e| format!("Could not create Drive staging directory: {e}"))?;
    let temp_path = temp_dir.join(format!("remote-{chunk_index:016x}.upload"));
    tokio::fs::write(&temp_path, &encoded)
        .await
        .map_err(|e| format!("Could not stage Drive chunk for upload: {e}"))?;

    let client = client_for_state(state).await?;
    let peer = resolve_peer(&client, Some(pending.backing_channel_id), &state.peer_cache).await?;
    let hidden_name = format!(
        "{DRIVE_DATA_FILE_PREFIX}{}-{chunk_index:016x}.bin",
        pending.id
    );
    let caption = format!("{DRIVE_DATA_CAPTION_PREFIX}{}:{chunk_index}", pending.id);

    let result = async {
        let mut file = tokio::fs::File::open(&temp_path)
            .await
            .map_err(|e| format!("Could not open staged Drive chunk: {e}"))?;
        let uploaded = client
            .upload_stream(&mut file, encoded.len(), hidden_name)
            .await
            .map_err(map_error)?;
        let sent = client
            .send_message(&peer, InputMessage::new().text(caption).file(uploaded))
            .await
            .map_err(map_error)?;
        Ok::<i32, String>(sent.id())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    let message_id = result?;

    {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        upsert_drive_pending_chunk(
            &conn,
            pending_id,
            chunk_index,
            i64::from(message_id),
            encoded.len() as u64,
            &encoded_sha256,
            &plaintext_sha256,
        )?;
    }

    if let Some(old) = existing {
        if old.message_id != i64::from(message_id) {
            if let Ok(old_id) = i32::try_from(old.message_id) {
                delete_messages_best_effort(state, pending.backing_channel_id, &[old_id]).await;
            }
        }
    }

    Ok(DrivePendingChunkRecord {
        pending_id: pending_id.to_string(),
        chunk_index,
        message_id: i64::from(message_id),
        chunk_size: encoded.len() as u64,
        sha256: encoded_sha256,
        plaintext_sha256,
        uploaded_at: chrono::Utc::now().timestamp_millis(),
    })
}

async fn send_encoded_part_once(
    client: &grammers_client::Client,
    state: &TelegramState,
    backing_channel_id: i64,
    encoded_blocks: &[Vec<u8>],
    part_size: usize,
    hidden_name: &str,
    caption: &str,
) -> Result<i64, String> {
    let peer = resolve_peer(client, Some(backing_channel_id), &state.peer_cache).await?;
    let mut reader = EncodedPartReader::new(encoded_blocks);
    let uploaded = client
        .upload_stream(&mut reader, part_size, hidden_name.to_string())
        .await
        .map_err(map_error)?;
    let sent = client
        .send_message(
            &peer,
            InputMessage::new().text(caption.to_string()).file(uploaded),
        )
        .await
        .map_err(map_error)?;
    Ok(i64::from(sent.id()))
}

pub async fn upload_encoded_part(
    app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
    part_index: u64,
    blocks: Vec<EncodedDriveBlock>,
) -> Result<DrivePendingPartRecord, String> {
    if blocks.is_empty() || blocks.len() as u64 > DRIVE_REMOTE_PART_MAX_BLOCKS {
        return Err("Drive remote part has an invalid block count".to_string());
    }
    let expected_first = part_index
        .checked_mul(DRIVE_REMOTE_PART_MAX_BLOCKS)
        .ok_or_else(|| "Drive remote part index overflow".to_string())?;
    for (offset, block) in blocks.iter().enumerate() {
        let expected = expected_first
            .checked_add(offset as u64)
            .ok_or_else(|| "Drive remote block index overflow".to_string())?;
        if block.chunk_index != expected || block.encoded.is_empty() {
            return Err("Drive remote part blocks are not contiguous".to_string());
        }
    }

    let pending = ensure_pending_storage(app, state, db, pending_id).await?;
    // Resolve the active pool member for every new remote part. The pending
    // record keeps the first channel only for backward compatibility; if this
    // exact channel becomes permanently unusable, this part may roll over once
    // to the next TeraRelay-managed storage generation.
    let storage = app.state::<crate::drive_storage::DriveStorageState>();
    let active_channel =
        crate::drive_storage::ensure_active_storage_channel(app, state, db, storage.inner())
            .await?;
    let mut part_backing_channel_id = active_channel.backing_channel_id;
    let existing_part = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_parts(&conn, pending_id)?
            .into_iter()
            .find(|part| part.part_index == part_index)
    };

    let mut part_hasher = Sha256::new();
    let mut part_size = 0u64;
    let mut metadata = Vec::with_capacity(blocks.len());
    let mut encoded_blocks = Vec::with_capacity(blocks.len());
    for block in blocks {
        part_hasher.update(&block.encoded);
        part_size = part_size
            .checked_add(block.encoded.len() as u64)
            .ok_or_else(|| "Drive remote part size overflow".to_string())?;
        metadata.push((
            block.chunk_index,
            block.encoded.len() as u64,
            block.encoded_sha256,
            block.plaintext_sha256,
        ));
        encoded_blocks.push(block.encoded);
    }
    let part_size_usize = usize::try_from(part_size)
        .map_err(|_| "Drive remote part is too large for this platform".to_string())?;
    let part_sha256 = format!("{:x}", part_hasher.finalize());

    let client = client_for_state(state).await?;
    let hidden_name = format!(
        "{DRIVE_DATA_FILE_PREFIX}{}-part-{part_index:016x}.bin",
        pending.id
    );
    let caption = format!(
        "{DRIVE_DATA_CAPTION_PREFIX}{}:part:{part_index}",
        pending.id
    );
    let message_id = match send_encoded_part_once(
        &client,
        state,
        part_backing_channel_id,
        &encoded_blocks,
        part_size_usize,
        &hidden_name,
        &caption,
    )
    .await
    {
        Ok(message_id) => message_id,
        Err(first_error) if crate::drive_storage::should_rotate_after_error(&first_error) => {
            // Never create a channel from one suspicious peer error. Drop only
            // this cached peer identity, rescan Telegram dialogs, and retry the
            // same storage channel once. Rollover is allowed only if the fresh
            // attempt independently returns another allowlisted permanent error.
            state
                .peer_cache
                .write()
                .await
                .remove(&part_backing_channel_id);
            match send_encoded_part_once(
                &client,
                state,
                part_backing_channel_id,
                &encoded_blocks,
                part_size_usize,
                &hidden_name,
                &caption,
            )
            .await
            {
                Ok(message_id) => message_id,
                Err(second_error)
                    if crate::drive_storage::should_rotate_after_error(&second_error) =>
                {
                    let replacement =
                        crate::drive_storage::rotate_storage_channel_after_permanent_error(
                            state,
                            db,
                            storage.inner(),
                            part_backing_channel_id,
                            &second_error,
                        )
                        .await
                        .map_err(|rollover_error| {
                            format!(
                                "Drive storage channel failed twice ({first_error}; {second_error}); rollover failed: {rollover_error}"
                            )
                        })?;
                    part_backing_channel_id = replacement.backing_channel_id;
                    send_encoded_part_once(
                        &client,
                        state,
                        part_backing_channel_id,
                        &encoded_blocks,
                        part_size_usize,
                        &hidden_name,
                        &caption,
                    )
                    .await
                    .map_err(|retry_error| {
                        format!("Drive storage rollover retry failed: {retry_error}")
                    })?
                }
                Err(second_error) => return Err(second_error),
            }
        }
        Err(error) => return Err(error),
    };

    let persist_result = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        conn.execute("BEGIN IMMEDIATE")
            .map_err(|e: sqlite::Error| e.to_string())?;
        let result = (|| -> Result<(), String> {
            upsert_drive_pending_part(
                &conn,
                pending_id,
                part_index,
                expected_first,
                metadata.len() as u64,
                part_backing_channel_id,
                message_id,
                part_size,
                &part_sha256,
            )?;
            for (chunk_index, chunk_size, encoded_sha256, plaintext_sha256) in &metadata {
                upsert_drive_pending_chunk(
                    &conn,
                    pending_id,
                    *chunk_index,
                    message_id,
                    *chunk_size,
                    encoded_sha256,
                    plaintext_sha256,
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute("COMMIT")
                .map_err(|e: sqlite::Error| e.to_string()),
            Err(error) => {
                let _ = conn.execute("ROLLBACK");
                Err(error)
            }
        }
    };

    if let Err(error) = persist_result {
        if let Ok(id) = i32::try_from(message_id) {
            delete_messages_best_effort(state, part_backing_channel_id, &[id]).await;
        }
        return Err(error);
    }

    if let Some(old) = existing_part {
        if old.message_id != message_id || old.backing_channel_id != part_backing_channel_id {
            if let Ok(old_id) = i32::try_from(old.message_id) {
                delete_messages_best_effort(state, old.backing_channel_id, &[old_id]).await;
            }
        }
    }

    let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
    drive_pending_parts(&conn, pending_id)?
        .into_iter()
        .find(|part| part.part_index == part_index)
        .ok_or_else(|| "Drive remote part disappeared after upload".to_string())
}

async fn upload_logical_manifest(
    app: &AppHandle,
    state: &TelegramState,
    pending: &DrivePendingRecord,
    manifest: &LogicalFileManifestV1,
) -> Result<i64, String> {
    let client = client_for_state(state).await?;
    let peer = resolve_peer(&client, Some(pending.backing_channel_id), &state.peer_cache).await?;
    let (temp_path, manifest_size) =
        crate::commands::logical_files::write_manifest_temp(app, manifest).await?;
    let file_name = crate::commands::logical_files::manifest_file_name(&manifest.file_id);
    let caption = crate::commands::logical_files::manifest_caption(&manifest.file_id);
    let result = async {
        let mut file = tokio::fs::File::open(&temp_path)
            .await
            .map_err(|e| format!("Could not open staged Drive file manifest: {e}"))?;
        let uploaded = client
            .upload_stream(&mut file, manifest_size as usize, file_name)
            .await
            .map_err(map_error)?;
        let sent = client
            .send_message(&peer, InputMessage::new().text(caption).file(uploaded))
            .await
            .map_err(map_error)?;
        Ok::<i64, String>(i64::from(sent.id()))
    }
    .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    result
}

pub async fn finalize_pending_v2(
    app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
) -> Result<String, String> {
    let pending = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_by_id(&conn, pending_id)?
            .ok_or_else(|| "Drive pending write no longer exists".to_string())?
    };
    if pending.closed_at.is_none() {
        return Err("Drive pending write is still open".to_string());
    }
    let chunks = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_chunks(&conn, pending_id)?
    };
    let expected_chunks = if pending.size == 0 {
        1
    } else {
        pending
            .size
            .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
    };
    if chunks.len() as u64 != expected_chunks {
        return Err(format!(
            "Drive V2 write is incomplete: expected {expected_chunks} blocks, found {}",
            chunks.len()
        ));
    }
    let mut plaintext_hashes = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.chunk_index != index as u64 {
            return Err("Drive V2 block order is incomplete".to_string());
        }
        plaintext_hashes.push(decode_sha256(&chunk.plaintext_sha256)?);
    }
    let content_fingerprint =
        if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
            let key = {
                let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
                crate::drive_crypto::load_verified_master_key_for_db(app, &conn)?
            }
            .ok_or_else(|| {
                "TeraRelay Drive is locked. Unlock Drive encryption before finalizing this file."
                    .to_string()
            })?;
            crate::drive_crypto::keyed_content_fingerprint(&*key, pending.size, &plaintext_hashes)?
        } else if pending.encryption_version == 0 {
            crate::drive_crypto::public_content_fingerprint(pending.size, &plaintext_hashes)
        } else {
            return Err(format!(
                "Unsupported TeraRelay Drive encryption version {}",
                pending.encryption_version
            ));
        };

    let proposed_object_id = crate::commands::logical_files::new_file_id();
    let finalized = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        finalize_drive_v2_metadata(&conn, pending_id, &proposed_object_id, &content_fingerprint)?
    };

    if finalized.reused_existing_object && !finalized.cleanup_parts.is_empty() {
        let mut by_channel: BTreeMap<i64, Vec<i32>> = BTreeMap::new();
        for part in &finalized.cleanup_parts {
            if let Ok(message_id) = i32::try_from(part.message_id) {
                by_channel
                    .entry(part.backing_channel_id)
                    .or_default()
                    .push(message_id);
            }
        }
        for (channel_id, ids) in by_channel {
            delete_messages_best_effort(state, channel_id, &ids).await;
        }
    }

    let _ = tokio::fs::remove_dir_all(staging_dir(&pending)).await;
    Ok(finalized.object_id)
}

/// Legacy one-message-per-crypto-block finalizer. Kept only for pending writes
/// created by older builds; all newly created Drive pending writes use V2.
pub async fn finalize_pending(
    app: &AppHandle,
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
) -> Result<String, String> {
    let pending = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_by_id(&conn, pending_id)?
            .ok_or_else(|| "Drive pending write no longer exists".to_string())?
    };
    if pending.closed_at.is_none() {
        return Err("Drive pending write is still open".to_string());
    }

    let expected_chunks = if pending.size == 0 {
        1
    } else {
        pending
            .size
            .div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE)
    };
    let chunks = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        drive_pending_chunks(&conn, pending_id)?
    };
    if chunks.len() as u64 != expected_chunks {
        return Err(format!(
            "Drive write is incomplete: expected {expected_chunks} remote chunk(s), found {}",
            chunks.len()
        ));
    }
    for (expected, chunk) in chunks.iter().enumerate() {
        if chunk.chunk_index != expected as u64 || chunk.message_id <= 0 || chunk.chunk_size == 0 {
            return Err("Drive write has incomplete chunk metadata".to_string());
        }
    }

    let mut plaintext_hashes = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        plaintext_hashes.push(decode_sha256(&chunk.plaintext_sha256)?);
    }
    let content_fingerprint =
        if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION {
            let key = {
                let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
                crate::drive_crypto::load_verified_master_key_for_db(app, &conn)?
            }
            .ok_or_else(|| {
                "TeraRelay Drive is locked. Unlock Drive encryption before finalizing this file."
                    .to_string()
            })?;
            Some(crate::drive_crypto::keyed_content_fingerprint(
                &*key,
                pending.size,
                &plaintext_hashes,
            )?)
        } else if pending.encryption_version == 0 {
            Some(crate::drive_crypto::public_content_fingerprint(
                pending.size,
                &plaintext_hashes,
            ))
        } else {
            return Err(format!(
                "Unsupported TeraRelay Drive encryption version {}",
                pending.encryption_version
            ));
        };

    if let Some(fingerprint) = content_fingerprint.as_deref() {
        let duplicate = {
            let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
            find_drive_content_match(&conn, fingerprint, pending.size, pending.encryption_version)?
        };
        if let Some(existing) = duplicate {
            if pending.encryption_version == crate::drive_crypto::DRIVE_ENCRYPTION_VERSION
                && existing.crypto_id.is_none()
            {
                return Err("Encrypted Drive duplicate is missing its crypto identity".to_string());
            }
            {
                let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
                finalize_drive_streamed_pending(
                    &conn,
                    pending_id,
                    &existing.file_id,
                    pending.size,
                    Some(fingerprint),
                    existing.crypto_id.as_deref(),
                )?;
            }
            let ids: Vec<i32> = chunks
                .iter()
                .filter_map(|chunk| i32::try_from(chunk.message_id).ok())
                .collect();
            delete_messages_best_effort(state, pending.backing_channel_id, &ids).await;
            let _ = tokio::fs::remove_dir_all(staging_dir(&pending)).await;
            return Ok(existing.file_id);
        }
    }

    let logical_channel_id = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::logical_channel_id_for_backing(
            &conn,
            pending.backing_channel_id,
        )?
        .ok_or_else(|| "Drive backing channel metadata is missing".to_string())?
    };
    let file_id = crate::commands::logical_files::new_file_id();
    let total_size = chunks.iter().try_fold(0u64, |total, chunk| {
        total
            .checked_add(chunk.chunk_size)
            .ok_or_else(|| "Drive remote size overflow".to_string())
    })?;
    let manifest = LogicalFileManifestV1 {
        schema_version: crate::commands::logical_files::MANIFEST_SCHEMA_VERSION,
        file_id: file_id.clone(),
        logical_channel_id,
        original_name: format!("{DRIVE_BACKING_FILE_PREFIX}{file_id}.blob"),
        total_size,
        mime_type: mime_guess::from_path(&pending.display_name)
            .first()
            .map(|mime| mime.essence_str().to_string()),
        created_at: chrono::Utc::now().timestamp(),
        whole_sha256: None,
        chunks: chunks
            .iter()
            .map(|chunk| ManifestChunkV1 {
                index: (chunk.chunk_index + 1) as u32,
                message_id: chunk.message_id,
                size: chunk.chunk_size,
                sha256: Some(chunk.sha256.clone()),
            })
            .collect(),
    };
    crate::commands::logical_files::validate_manifest(&manifest, None)?;
    upload_logical_manifest(app, state, &pending, &manifest).await?;
    {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_files::persist_manifest(&conn, &manifest)?;
        finalize_drive_streamed_pending(
            &conn,
            pending_id,
            &file_id,
            pending.size,
            content_fingerprint.as_deref(),
            None,
        )?;
    }
    let _ = tokio::fs::remove_dir_all(staging_dir(&pending)).await;
    Ok(file_id)
}

pub async fn delete_pending_part_records(
    state: &TelegramState,
    records: &[DrivePendingPartRecord],
) {
    let mut by_channel: BTreeMap<i64, Vec<i32>> = BTreeMap::new();
    for part in records {
        if let Ok(message_id) = i32::try_from(part.message_id) {
            by_channel
                .entry(part.backing_channel_id)
                .or_default()
                .push(message_id);
        }
    }
    for (channel_id, ids) in by_channel {
        delete_messages_best_effort(state, channel_id, &ids).await;
    }
}

pub async fn delete_pending_chunk_records(
    state: &TelegramState,
    backing_channel_id: i64,
    records: &[DrivePendingChunkRecord],
) {
    let ids: Vec<i32> = records
        .iter()
        .filter_map(|chunk| i32::try_from(chunk.message_id).ok())
        .collect();
    delete_messages_best_effort(state, backing_channel_id, &ids).await;
}

pub async fn cancel_pending_remote(
    state: &TelegramState,
    db: &DbConnection,
    pending_id: &str,
) -> Result<(), String> {
    let (pending, parts, legacy_ids) = {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        let pending = drive_pending_by_id(&conn, pending_id)?;
        let parts = drive_pending_parts(&conn, pending_id)?;
        let legacy_ids: Vec<i32> = if parts.is_empty() {
            drive_pending_chunks(&conn, pending_id)?
                .into_iter()
                .filter_map(|chunk| i32::try_from(chunk.message_id).ok())
                .collect()
        } else {
            Vec::new()
        };
        (pending, parts, legacy_ids)
    };

    if !parts.is_empty() {
        let mut by_channel: BTreeMap<i64, Vec<i32>> = BTreeMap::new();
        for part in parts {
            if let Ok(message_id) = i32::try_from(part.message_id) {
                by_channel
                    .entry(part.backing_channel_id)
                    .or_default()
                    .push(message_id);
            }
        }
        for (channel_id, ids) in by_channel {
            delete_messages_best_effort(state, channel_id, &ids).await;
        }
    } else if let Some(pending) = pending {
        delete_messages_best_effort(state, pending.backing_channel_id, &legacy_ids).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_part_plan_coalesces_crypto_blocks_within_staging_budget() {
        assert!(DRIVE_REMOTE_PART_MAX_BLOCKS > 1);
        let max_encoded = DRIVE_REMOTE_PART_MAX_BLOCKS
            * (crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE + DRIVE_ENCRYPTED_CHUNK_OVERHEAD);
        assert!(max_encoded < 256 * 1024 * 1024);
        assert_eq!(remote_part_index_for_chunk(0), 0);
        assert_eq!(
            remote_part_index_for_chunk(DRIVE_REMOTE_PART_MAX_BLOCKS - 1),
            0
        );
        assert_eq!(remote_part_index_for_chunk(DRIVE_REMOTE_PART_MAX_BLOCKS), 1);
        assert_eq!(
            remote_part_chunk_range(0, 40),
            Some((0, DRIVE_REMOTE_PART_MAX_BLOCKS))
        );
        assert_eq!(remote_part_chunk_range(2, 40), Some((30, 40)));
    }

    #[test]
    fn open_files_flush_only_complete_remote_parts_and_close_flushes_tail() {
        let block = crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE;
        assert!(ready_remote_parts(14 * block, false).is_empty());
        assert_eq!(ready_remote_parts(15 * block, false), vec![(0, 0, 15)]);
        assert_eq!(ready_remote_parts(16 * block, false), vec![(0, 0, 15)]);
        assert_eq!(
            ready_remote_parts(16 * block, true),
            vec![(0, 0, 15), (1, 15, 16)]
        );
        assert_eq!(
            ready_remote_parts(15 * block + 1, true),
            vec![(0, 0, 15), (1, 15, 16)]
        );
        assert_eq!(ready_remote_parts(0, false), Vec::<(u64, u64, u64)>::new());
        assert_eq!(ready_remote_parts(0, true), vec![(0, 0, 1)]);
    }

    #[tokio::test]
    async fn encoded_part_reader_can_retry_without_copying_a_whole_part() {
        use tokio::io::AsyncReadExt;
        let blocks = vec![vec![1, 2, 3], vec![4], vec![5, 6]];
        for _ in 0..2 {
            let mut reader = EncodedPartReader::new(&blocks);
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, vec![1, 2, 3, 4, 5, 6]);
        }
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn ten_tb_logical_file_has_no_artificial_size_limit() {
        let logical_size = 10_000_000_000_000u64;
        let blocks = logical_size.div_ceil(crate::drive_crypto::DRIVE_PLAINTEXT_CHUNK_SIZE);
        let parts = remote_part_count(blocks);
        assert!(blocks > 100_000);
        assert!(parts > 0);
        assert!(parts < blocks);
        let (first, end) = remote_part_chunk_range(parts - 1, blocks).unwrap();
        assert!(first < blocks);
        assert_eq!(end, blocks);
    }
}
