use crate::bandwidth::BandwidthManager;
use crate::commands::utils::{map_error, resolve_peer};
use crate::db::DbConnection;
use crate::models::{FileMetadata, FolderMetadata};
use crate::vpn_optimizer::{backoff_ms, NetworkConfig};
use crate::TelegramState;
use grammers_client::types::{Media, Peer};
use grammers_client::InputMessage;
use grammers_mtproto::{mtp, transport};
use grammers_mtsender::{connect_with_auth, ConnectionParams, Sender, ServerAddr};
use grammers_session::Session;
use grammers_tl_types as tl;
use serde::Serialize;
use sqlite;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use tauri::{Emitter, State};
use tokio::sync::watch;

// One watch channel per transfer id; flipping it to true cancels every
// concurrently running part of that transfer.
static UPLOAD_CANCELLATIONS: OnceLock<Mutex<HashMap<String, watch::Sender<bool>>>> =
    OnceLock::new();

fn get_upload_cancellations() -> &'static Mutex<HashMap<String, watch::Sender<bool>>> {
    UPLOAD_CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn url_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    result.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

pub fn clean_file_uri(raw_path: &str) -> String {
    let decoded = url_decode(raw_path);
    decoded
        .strip_prefix("file://")
        .unwrap_or(&decoded)
        .to_string()
}

pub fn clean_android_path(raw_path: &str) -> String {
    let decoded = url_decode(raw_path);
    log::info!("URL Decoded path: {}", decoded);
    let mut cleaned = decoded;
    if cleaned.starts_with("raw%3/") {
        cleaned = cleaned.replace("raw%3/", "/");
    }
    if cleaned.starts_with("raw://") {
        cleaned = cleaned.replace("raw://", "/");
    } else if cleaned.starts_with("file://") {
        cleaned = cleaned.replace("file://", "");
    } else if cleaned.starts_with("raw:") {
        cleaned = cleaned.replace("raw:", "");
    }
    if !cleaned.starts_with("content://") {
        cleaned = cleaned.replace("//", "/");
    }
    log::info!("Cleaned absolute path: {}", cleaned);
    cleaned
}

#[cfg(target_os = "android")]
pub fn copy_to_android_cache(raw_path: &str) -> Result<String, String> {
    log::info!("JNI copy_to_android_cache started for path: {}", raw_path);
    let ctx_obj = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx_obj.vm().cast()) }
        .map_err(|e| format!("Failed to get JavaVM: {}", e))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;

    let ctx = unsafe { jni::objects::JObject::from_raw(ctx_obj.context().cast()) };

    // 1. URL Decode & Clean Path in Rust
    let cleaned = clean_android_path(raw_path);
    log::info!("JNI Cleaned path: {}", cleaned);

    // 2. Parse the selected URI directly through Android's ContentResolver.
    // No custom MainActivity methods are required; this keeps fresh Tauri
    // Android projects reproducible and avoids generated Kotlin patches.

    // 3. Parse URI
    let uri_class = env
        .find_class("android/net/Uri")
        .map_err(|e| format!("Failed to find android/net/Uri: {}", e))?;
    let j_cleaned = env
        .new_string(&cleaned)
        .map_err(|e| format!("Failed to create Java string: {}", e))?;
    let uri_val = env
        .call_static_method(
            &uri_class,
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[jni::objects::JValue::from(&j_cleaned)],
        )
        .map_err(|e| format!("Failed to parse URI: {}", e))?;

    let uri = uri_val
        .l()
        .map_err(|e| format!("URI result is not an object: {}", e))?;

    if uri.is_null() {
        return Err("Parsed URI is null".to_string());
    }

    // 4. Get ContentResolver
    let content_resolver = env
        .call_method(
            &ctx,
            "getContentResolver",
            "()Landroid/content/ContentResolver;",
            &[],
        )
        .map_err(|e| format!("Failed to get ContentResolver: {}", e))?
        .l()
        .map_err(|e| format!("ContentResolver is not an object: {}", e))?;

    // 5. Take Persistable URI Permission (best-effort, won't throw if it fails)
    if cleaned.starts_with("content://") {
        let intent_class = env
            .find_class("android/content/Intent")
            .map_err(|e| format!("Failed to find android/content/Intent: {}", e))?;
        if let Ok(flag_val) =
            env.get_static_field(&intent_class, "FLAG_GRANT_READ_URI_PERMISSION", "I")
        {
            if let Ok(flag_grant_read) = flag_val.i() {
                let res = env.call_method(
                    &content_resolver,
                    "takePersistableUriPermission",
                    "(Landroid/net/Uri;I)V",
                    &[
                        jni::objects::JValue::from(&uri),
                        jni::objects::JValue::from(flag_grant_read),
                    ],
                );
                if res.is_err() {
                    log::warn!("JNI: takePersistableUriPermission failed; clearing exception.");
                    let _ = env.exception_clear();
                }
            }
        }
    }

    // 6. Open Input Stream
    let input_stream = env
        .call_method(
            &content_resolver,
            "openInputStream",
            "(Landroid/net/Uri;)Ljava/io/InputStream;",
            &[jni::objects::JValue::from(&uri)],
        )
        .map_err(|e| format!("Failed to openInputStream: {}", e))?
        .l()
        .map_err(|e| format!("InputStream is not an object: {}", e))?;

    if input_stream.is_null() {
        return Err("InputStream is null".to_string());
    }

    // 7. Get Cache Dir
    let cache_dir_file = env
        .call_method(&ctx, "getCacheDir", "()Ljava/io/File;", &[])
        .map_err(|e| format!("Failed to getCacheDir: {}", e))?
        .l()
        .map_err(|e| format!("Cache dir is not an object: {}", e))?;

    let cache_path_jstr = env
        .call_method(
            &cache_dir_file,
            "getAbsolutePath",
            "()Ljava/lang/String;",
            &[],
        )
        .map_err(|e| format!("Failed to get absolute path of cache: {}", e))?
        .l()
        .map_err(|e| format!("Cache path is not String: {}", e))?;

    let cache_path_jstring: jni::objects::JString = cache_path_jstr.into();
    let cache_path: String = env
        .get_string(&cache_path_jstring)
        .map_err(|e| format!("Failed to convert cache path to Rust: {}", e))?
        .into();

    // 8. Get display name or file name
    let mut file_name = "temp_upload".to_string();
    if cleaned.starts_with("content://") {
        let cursor_val = env.call_method(
            &content_resolver,
            "query",
            "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
            &[
                jni::objects::JValue::from(&uri),
                jni::objects::JValue::from(&jni::objects::JObject::null()),
                jni::objects::JValue::from(&jni::objects::JObject::null()),
                jni::objects::JValue::from(&jni::objects::JObject::null()),
                jni::objects::JValue::from(&jni::objects::JObject::null()),
            ],
        );

        if let Ok(c_res) = cursor_val {
            if let Ok(cursor_obj) = c_res.l() {
                if !cursor_obj.is_null() {
                    let j_display_name = env
                        .new_string("_display_name")
                        .map_err(|e| format!("Failed to create display name string: {}", e))?;

                    let col_index = env
                        .call_method(
                            &cursor_obj,
                            "getColumnIndex",
                            "(Ljava/lang/String;)I",
                            &[jni::objects::JValue::from(&j_display_name)],
                        )
                        .ok()
                        .and_then(|r| r.i().ok())
                        .unwrap_or(-1);

                    let has_first = env
                        .call_method(&cursor_obj, "moveToFirst", "()Z", &[])
                        .ok()
                        .and_then(|r| r.z().ok())
                        .unwrap_or(false);

                    if col_index != -1 && has_first {
                        if let Ok(name_val) = env.call_method(
                            &cursor_obj,
                            "getString",
                            "(I)Ljava/lang/String;",
                            &[jni::objects::JValue::from(col_index)],
                        ) {
                            if let Ok(name_jstr_obj) = name_val.l() {
                                if !name_jstr_obj.is_null() {
                                    let name_jstring: jni::objects::JString = name_jstr_obj.into();
                                    if let Ok(name_rust) =
                                        env.get_string(&name_jstring).map(String::from)
                                    {
                                        file_name = name_rust;
                                    }
                                }
                            }
                        }
                    }
                    let _ = env.call_method(&cursor_obj, "close", "()V", &[]);
                }
            }
        }
    } else {
        if let Some(name) = std::path::Path::new(&cleaned).file_name() {
            file_name = name.to_string_lossy().to_string();
        }
    }

    // 9. Create cache file destination
    let cache_file_name = format!(
        "upload_{}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        file_name
    );
    let dest_path = std::path::Path::new(&cache_path).join(cache_file_name);
    let dest_path_str = dest_path.to_string_lossy().to_string();

    // 10. Read InputStream bytes and write to local file in Rust (with retry).
    //    Uses a helper to avoid duplicating the read loop between first attempt and retry.

    // Helper: read all bytes from an InputStream JObject and write them to dest_path.
    // Returns Ok(total_bytes_read) on success, or Err(message) on failure.
    // Closes the stream when done (both on success and on read failure).
    let read_stream_to_file = |env: &mut jni::JNIEnv,
                               stream: &jni::objects::JObject,
                               dest_path: &str|
     -> Result<u64, String> {
        let mut out_file = std::fs::File::create(dest_path)
            .map_err(|e| format!("Failed to create destination cache file: {}", e))?;

        const BUFFER_SIZE: i32 = 65536;
        let byte_array = env
            .new_byte_array(BUFFER_SIZE)
            .map_err(|e| format!("Failed to create Java byte array: {}", e))?;

        let mut total_read: u64 = 0;
        loop {
            let bytes_read = match env.call_method(
                stream,
                "read",
                "([B)I",
                &[jni::objects::JValue::from(&byte_array)],
            ) {
                Ok(val) => match val.i() {
                    Ok(n) => n,
                    Err(e) => return Err(format!("read result error: {}", e)),
                },
                Err(e) => {
                    let _ = env.exception_clear();
                    return Err(format!("Failed to read from InputStream: {}", e));
                }
            };

            if bytes_read <= 0 {
                break;
            }

            let java_bytes = env
                .convert_byte_array(&byte_array)
                .map_err(|e| format!("Failed to convert Java byte array: {}", e))?;

            use std::io::Write;
            out_file
                .write_all(&java_bytes[..bytes_read as usize])
                .map_err(|e| format!("Failed to write bytes to cache file: {}", e))?;
            total_read += bytes_read as u64;
        }

        let _ = env.call_method(stream, "close", "()V", &[]);

        // Validate the written file is non-empty
        match std::fs::metadata(dest_path) {
            Ok(meta) if meta.len() > 0 => Ok(total_read),
            Ok(meta) => Err(format!(
                "File written is {} bytes (read {} bytes from stream)",
                meta.len(),
                total_read
            )),
            Err(e) => Err(format!("Result file missing: {}", e)),
        }
    };

    // First attempt: use the already-opened input_stream
    match read_stream_to_file(&mut env, &input_stream, &dest_path_str) {
        Ok(total_read) => {
            log::info!(
                "JNI InputStream first attempt succeeded: {} ({} bytes)",
                dest_path_str,
                total_read
            );
            return Ok(dest_path_str);
        }
        Err(err) => {
            log::warn!("JNI InputStream first attempt failed: {}. Retrying...", err);
        }
    }

    // Retry: re-open the InputStream and try again
    log::info!("JNI InputStream retry attempt for: {}", dest_path_str);
    let retry_result = env.call_method(
        &content_resolver,
        "openInputStream",
        "(Landroid/net/Uri;)Ljava/io/InputStream;",
        &[jni::objects::JValue::from(&uri)],
    );
    let retry_stream = match retry_result {
        Ok(val) => match val.l() {
            Ok(obj) if !obj.is_null() => obj,
            _ => return Err("Retry: Failed to open InputStream".to_string()),
        },
        Err(e) => {
            let _ = env.exception_clear();
            return Err(format!("Retry: Failed to open InputStream: {}", e));
        }
    };

    match read_stream_to_file(&mut env, &retry_stream, &dest_path_str) {
        Ok(total_read) => {
            log::info!(
                "JNI InputStream retry succeeded: {} ({} bytes)",
                dest_path_str,
                total_read
            );
            Ok(dest_path_str)
        }
        Err(err) => Err(format!("InputStream copy failed after retry: {}", err)),
    }
}

#[cfg(target_os = "android")]
fn copy_file_to_android_uri(source_path: &str, destination_uri: &str) -> Result<(), String> {
    use std::io::Read;

    let ctx_obj = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx_obj.vm().cast()) }
        .map_err(|e| format!("Failed to get JavaVM: {}", e))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;
    let ctx = unsafe { jni::objects::JObject::from_raw(ctx_obj.context().cast()) };

    let uri_class = env
        .find_class("android/net/Uri")
        .map_err(|e| format!("Failed to find android/net/Uri: {}", e))?;
    let j_uri = env
        .new_string(destination_uri)
        .map_err(|e| format!("Failed to create destination URI string: {}", e))?;
    let uri = env
        .call_static_method(
            &uri_class,
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[jni::objects::JValue::from(&j_uri)],
        )
        .map_err(|e| format!("Failed to parse destination URI: {}", e))?
        .l()
        .map_err(|e| format!("Destination URI is not an object: {}", e))?;

    let resolver = env
        .call_method(
            &ctx,
            "getContentResolver",
            "()Landroid/content/ContentResolver;",
            &[],
        )
        .map_err(|e| format!("Failed to get ContentResolver: {}", e))?
        .l()
        .map_err(|e| format!("ContentResolver is not an object: {}", e))?;

    let output_stream = env
        .call_method(
            &resolver,
            "openOutputStream",
            "(Landroid/net/Uri;)Ljava/io/OutputStream;",
            &[jni::objects::JValue::from(&uri)],
        )
        .map_err(|e| format!("Failed to open destination OutputStream: {}", e))?
        .l()
        .map_err(|e| format!("OutputStream is not an object: {}", e))?;

    if output_stream.is_null() {
        return Err("Android save destination returned a null OutputStream".to_string());
    }

    let mut source = std::fs::File::open(source_path)
        .map_err(|e| format!("Failed to open downloaded cache file: {}", e))?;
    let mut buffer = vec![0u8; 256 * 1024];

    let copy_result = (|| -> Result<(), String> {
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|e| format!("Failed to read downloaded cache file: {}", e))?;
            if read == 0 {
                break;
            }
            let chunk = env
                .byte_array_from_slice(&buffer[..read])
                .map_err(|e| format!("Failed to allocate Android output buffer: {}", e))?;
            env.call_method(
                &output_stream,
                "write",
                "([B)V",
                &[jni::objects::JValue::from(&chunk)],
            )
            .map_err(|e| format!("Failed to write Android save destination: {}", e))?;
        }

        env.call_method(&output_stream, "flush", "()V", &[])
            .map_err(|e| format!("Failed to flush Android save destination: {}", e))?;
        Ok(())
    })();

    let close_result = env.call_method(&output_stream, "close", "()V", &[]);
    if let Err(e) = close_result {
        if copy_result.is_ok() {
            return Err(format!("Failed to close Android save destination: {}", e));
        }
    }

    copy_result
}

#[cfg(not(target_os = "android"))]
pub fn copy_to_android_cache(_raw_path: &str) -> Result<String, String> {
    Err("Not supported on this platform".to_string())
}

pub async fn create_folder_inner(
    name: &str,
    client: &grammers_client::Client,
    peer_cache: &Arc<tokio::sync::RwLock<HashMap<i64, Peer>>>,
) -> Result<FolderMetadata, String> {
    log::info!("Creating Telegram Channel: {}", name);

    let result = client
        .invoke(&tl::functions::channels::CreateChannel {
            broadcast: true,
            megagroup: false,
            title: format!("{} [TR]", name),
            about: "TeraRelay Storage Folder\n[terarelay-folder]".to_string(),
            geo_point: None,
            address: None,
            for_import: false,
            forum: false,
            ttl_period: None,
        })
        .await
        .map_err(map_error)?;

    let (chat_id, access_hash) = match &result {
        tl::enums::Updates::Updates(u) => {
            let chat = u.chats.first().ok_or("No chat in updates")?;
            match chat {
                tl::enums::Chat::Channel(c) => {
                    let channel_obj = grammers_client::types::Channel { raw: c.clone() };
                    peer_cache
                        .write()
                        .await
                        .insert(c.id, grammers_client::types::Peer::Channel(channel_obj));
                    (c.id, c.access_hash.unwrap_or(0))
                }
                _ => return Err("Created chat is not a channel".to_string()),
            }
        }
        _ => return Err("Unexpected response (not Updates::Updates)".to_string()),
    };

    let _ = client
        .invoke(&tl::functions::messages::SetHistoryTtl {
            peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: chat_id,
                access_hash,
            }),
            period: 0,
        })
        .await;
    Ok(FolderMetadata {
        id: chat_id,
        name: name.to_string(),
        parent_id: None,
        username: None,
        is_public: false,
        group_id: None,
        display_order: 0,
        role: Some("owner".to_string()),
    })
}

#[tauri::command]
pub async fn cmd_create_folder(
    name: String,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<FolderMetadata, String> {
    let client_opt = { state.client.lock().await.clone() };

    let mut folder = if client_opt.is_none() {
        let mock_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        log::info!("[MOCK] Created folder '{}' with ID {}", name, mock_id);
        FolderMetadata {
            id: mock_id,
            name,
            parent_id: None,
            username: None,
            is_public: false,
            group_id: None,
            display_order: 0,
            role: Some("owner".to_string()),
        }
    } else {
        let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;
        create_folder_inner(&name, &client, &state.peer_cache).await?
    };

    // Save to SQLite
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;

    // Calculate new display order
    let mut max_stmt = conn
        .prepare("SELECT MAX(display_order) FROM folder_metadata")
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut display_order = 0;
    if let sqlite::State::Row = max_stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        display_order = max_stmt
            .read::<Option<i64>, _>(0)
            .ok()
            .flatten()
            .unwrap_or(0)
            + 1;
    }

    let mut insert_stmt = conn
        .prepare("INSERT INTO folder_metadata (channel_id, name, username, is_public, display_order, group_id) VALUES (?, ?, ?, ?, ?, NULL)")
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .bind((1, folder.id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .bind((2, folder.name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .bind((3, folder.username.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .bind((4, if folder.is_public { 1 } else { 0 }))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .bind((5, display_order))
        .map_err(|e: sqlite::Error| e.to_string())?;
    insert_stmt
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?;

    // Every user-facing TeraRelay folder now has a durable logical identity
    // separate from its Telegram backing channel ID. Existing commands keep
    // using the backing ID for compatibility while the logical layer is
    // introduced incrementally.
    crate::commands::logical_channels::ensure_logical_channel(
        &conn,
        folder.id,
        &folder.name,
        "owner",
    )?;

    folder.display_order = display_order as i32;
    Ok(folder)
}

pub async fn delete_folder_inner(
    folder_id: i64,
    client: &grammers_client::Client,
    peer_cache: &Arc<tokio::sync::RwLock<HashMap<i64, Peer>>>,
) -> Result<bool, String> {
    log::info!("Deleting owned backing channel: {}", folder_id);

    let peer = resolve_peer(client, Some(folder_id), peer_cache).await?;

    let input_channel = match peer {
        Peer::Channel(c) => {
            let chan = &c.raw;
            tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: chan.id,
                access_hash: chan.access_hash.ok_or("No access hash for channel")?,
            })
        }
        _ => return Err("Only channels (folders) can be deleted.".to_string()),
    };

    client
        .invoke(&tl::functions::channels::DeleteChannel {
            channel: input_channel,
        })
        .await
        .map_err(|e| format!("Failed to delete channel: {}", e))?;

    Ok(true)
}

async fn leave_member_channel_inner(
    folder_id: i64,
    client: &grammers_client::Client,
    peer_cache: &Arc<tokio::sync::RwLock<HashMap<i64, Peer>>>,
) -> Result<bool, String> {
    log::info!("Leaving joined TeraRelay backing channel: {}", folder_id);
    let peer = resolve_peer(client, Some(folder_id), peer_cache).await?;
    let input_channel = match peer {
        Peer::Channel(c) => tl::enums::InputChannel::Channel(tl::types::InputChannel {
            channel_id: c.raw.id,
            access_hash: c
                .raw
                .access_hash
                .ok_or("No access hash for backing channel")?,
        }),
        _ => return Err("TeraRelay backing storage is not a Telegram channel".to_string()),
    };

    match client
        .invoke(&tl::functions::channels::LeaveChannel {
            channel: input_channel,
        })
        .await
    {
        Ok(_) => Ok(true),
        Err(e) => {
            let message = e.to_string();
            if message.contains("USER_NOT_PARTICIPANT") {
                Ok(true)
            } else {
                Err(format!("Failed to leave TeraRelay channel: {}", e))
            }
        }
    }
}

#[tauri::command]
pub async fn cmd_delete_folder(
    folder_id: i64,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let role = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::role_for_backing_channel(&conn, folder_id)?
    };
    let is_member = role.as_deref() == Some("member");
    let client_opt = { state.client.lock().await.clone() };

    if client_opt.is_none() {
        log::info!(
            "[MOCK] {} folder ID {}",
            if is_member { "Left" } else { "Deleted" },
            folder_id
        );
    } else {
        let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;
        if is_member {
            leave_member_channel_inner(folder_id, &client, &state.peer_cache).await?;
        } else {
            delete_folder_inner(folder_id, &client, &state.peer_cache).await?;
        }
    }

    // Removing a logical channel locally also removes its manifest/index rows
    // through the logical_files foreign-key relationship. Owners delete the
    // backing Telegram channel; members only leave it.
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        let mut folder_stmt = conn
            .prepare("DELETE FROM folder_metadata WHERE channel_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        folder_stmt
            .bind((1, folder_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        folder_stmt
            .next()
            .map_err(|e: sqlite::Error| e.to_string())?;

        let mut logical_stmt = conn
            .prepare("DELETE FROM logical_channels WHERE backing_channel_id = ?")
            .map_err(|e: sqlite::Error| e.to_string())?;
        logical_stmt
            .bind((1, folder_id))
            .map_err(|e: sqlite::Error| e.to_string())?;
        logical_stmt
            .next()
            .map_err(|e: sqlite::Error| e.to_string())?;
    }

    state.peer_cache.write().await.remove(&folder_id);
    Ok(true)
}

pub async fn rename_folder_inner(
    folder_id: i64,
    new_name: &str,
    client: &grammers_client::Client,
    peer_cache: &Arc<tokio::sync::RwLock<HashMap<i64, Peer>>>,
) -> Result<bool, String> {
    log::info!("Renaming folder/channel: {} to {}", folder_id, new_name);

    let peer = resolve_peer(client, Some(folder_id), peer_cache).await?;

    let input_channel = match peer {
        Peer::Channel(c) => {
            let chan = &c.raw;
            tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: chan.id,
                access_hash: chan.access_hash.ok_or("No access hash for channel")?,
            })
        }
        _ => return Err("Only channels (folders) can be renamed.".to_string()),
    };

    client
        .invoke(&tl::functions::channels::EditTitle {
            channel: input_channel,
            title: format!("{} [TR]", new_name),
        })
        .await
        .map_err(|e| format!("Failed to rename channel: {}", e))?;

    Ok(true)
}

#[tauri::command]
pub async fn cmd_rename_folder(
    folder_id: i64,
    new_name: String,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::require_owner_role(&conn, folder_id)?;
    }

    let client_opt = { state.client.lock().await.clone() };

    if client_opt.is_none() {
        log::info!("[MOCK] Renamed folder ID {} to {}", folder_id, new_name);
    } else {
        let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;
        rename_folder_inner(folder_id, &new_name, &client, &state.peer_cache).await?;
    }

    // Update SQLite
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    let mut stmt = conn
        .prepare("UPDATE folder_metadata SET name = ? WHERE channel_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, new_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, folder_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;

    let mut logical_stmt = conn
        .prepare("UPDATE logical_channels SET name = ? WHERE backing_channel_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical_stmt
        .bind((1, new_name.as_str()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical_stmt
        .bind((2, folder_id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    logical_stmt
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?;

    Ok(true)
}

#[derive(Clone, serde::Serialize)]
struct ProgressPayload {
    id: String,
    percent: u8,
    uploaded_bytes: u64,
    total_bytes: u64,
    speed_bytes_per_sec: u64,
}

/// Report real acknowledged throughput over a short sliding window.
/// No exponential smoothing or guessed carry-forward: the displayed MB/s is
/// computed only from bytes Telegram has actually acknowledged.
struct SpeedWindow {
    total: u64,
    samples: std::collections::VecDeque<(std::time::Instant, u64)>,
}

impl SpeedWindow {
    fn new() -> Self {
        let now = std::time::Instant::now();
        let mut samples = std::collections::VecDeque::new();
        samples.push_back((now, 0));
        Self { total: 0, samples }
    }

    fn update(&mut self, bytes_delta: u64, _seconds: f64) -> u64 {
        self.total = self.total.saturating_add(bytes_delta);
        let now = std::time::Instant::now();
        self.samples.push_back((now, self.total));

        // Average measured traffic across coalesced TDLib reports. File-byte
        // progress remains independent of this presentation window.
        while self.samples.len() > 2 {
            let remove = self
                .samples
                .get(1)
                .map(|(time, _)| now.duration_since(*time).as_secs_f64() > 12.0)
                .unwrap_or(false);
            if remove {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        let Some((old_time, old_total)) = self.samples.front().copied() else {
            return 0;
        };
        let elapsed = now.duration_since(old_time).as_secs_f64();
        if elapsed < 0.20 {
            return 0;
        }
        ((self.total.saturating_sub(old_total)) as f64 / elapsed).round() as u64
    }
}

// ── Split-file support ──────────────────────────────────────────────
// Files larger than this are split into multiple Telegram documents named
// "<name>.tgdpart<NNN>-<TTT>" (caption + document filename).
//
// This is deliberately decimal 2 GB (2,000,000,000 bytes), not 2 GiB.
// Keeping the storage boundary explicit avoids UI/unit ambiguity and stays
// below Telegram's normal per-document upload ceiling with safety headroom.
const SPLIT_PART_SIZE: u64 = 2_000_000_000;
const SPLIT_MARKER: &str = ".tgdpart";

pub fn split_part_size() -> u64 {
    // TGD_PART_SIZE env override is for testing split logic with small files
    std::env::var("TGD_PART_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SPLIT_PART_SIZE)
}

// Keep logical-document concurrency separate for upload and download.
// grammers already pipelines each large Telegram document internally; running a
// small number of split documents side by side fills idle network time without
// changing the stored file format. Downloads are less flood-sensitive and can
// use a wider lane count.
const UPLOAD_MAX_CONNECTIONS: usize = 8;
const UPLOAD_INITIAL_CONNECTIONS: usize = 4;
const UPLOAD_PART_WORKERS: usize = 16;
const UPLOAD_MIN_PART_WORKERS: usize = 8;
const UPLOAD_MAX_PART_WORKERS: usize = 32;
const UPLOAD_MAIN_SINGLE_WORKERS: usize = 5;
const UPLOAD_MAIN_SINGLE_MIN_WORKERS: usize = 2;
const UPLOAD_MAIN_SINGLE_MAX_WORKERS: usize = 10;
const UPLOAD_ADAPT_BYTES: u64 = 16 * 1024 * 1024;
const DOWNLOAD_PARALLEL_PARTS: usize = 4;

fn upload_connection_limit() -> usize {
    std::env::var("TERARELAY_UPLOAD_CONNECTIONS")
        .or_else(|_| std::env::var("TERARELAY_UPLOAD_LANES"))
        .or_else(|_| std::env::var("TGD_PARALLEL_PARTS"))
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| (1..=UPLOAD_MAX_CONNECTIONS).contains(n))
        .unwrap_or(UPLOAD_MAX_CONNECTIONS)
}

fn upload_pool_plan(
    has_media_endpoint: bool,
    connection_limit: usize,
    tmp_sessions: usize,
) -> (&'static str, usize, usize, bool, usize) {
    let limit = connection_limit.clamp(1, UPLOAD_MAX_CONNECTIONS);
    if has_media_endpoint {
        let initial = UPLOAD_INITIAL_CONNECTIONS.min(limit).max(1);
        ("media-pool", limit, initial, false, UPLOAD_PART_WORKERS)
    } else {
        let allowed_total = tmp_sessions.max(1).min(limit);
        if allowed_total <= 1 {
            ("main-single", 1, 1, true, UPLOAD_MAIN_SINGLE_WORKERS)
        } else {
            let initial = UPLOAD_INITIAL_CONNECTIONS.min(allowed_total).max(1);
            (
                "tmp-session-pool",
                allowed_total,
                initial,
                true,
                UPLOAD_PART_WORKERS,
            )
        }
    }
}

fn adapt_upload_parallelism(
    mode: &str,
    active_lanes: usize,
    workers: usize,
    max_lanes: usize,
    previous_speed: Option<f64>,
    current_speed: f64,
) -> (usize, usize) {
    if current_speed <= 0.0 {
        return (active_lanes, workers);
    }

    if mode == "main-single" {
        let mut next_workers = workers.clamp(
            UPLOAD_MAIN_SINGLE_MIN_WORKERS,
            UPLOAD_MAIN_SINGLE_MAX_WORKERS,
        );
        match previous_speed {
            None => {
                next_workers = (next_workers + 1).min(UPLOAD_MAIN_SINGLE_MAX_WORKERS);
            }
            Some(previous) if current_speed >= previous * 0.97 => {
                next_workers = (next_workers + 1).min(UPLOAD_MAIN_SINGLE_MAX_WORKERS);
            }
            Some(previous) if current_speed < previous * 0.85 => {
                next_workers = next_workers
                    .saturating_sub(1)
                    .max(UPLOAD_MAIN_SINGLE_MIN_WORKERS);
            }
            _ => {}
        }
        return (1, next_workers);
    }

    let mut next_lanes = active_lanes.clamp(1, max_lanes.max(1));
    let mut next_workers = workers.clamp(UPLOAD_MIN_PART_WORKERS, UPLOAD_MAX_PART_WORKERS);
    match previous_speed {
        None => {
            let step = next_lanes.max(2);
            next_workers = (next_workers + step).min(UPLOAD_MAX_PART_WORKERS);
            next_lanes = (next_lanes + 1).min(max_lanes.max(1));
        }
        Some(previous) if current_speed >= previous * 0.97 => {
            let step = next_lanes.max(2);
            next_workers = (next_workers + step).min(UPLOAD_MAX_PART_WORKERS);
            next_lanes = (next_lanes + 1).min(max_lanes.max(1));
        }
        Some(previous) if current_speed < previous * 0.85 => {
            let step = next_lanes.max(2);
            next_workers = next_workers
                .saturating_sub(step)
                .max(UPLOAD_MIN_PART_WORKERS);
            next_lanes = next_lanes.saturating_sub(1).max(1);
        }
        _ => {}
    }
    (next_lanes, next_workers)
}

fn observe_upload_ack(
    mode: &str,
    max_lanes: usize,
    active_lanes: &mut usize,
    workers: &mut usize,
    previous_speed: &mut Option<f64>,
    stage_acknowledged: &mut u64,
    stage_started: &mut std::time::Instant,
    acknowledged: usize,
) {
    *stage_acknowledged = stage_acknowledged.saturating_add(acknowledged as u64);
    if *stage_acknowledged < UPLOAD_ADAPT_BYTES {
        return;
    }

    let elapsed = stage_started.elapsed().as_secs_f64().max(0.001);
    let current_speed = *stage_acknowledged as f64 / elapsed;
    let old_lanes = *active_lanes;
    let old_workers = *workers;
    let (next_lanes, next_workers) = adapt_upload_parallelism(
        mode,
        old_lanes,
        old_workers,
        max_lanes,
        *previous_speed,
        current_speed,
    );

    log::info!(
        "Single-file upload tuning: mode={}, stage={:.2} MiB/s, lanes={}->{}, workers={}->{}",
        mode,
        current_speed / (1024.0 * 1024.0),
        old_lanes,
        next_lanes,
        old_workers,
        next_workers
    );

    *active_lanes = next_lanes;
    *workers = next_workers;
    *previous_speed = Some(current_speed);
    *stage_acknowledged = 0;
    *stage_started = std::time::Instant::now();
}

fn download_parallel_parts() -> usize {
    std::env::var("TERARELAY_DOWNLOAD_LANES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| (1..=8).contains(n))
        .unwrap_or(DOWNLOAD_PARALLEL_PARTS)
}

type RawUploadSender = Sender<transport::Full, mtp::Encrypted>;
type SharedUploadSender = Arc<tokio::sync::Mutex<RawUploadSender>>;

#[derive(Clone)]
enum UploadLane {
    Raw(SharedUploadSender),
    Main {
        client: grammers_client::Client,
        lock: Option<Arc<tokio::sync::Mutex<()>>>,
    },
}

struct UploadLanePool {
    lanes: Vec<UploadLane>,
    mode: &'static str,
    initial_active_lanes: usize,
    worker_window: usize,
}

/// Build low-level MTProto senders from the already-authorized Grammers auth
/// key. It uses one real TCP sender per lane with one in-flight
/// SaveBigFilePart request per sender.
/// No second Telegram login/session is created.
async fn build_upload_lane_pool(
    main_client: &grammers_client::Client,
    state: &TelegramState,
    net_config: &NetworkConfig,
) -> Result<UploadLanePool, String> {
    let source = state
        .session
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Telegram session is not available".to_string())?;
    let api_id = state
        .api_id
        .lock()
        .await
        .ok_or_else(|| "Telegram API ID is not available".to_string())?;

    let home_dc = source.home_dc_id();
    let base_dc = source
        .dc_option(home_dc)
        .ok_or_else(|| format!("Telegram home DC {home_dc} is unavailable"))?;
    let auth_key = base_dc
        .auth_key
        .ok_or_else(|| "Telegram session has no authorization key".to_string())?;

    let config = main_client
        .invoke(&tl::functions::help::GetConfig {})
        .await
        .map_err(map_error)?;
    let tl::enums::Config::Config(config) = config;

    let media_endpoint = config.dc_options.iter().find_map(|entry| {
        let tl::enums::DcOption::Option(option) = entry;
        if option.id != home_dc || !option.media_only || option.cdn || option.ipv6 {
            return None;
        }
        let ip = option.ip_address.parse::<std::net::Ipv4Addr>().ok()?;
        let port = u16::try_from(option.port).ok()?;
        Some(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
            ip, port,
        )))
    });

    let connection_limit = upload_connection_limit();
    let tmp_sessions = config.tmp_sessions.unwrap_or(1).max(1) as usize;
    let (mode, desired_total, initial_active_lanes, include_main, worker_window) =
        upload_pool_plan(media_endpoint.is_some(), connection_limit, tmp_sessions);
    let params = ConnectionParams::default();

    if mode == "main-single" {
        log::info!(
            "Fast upload sender pool ready: mode=main-single, workers={}",
            worker_window
        );
        return Ok(UploadLanePool {
            lanes: vec![UploadLane::Main {
                client: main_client.clone(),
                lock: None,
            }],
            mode,
            initial_active_lanes,
            worker_window,
        });
    }

    let address = media_endpoint.unwrap_or(std::net::SocketAddr::V4(base_dc.ipv4));

    let mut lanes = Vec::with_capacity(desired_total.max(1));
    if include_main {
        // Telegram's tmp_sessions limit counts the already-connected main
        // session. Reuse it as lane 1 instead of accidentally opening
        // tmp_sessions extra sockets on top of it.
        lanes.push(UploadLane::Main {
            client: main_client.clone(),
            lock: Some(Arc::new(tokio::sync::Mutex::new(()))),
        });
    }

    let raw_needed = desired_total.saturating_sub(lanes.len());
    for _ in 0..raw_needed {
        let server = if let Some(proxy) = net_config.effective_proxy_url() {
            ServerAddr::Proxied { address, proxy }
        } else {
            ServerAddr::Tcp { address }
        };

        let connect_result = async {
            let mut sender = connect_with_auth(transport::Full::new(), server, auth_key)
                .await
                .map_err(map_error)?;

            let init = tl::functions::InvokeWithLayer {
                layer: tl::LAYER,
                query: tl::functions::InitConnection {
                    api_id,
                    device_model: params.device_model.clone(),
                    system_version: params.system_version.clone(),
                    app_version: params.app_version.clone(),
                    system_lang_code: params.system_lang_code.clone(),
                    lang_pack: "".into(),
                    lang_code: params.lang_code.clone(),
                    proxy: None,
                    params: None,
                    query: tl::functions::help::GetConfig {},
                },
            };
            sender.invoke(&init).await.map_err(map_error)?;
            Ok::<RawUploadSender, String>(sender)
        }
        .await;

        match connect_result {
            Ok(sender) => {
                let logical_lane = lanes.len() + 1;
                log::debug!("Upload sender lane {} connected via {}", logical_lane, mode);
                lanes.push(UploadLane::Raw(Arc::new(tokio::sync::Mutex::new(sender))));
            }
            Err(error) if !lanes.is_empty() => {
                log::warn!(
                    "Upload lane {} could not connect; keeping {} working lane(s): {}",
                    lanes.len() + 1,
                    lanes.len(),
                    error
                );
                break;
            }
            Err(error) => return Err(error),
        }
    }

    if lanes.is_empty() {
        return Err("No Telegram upload lanes could be established".to_string());
    }

    log::info!(
        "Fast upload sender pool ready: mode={}, lanes={}, target={}, endpoint={}",
        mode,
        lanes.len(),
        desired_total,
        address
    );

    Ok(UploadLanePool {
        lanes,
        mode,
        initial_active_lanes: initial_active_lanes.min(desired_total).max(1),
        worker_window,
    })
}

const FAST_UPLOAD_CHUNK: usize = 512 * 1024;
const FAST_UPLOAD_BIG_THRESHOLD: u64 = 10 * 1024 * 1024;

fn telegram_upload_chunk_size(len: u64, mode: &str) -> usize {
    if mode != "main-single" {
        return FAST_UPLOAD_CHUNK;
    }
    if len <= 100 * 1024 * 1024 {
        128 * 1024
    } else if len <= 750 * 1024 * 1024 {
        256 * 1024
    } else {
        FAST_UPLOAD_CHUNK
    }
}

async fn save_upload_part_with_retry(
    lane: UploadLane,
    file_id: i64,
    part_index: i32,
    part_count: i32,
    is_big: bool,
    bytes: Vec<u8>,
    max_retries: u32,
    base_ms: u64,
    max_ms: u64,
    respect_flood: bool,
) -> Result<usize, String> {
    let byte_len = bytes.len();
    let mut last_err = String::new();

    for attempt in 0..=max_retries {
        let result = if is_big {
            let request = tl::functions::upload::SaveBigFilePart {
                file_id,
                file_part: part_index,
                file_total_parts: part_count,
                bytes: bytes.clone(),
            };
            match &lane {
                UploadLane::Raw(sender) => {
                    let mut locked = sender.lock().await;
                    locked.invoke(&request).await
                }
                UploadLane::Main { client, lock } => {
                    if let Some(lock) = lock {
                        let _guard = lock.lock().await;
                        client.invoke(&request).await
                    } else {
                        client.invoke(&request).await
                    }
                }
            }
        } else {
            let request = tl::functions::upload::SaveFilePart {
                file_id,
                file_part: part_index,
                bytes: bytes.clone(),
            };
            match &lane {
                UploadLane::Raw(sender) => {
                    let mut locked = sender.lock().await;
                    locked.invoke(&request).await
                }
                UploadLane::Main { client, lock } => {
                    if let Some(lock) = lock {
                        let _guard = lock.lock().await;
                        client.invoke(&request).await
                    } else {
                        client.invoke(&request).await
                    }
                }
            }
        };

        match result {
            Ok(true) => return Ok(byte_len),
            Ok(false) => last_err = "Telegram rejected an upload chunk".to_string(),
            Err(error) => {
                let mapped = map_error(error);
                if respect_flood && mapped.starts_with("FLOOD_WAIT_") {
                    if let Ok(secs) = mapped.trim_start_matches("FLOOD_WAIT_").parse::<u64>() {
                        let wait = secs.min(300);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        last_err = mapped;
                        continue;
                    }
                }
                last_err = mapped;
            }
        }

        if attempt < max_retries {
            let delay = backoff_ms(attempt, base_ms, max_ms);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
    }

    Err(format!(
        "Upload chunk {} failed after {} attempts: {}",
        part_index,
        max_retries + 1,
        last_err
    ))
}

/// Upload one Telegram document through the already-authorized transfer pool.
/// Bytes are added to the progress counter only after Telegram acknowledges the
/// corresponding SaveFilePart/SaveBigFilePart RPC.
async fn upload_range_fast(
    pool: &UploadLanePool,
    path: &str,
    offset: u64,
    len: u64,
    doc_name: String,
    bytes_counter: Arc<std::sync::atomic::AtomicU64>,
    mut cancel_rx: watch::Receiver<bool>,
    net_config: &NetworkConfig,
    compute_hash: bool,
) -> Result<(tl::enums::InputFile, Option<String>), String> {
    if pool.lanes.is_empty() {
        return Err("No Telegram upload connections are available".to_string());
    }

    use futures::stream::{FuturesUnordered, StreamExt};
    use sha2::Digest;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let file_id = rand::random::<i64>();
    let is_big = len > FAST_UPLOAD_BIG_THRESHOLD;
    let chunk_size = telegram_upload_chunk_size(len, pool.mode);
    let part_count_u64 = len.div_ceil(chunk_size as u64);
    let part_count = i32::try_from(part_count_u64)
        .map_err(|_| "Telegram document needs too many upload chunks".to_string())?;

    let mut source = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("Failed to open upload source: {e}"))?;
    source
        .seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("Failed to seek upload source: {e}"))?;

    let max_retries = net_config.retry_attempts();
    let base_ms = net_config.retry_base_backoff_ms();
    let max_ms = net_config.retry_max_backoff_ms();
    let respect_flood = net_config.should_respect_flood_wait();
    let max_lanes = pool.lanes.len().max(1);
    let mut active_lanes = pool.initial_active_lanes.min(max_lanes).max(1);
    let mut window = pool.worker_window.max(1);
    let mut previous_stage_speed = None;
    let mut stage_acknowledged = 0u64;
    let mut stage_started = std::time::Instant::now();
    let mut inflight = FuturesUnordered::new();
    let mut sha256 = compute_hash.then(sha2::Sha256::new);
    let mut md5 = (!is_big).then(md5::Context::new);
    let mut remaining = len;

    log::info!(
        "Single-file upload start: mode={}, active_lanes={}, available_lanes={}, workers={}, chunk={} KiB",
        pool.mode,
        active_lanes,
        max_lanes,
        window,
        chunk_size / 1024
    );

    for part_index in 0..part_count {
        if *cancel_rx.borrow() {
            return Err("Transfer cancelled".to_string());
        }

        let chunk_len = remaining.min(chunk_size as u64) as usize;
        let mut chunk = vec![0u8; chunk_len];
        source
            .read_exact(&mut chunk)
            .await
            .map_err(|e| format!("Failed reading upload chunk {part_index}: {e}"))?;
        remaining -= chunk_len as u64;

        if let Some(hash) = sha256.as_mut() {
            hash.update(&chunk);
        }
        if let Some(hash) = md5.as_mut() {
            hash.consume(&chunk);
        }

        let lane = pool.lanes[part_index as usize % active_lanes].clone();
        let counter = bytes_counter.clone();
        inflight.push(async move {
            let acknowledged = save_upload_part_with_retry(
                lane,
                file_id,
                part_index,
                part_count,
                is_big,
                chunk,
                max_retries,
                base_ms,
                max_ms,
                respect_flood,
            )
            .await?;
            counter.fetch_add(acknowledged as u64, std::sync::atomic::Ordering::Relaxed);
            Ok::<usize, String>(acknowledged)
        });

        while inflight.len() >= window {
            tokio::select! {
                result = inflight.next() => {
                    if let Some(result) = result {
                        let acknowledged = result?;
                        observe_upload_ack(
                            pool.mode,
                            max_lanes,
                            &mut active_lanes,
                            &mut window,
                            &mut previous_stage_speed,
                            &mut stage_acknowledged,
                            &mut stage_started,
                            acknowledged,
                        );
                    }
                }
                _ = cancel_rx.changed() => {
                    return Err("Transfer cancelled".to_string());
                }
            }
        }
    }

    while !inflight.is_empty() {
        tokio::select! {
            result = inflight.next() => {
                match result {
                    Some(result) => {
                        let acknowledged = result?;
                        observe_upload_ack(
                            pool.mode,
                            max_lanes,
                            &mut active_lanes,
                            &mut window,
                            &mut previous_stage_speed,
                            &mut stage_acknowledged,
                            &mut stage_started,
                            acknowledged,
                        );
                    }
                    None => break,
                }
            }
            _ = cancel_rx.changed() => {
                return Err("Transfer cancelled".to_string());
            }
        }
    }

    let hash = sha256.map(|hasher| format!("{:x}", hasher.finalize()));
    let uploaded: tl::enums::InputFile = if is_big {
        tl::types::InputFileBig {
            id: file_id,
            parts: part_count,
            name: doc_name,
        }
        .into()
    } else {
        let checksum = md5
            .map(|ctx| format!("{:x}", ctx.finalize()))
            .unwrap_or_default();
        tl::types::InputFile {
            id: file_id,
            parts: part_count,
            name: doc_name,
            md5_checksum: checksum,
        }
        .into()
    };

    Ok((uploaded, hash))
}

fn checked_upload_part_count(size: u64, part_size: u64) -> Result<u32, String> {
    if part_size == 0 {
        return Err("Upload part size must be positive".to_string());
    }
    u32::try_from(size.div_ceil(part_size).max(1))
        .map_err(|_| "File requires more parts than the supported part-number range".to_string())
}

/// Fetch bounded metadata batches; reconstruction still follows the manifest
/// IDs explicitly rather than relying on Telegram response order.
pub(crate) async fn get_messages_in_batches(
    client: &grammers_client::Client,
    peer: &Peer,
    ids: &[i32],
) -> Result<Vec<Option<grammers_client::types::Message>>, String> {
    let mut messages = Vec::with_capacity(ids.len());
    for batch in ids.chunks(100) {
        messages.extend(
            client
                .get_messages_by_id(peer, batch)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(messages)
}

async fn verified_upload_parts(
    client: &grammers_client::Client,
    peer: &Peer,
    path: &str,
    base: &str,
    total: u32,
    part_size: u64,
    journal: &crate::commands::transfers::UploadJournal,
) -> Result<HashMap<u32, (i32, u64, Option<String>)>, String> {
    let checkpoint_ids: Vec<i32> = journal
        .snapshot
        .chunks
        .values()
        .map(|chunk| chunk.message_id as i32)
        .collect();
    let messages = get_messages_in_batches(client, peer, &checkpoint_ids).await?;
    let message_map: HashMap<i32, _> = messages
        .into_iter()
        .flatten()
        .map(|message| (message.id(), message))
        .collect();
    let mut verified = HashMap::new();
    for chunk in journal.snapshot.chunks.values() {
        let Some(message) = message_map.get(&(chunk.message_id as i32)) else {
            continue;
        };
        let Some(Media::Document(document)) = message.media() else {
            continue;
        };
        if document.size() as u64 != chunk.size {
            continue;
        }
        let correct_caption = if total == 1 {
            true
        } else {
            let name = message_display_name(message);
            parse_part_name(&name).is_some_and(|(b, i, t, h)| {
                b == base && i == chunk.index && t == total && h == chunk.sha256.as_deref()
            })
        };
        if correct_caption {
            verified.insert(
                chunk.index,
                (chunk.message_id as i32, chunk.size, chunk.sha256.clone()),
            );
        }
    }
    let reject_legacy_collisions = journal.snapshot.logical_file_id.is_none();
    if total > 1 && (reject_legacy_collisions || verified.len() < total as usize) {
        let discovered = find_parts(client, peer, base, total, reject_legacy_collisions).await?;
        for (index, (id, size, hash)) in discovered {
            let owned = verified.get(&index);
            if !reject_legacy_collisions && owned.is_some() {
                continue;
            }
            let offset = (u64::from(index) - 1) * part_size;
            let expected = part_size.min(journal.snapshot.source.size - offset);
            let own_id = owned.is_some_and(|(own_id, _, _)| *own_id == id);
            if reject_legacy_collisions && (size != expected || (hash.is_none() && !own_id)) {
                return Err("A different or unverified multipart file already uses this filename. Choose another filename before uploading.".to_string());
            }
            if size != expected || (hash.is_none() && !own_id) {
                continue;
            }
            let local_hash = match owned.and_then(|(_, _, hash)| hash.clone()) {
                Some(hash) => Some(hash),
                None if hash.is_some() => {
                    Some(crate::commands::transfers::hash_file_range(path, offset, expected).await?)
                }
                None => None,
            };
            if reject_legacy_collisions {
                legacy_source_part_matches(
                    expected,
                    local_hash.as_deref(),
                    size,
                    hash.as_deref(),
                    own_id,
                )?;
            }
            if hash.is_some() && local_hash.as_deref() == hash.as_deref() {
                verified.entry(index).or_insert((id, size, hash));
            }
        }
    }
    // No remote documents are deleted merely because a different same-name
    // file or an interrupted transfer used the same multipart layout.
    Ok(verified)
}

pub fn split_part_name(base: &str, idx: u32, total: u32) -> String {
    format!("{}{}{:03}-{:03}", base, SPLIT_MARKER, idx, total)
}

/// Part caption, optionally carrying the part's SHA-256 for download-time
/// verification: "<base>.tgdpart<NNN>-<TTT>[#<sha256 hex>]".
pub fn split_part_caption(base: &str, idx: u32, total: u32, hash: Option<&str>) -> String {
    match hash {
        Some(h) => format!("{}#{}", split_part_name(base, idx, total), h),
        None => split_part_name(base, idx, total),
    }
}

/// Parses "<base>.tgdpart<NNN>-<TTT>[#<sha256 hex>]" into (base, idx, total, hash).
/// Strict: canonical decimal numbers padded to at least 3 digits, 1 <= idx
/// <= total; hash, when present, is exactly 64 lowercase hex chars.
pub fn parse_part_name(name: &str) -> Option<(&str, u32, u32, Option<&str>)> {
    let pos = name.rfind(SPLIT_MARKER)?;
    let suffix = &name[pos + SPLIT_MARKER.len()..];
    let (nums, hash) = match suffix.find('#') {
        Some(h_pos) => {
            let h = &suffix[h_pos + 1..];
            if h.len() != 64 || !h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return None;
            }
            (&suffix[..h_pos], Some(h))
        }
        None => (suffix, None),
    };
    let (idx_text, total_text) = nums.split_once('-')?;
    let parse_number = |text: &str| -> Option<u32> {
        if !(3..=10).contains(&text.len())
            || !text.bytes().all(|b| b.is_ascii_digit())
            || (text.len() > 3 && text.starts_with('0'))
        {
            return None;
        }
        text.parse().ok()
    };
    let idx = parse_number(idx_text)?;
    let total = parse_number(total_text)?;
    if idx == 0 || total == 0 || idx > total || name[..pos].is_empty() {
        return None;
    }
    Some((&name[..pos], idx, total, hash))
}

/// Async reader wrapper that tracks bytes read for progress reporting.
/// Reads a byte range of a file and adds consumed bytes to a shared counter,
/// so multiple readers (split parts) report cumulative progress. Optionally
/// hashes everything it yields, for the part checksum in the caption.
struct ProgressReader {
    inner: tokio::io::Take<tokio::io::BufReader<tokio::fs::File>>,
    bytes_read: std::sync::Arc<std::sync::atomic::AtomicU64>,
    hasher: Option<sha2::Sha256>,
}

impl ProgressReader {
    async fn new(
        path: &str,
    ) -> Result<(Self, u64, std::sync::Arc<std::sync::atomic::AtomicU64>), String> {
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|e| e.to_string())?
            .len();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reader = Self::new_range(path, 0, size, counter.clone(), false).await?;
        Ok((reader, size, counter))
    }

    /// Reader over bytes [offset, offset + len) of the file.
    async fn new_range(
        path: &str,
        offset: u64,
        len: u64,
        counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
        hash: bool,
    ) -> Result<Self, String> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| e.to_string())?;
        if offset > 0 {
            tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            inner: tokio::io::AsyncReadExt::take(tokio::io::BufReader::new(file), len),
            bytes_read: counter,
            hasher: hash.then(sha2::Sha256::default),
        })
    }

    /// SHA-256 hex of everything read so far; None when hashing was off.
    #[cfg_attr(not(test), allow(dead_code))]
    fn finalize_hash(&mut self) -> Option<String> {
        use sha2::Digest;
        self.hasher.take().map(|h| format!("{:x}", h.finalize()))
    }
}

impl tokio::io::AsyncRead for ProgressReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &result {
            let after = buf.filled().len();
            let delta = (after - before) as u64;
            self.bytes_read
                .fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
            if delta > 0 {
                if let Some(h) = self.hasher.as_mut() {
                    use sha2::Digest;
                    h.update(&buf.filled()[before..after]);
                }
            }
        }
        result
    }
}

/// Delete a partial file with retries (best-effort cleanup)

#[tauri::command]
pub async fn cmd_cancel_transfer(
    transfer_id: String,
    state: State<'_, TelegramState>,
) -> Result<bool, String> {
    log::info!("Cancelling transfer: {}", transfer_id);
    state
        .cancelled_transfers
        .write()
        .await
        .insert(transfer_id.clone());
    if let Some(tx) = get_upload_cancellations()
        .lock()
        .unwrap()
        .remove(&transfer_id)
    {
        let _ = tx.send(true);
    }
    Ok(true)
}

#[cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(unused_mut))]
#[tauri::command]
pub async fn cmd_upload_file(
    mut path: String,
    folder_id: Option<i64>,
    transfer_id: Option<String>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    tdlib_state: State<'_, crate::tdlib_fast::TdlibFastState>,
    db_pool: State<'_, DbConnection>,
    bw_state: State<'_, Arc<BandwidthManager>>,
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<String, String> {
    let mut temp_cache_path: Option<String> = None;

    #[cfg(target_os = "ios")]
    {
        // Tauri's iOS file dialog returns file:// URIs. The Telegram transfer
        // layer and std/tokio filesystem APIs require a normal filesystem path.
        path = clean_file_uri(&path);
    }

    // Strict JNI Interception Guard for Android URI Schemes
    #[cfg(target_os = "android")]
    {
        if path.contains("content://") || path.contains("msf:") || path.contains("msf%") {
            match copy_to_android_cache(&path) {
                Ok(cached_path) => {
                    log::info!(
                        "JNI STRICT GUARD: Intercepted raw URI. Overwriting path: {} -> {}",
                        path,
                        cached_path
                    );
                    temp_cache_path = Some(cached_path.clone());
                    path = cached_path;
                }
                Err(err) => {
                    return Err(format!(
                        "JNI STRICT GUARD FAILURE: Failed to copy raw URI {} to android cache: {}",
                        path, err
                    ));
                }
            }
        }
    }

    let result = cmd_upload_file_inner(
        path.clone(),
        folder_id,
        transfer_id,
        app_handle,
        state,
        tdlib_state,
        db_pool,
        bw_state,
        net_config,
    )
    .await;

    if let Some(ref cache_path) = temp_cache_path {
        let _ = tokio::fs::remove_file(cache_path).await;
        log::info!("Removed temporary upload cache file: {}", cache_path);
    }

    result
}

async fn cmd_upload_file_inner(
    path: String,
    folder_id: Option<i64>,
    transfer_id: Option<String>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    tdlib_state: State<'_, crate::tdlib_fast::TdlibFastState>,
    db_pool: State<'_, DbConnection>,
    bw_state: State<'_, Arc<BandwidthManager>>,
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<String, String> {
    let logical_channel_id = if let Some(backing_channel_id) = folder_id {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, folder_id)?;
        Some(
            crate::commands::logical_channels::logical_channel_id_for_backing(
                &conn,
                backing_channel_id,
            )?
            .ok_or_else(|| {
                "TeraRelay logical channel metadata is missing. Sync channels and retry."
                    .to_string()
            })?,
        )
    } else {
        None
    };
    let logical_file_id = logical_channel_id
        .as_ref()
        .map(|_| crate::commands::logical_files::new_file_id());

    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| e.to_string())?
        .len();
    bw_state.try_reserve_up(size)?;

    let tid = transfer_id.unwrap_or_default();

    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!("[MOCK] Uploaded file {} to {:?}", path, folder_id);
        bw_state.release_up(size);
        return Ok("Mock upload successful".to_string());
    }
    let client = client_opt.ok_or_else(|| {
        bw_state.release_up(size);
        "Client not connected".to_string()
    })?;

    // Emit start progress
    if !tid.is_empty() {
        let _ = app_handle.emit(
            "upload-progress",
            ProgressPayload {
                id: tid.clone(),
                percent: 0,
                uploaded_bytes: 0,
                total_bytes: size,
                speed_bytes_per_sec: 0,
            },
        );
    }

    let file_name = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let peer = match resolve_peer(&client, folder_id, &state.peer_cache).await {
        Ok(p) => p,
        Err(e) => {
            bw_state.release_up(size);
            return Err(e);
        }
    };

    // Multipart numbering is checked independently of file size. The
    // manifest preflight runs before any data upload or large allocation.
    let part_size = split_part_size().max(1);
    let total_parts = checked_upload_part_count(size, part_size).map_err(|error| {
        bw_state.release_up(size);
        error
    })?;
    crate::commands::logical_files::ensure_upload_manifest_capacity(
        logical_channel_id.as_deref().unwrap_or("saved-messages"),
        &file_name,
        size,
        total_parts,
    )
    .map_err(|error| {
        bw_state.release_up(size);
        error
    })?;

    let mut journal = crate::commands::transfers::UploadJournal::open(
        db_pool.inner().clone(),
        &path,
        &tid,
        folder_id,
        part_size,
        logical_file_id,
    )
    .await
    .map_err(|error| {
        bw_state.release_up(size);
        error
    })?;
    let logical_file_id = journal.snapshot.logical_file_id.clone();
    if journal.snapshot.source.size != size {
        bw_state.release_up(size);
        return Err(
            "Upload source changed since this transfer was saved. Select the file as a new upload."
                .to_string(),
        );
    }

    // Locally journaled parts are tied to this unchanged source. Legacy
    // caption checksums must match the actual local range before reuse.
    let existing_parts = verified_upload_parts(
        &client,
        &peer,
        &path,
        &file_name,
        total_parts,
        part_size,
        &journal,
    )
    .await
    .map_err(|error| {
        bw_state.release_up(size);
        error
    })?;
    let (upload_indices, _, done_bytes) =
        parts_to_upload(size, part_size, total_parts, &existing_parts);
    let mut manifest_chunks: std::collections::BTreeMap<
        u32,
        crate::commands::logical_files::ManifestChunkV1,
    > = existing_parts
        .iter()
        .map(|(index, (message_id, chunk_size, hash))| {
            (
                *index,
                crate::commands::logical_files::ManifestChunkV1 {
                    index: *index,
                    message_id: i64::from(*message_id),
                    size: *chunk_size,
                    sha256: hash.clone(),
                },
            )
        })
        .collect();
    journal
        .replace_chunks(manifest_chunks.clone())
        .map_err(|error| {
            bw_state.release_up(size);
            error
        })?;
    journal.verify_source(&path).await.map_err(|error| {
        bw_state.release_up(size);
        error
    })?;
    bw_state.release_up(done_bytes);

    // Authoritative file-byte counter shared by every upload chunk. It starts
    // at bytes already verified on Telegram by resume. TDLib progress updates
    // come from getFile/updateFile, never from protocol/network overhead.
    let file_size = size;
    let bytes_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(done_bytes));
    // Separate live network byte counter for throughput only. On Linux TDLib
    // this advances from getNetworkStatistics; fallback transports continue to
    // derive speed from their authoritative file-byte counter.
    let network_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let use_tdlib_network_speed = cfg!(all(target_os = "linux", target_arch = "x86_64"));

    let cancelled = state.cancelled_transfers.clone();
    let progress_tid = tid.clone();
    let progress_handle = app_handle.clone();
    let progress_counter = bytes_counter.clone();
    let progress_network_counter = network_counter.clone();
    let progress_task = if !tid.is_empty() {
        Some(tokio::spawn(async move {
            let mut last_speed_bytes: u64 = if use_tdlib_network_speed {
                progress_network_counter.load(std::sync::atomic::Ordering::Relaxed)
            } else {
                progress_counter.load(std::sync::atomic::Ordering::Relaxed)
            };
            let mut last_time = std::time::Instant::now();
            let mut speed_window = SpeedWindow::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let current = progress_counter.load(std::sync::atomic::Ordering::Relaxed);
                let speed_bytes = if use_tdlib_network_speed {
                    progress_network_counter.load(std::sync::atomic::Ordering::Relaxed)
                } else {
                    current
                };
                let now = std::time::Instant::now();
                let dt = now.duration_since(last_time).as_secs_f64();
                let transferred = speed_bytes.saturating_sub(last_speed_bytes);
                let speed = speed_window.update(transferred, dt);
                let percent = if file_size > 0 {
                    ((current as f64 / file_size as f64) * 100.0).min(99.0) as u8
                } else {
                    0
                };

                let _ = progress_handle.emit(
                    "upload-progress",
                    ProgressPayload {
                        id: progress_tid.clone(),
                        percent,
                        uploaded_bytes: current,
                        total_bytes: file_size,
                        speed_bytes_per_sec: speed,
                    },
                );

                last_speed_bytes = speed_bytes;
                last_time = now;

                if current >= file_size {
                    break;
                }
                // Check cancellation
                if cancelled.read().await.contains(&progress_tid) {
                    break;
                }
            }
        }))
    } else {
        None
    };

    // One cancellation channel for the whole transfer; every part task
    // subscribes and cmd_cancel_transfer flips it to true.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut _local_cancel_tx = None;
    if !tid.is_empty() {
        get_upload_cancellations()
            .lock()
            .unwrap()
            .insert(tid.clone(), cancel_tx);
    } else {
        // Keep the sender alive for the whole transfer, otherwise receivers
        // would observe the drop and cancel immediately
        _local_cancel_tx = Some(cancel_tx);
    }

    // Cancel requested before the watch channel was registered
    if state.cancelled_transfers.read().await.contains(&tid) {
        state.cancelled_transfers.write().await.remove(&tid);
        if let Some(t) = progress_task {
            t.abort();
        }
        if !tid.is_empty() {
            get_upload_cancellations().lock().unwrap().remove(&tid);
        }
        bw_state.release_up(size.saturating_sub(done_bytes));
        return Err("Transfer cancelled".to_string());
    }

    let tdlib_required = cfg!(all(target_os = "linux", target_arch = "x86_64"));

    // On supported desktop Linux builds, local uploads use the native
    // TDLib/C++ data plane. We deliberately do not
    // silently fall back to the slower MTProto uploader here: if the one-time
    // TDLib authorization has not been completed yet, the frontend opens the
    // setup flow and retries this exact queued upload afterwards.
    if tdlib_required && !crate::tdlib_fast::tdlib_is_ready(tdlib_state.inner()) {
        if let Some(task) = progress_task.as_ref() {
            task.abort();
        }
        if !tid.is_empty() {
            get_upload_cancellations().lock().unwrap().remove(&tid);
        }
        bw_state.release_up(size.saturating_sub(done_bytes));
        return Err("TDLIB_SETUP_REQUIRED".to_string());
    }

    let upload_result: Result<(), String> = if tdlib_required {
        log::info!("Upload transport: TDLib/C++ native");

        let destination = match folder_id {
            Some(id) => crate::tdlib_fast::FastDestination::Channel(id),
            None => crate::tdlib_fast::FastDestination::SavedMessages,
        };

        async {
            // Normal files keep the direct TDLib path with no preparation.
            if total_parts == 1 {
                for idx in upload_indices {
                    if *cancel_rx.borrow() {
                        return Err("Transfer cancelled".to_string());
                    }

                    if !tid.is_empty() {
                        let _ = app_handle.emit(
                            "upload-phase",
                            serde_json::json!({"id": tid.clone(), "phase": "uploading"}),
                        );
                    }

                    journal.verify_source(&path).await?;
                    let message_id = crate::tdlib_fast::upload_document(
                        tdlib_state.inner(),
                        destination,
                        path.clone(),
                        String::new(),
                        bytes_counter.clone(),
                        network_counter.clone(),
                        cancel_rx.clone(),
                    )
                    .await?;

                    journal.verify_source(&path).await?;
                    let chunk = crate::commands::logical_files::ManifestChunkV1 {
                        index: idx,
                        message_id,
                        size,
                        sha256: None,
                    };
                    journal.record(chunk.clone())?;
                    manifest_chunks.insert(idx, chunk);
                }
                return Ok(());
            }

            // Large logical files keep exactly one prepared part ahead. The
            // first part is prepared/hashed before upload; while TDLib uploads
            // it, the next part is copied+hashed concurrently. This removes the
            // prepare/upload/prepare gap without changing the split format.
            let expected_source = journal.snapshot.source.clone();
            let prepare_part = |idx: u32| {
                let app_handle = app_handle.clone();
                let source_path = path.clone();
                let logical_name = file_name.clone();
                let expected_source = expected_source.clone();
                async move {
                    crate::commands::transfers::verify_source(&source_path, &expected_source)
                        .await?;
                    let offset = (idx as u64 - 1) * part_size;
                    let len = part_size.min(size - offset);
                    let doc_name = split_part_name(&logical_name, idx, total_parts as u32);
                    let (temp_path, hash) = crate::tdlib_fast::create_split_temp(
                        &app_handle,
                        &source_path,
                        offset,
                        len,
                        &doc_name,
                    )
                    .await?;
                    if let Err(error) =
                        crate::commands::transfers::verify_source(&source_path, &expected_source)
                            .await
                    {
                        let parent = temp_path.parent().map(|p| p.to_path_buf());
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        if let Some(parent) = parent {
                            let _ = tokio::fs::remove_dir(parent).await;
                        }
                        return Err(error);
                    }
                    let caption = format!("{}#{}", doc_name, hash);
                    Ok::<_, String>((idx, len, temp_path, caption, hash))
                }
            };

            let cleanup_temp = |temp_path: std::path::PathBuf| async move {
                let parent = temp_path.parent().map(|p| p.to_path_buf());
                let _ = tokio::fs::remove_file(&temp_path).await;
                if let Some(parent) = parent {
                    let _ = tokio::fs::remove_dir(&parent).await;
                }
            };

            let mut indices = upload_indices.into_iter();
            let Some(first_idx) = indices.next() else {
                return Ok(());
            };

            if !tid.is_empty() {
                let _ = app_handle.emit(
                    "upload-phase",
                    serde_json::json!({"id": tid.clone(), "phase": "preparing"}),
                );
            }
            let mut current = prepare_part(first_idx).await?;

            loop {
                let (idx, len, temp_path, caption, hash) = current;

                if *cancel_rx.borrow() {
                    cleanup_temp(temp_path).await;
                    return Err("Transfer cancelled".to_string());
                }

                if !tid.is_empty() {
                    let _ = app_handle.emit(
                        "upload-phase",
                        serde_json::json!({"id": tid.clone(), "phase": "uploading"}),
                    );
                }

                let upload_path = temp_path.to_string_lossy().to_string();

                if let Some(next_idx) = indices.next() {
                    let upload_future = crate::tdlib_fast::upload_document(
                        tdlib_state.inner(),
                        destination,
                        upload_path,
                        caption,
                        bytes_counter.clone(),
                        network_counter.clone(),
                        cancel_rx.clone(),
                    );
                    let prepare_future = prepare_part(next_idx);

                    let (upload_result, next_result) = tokio::join!(upload_future, prepare_future);

                    cleanup_temp(temp_path).await;

                    let message_id = match upload_result {
                        Ok(message_id) => message_id,
                        Err(error) => {
                            if let Ok((_, _, next_temp, _, _)) = next_result {
                                cleanup_temp(next_temp).await;
                            }
                            return Err(error);
                        }
                    };

                    let chunk = crate::commands::logical_files::ManifestChunkV1 {
                        index: idx,
                        message_id,
                        size: len,
                        sha256: Some(hash),
                    };
                    if let Err(error) = journal.record(chunk.clone()) {
                        if let Ok((_, _, next_temp, _, _)) = next_result {
                            cleanup_temp(next_temp).await;
                        }
                        return Err(error);
                    }
                    manifest_chunks.insert(idx, chunk);
                    current = next_result?;
                    continue;
                }

                let result = crate::tdlib_fast::upload_document(
                    tdlib_state.inner(),
                    destination,
                    upload_path,
                    caption,
                    bytes_counter.clone(),
                    network_counter.clone(),
                    cancel_rx.clone(),
                )
                .await;

                cleanup_temp(temp_path).await;

                let message_id = result?;
                let chunk = crate::commands::logical_files::ManifestChunkV1 {
                    index: idx,
                    message_id,
                    size: len,
                    sha256: Some(hash),
                };
                journal.record(chunk.clone())?;
                manifest_chunks.insert(idx, chunk);
                break;
            }

            Ok(())
        }
        .await
    } else {
        // Preserve the existing MTProto implementation for platforms where the
        // isolated TDLib worker is not supported yet. Linux desktop never uses
        // this path once TDLib support is available.
        let lane_pool = match build_upload_lane_pool(&client, &state, &net_config).await {
            Ok(pool) => pool,
            Err(error) => {
                log::warn!(
                    "Extra MTProto upload lanes unavailable; using the existing Telegram session: {}",
                    error
                );
                UploadLanePool {
                    lanes: vec![UploadLane::Main {
                        client: client.clone(),
                        lock: None,
                    }],
                    mode: "main-single",
                    initial_active_lanes: 1,
                    worker_window: UPLOAD_MAIN_SINGLE_WORKERS,
                }
            }
        };
        log::info!(
            "Upload fallback transport: mode={}, active_lanes={}, available_lanes={}, worker_window={}",
            lane_pool.mode,
            lane_pool
                .initial_active_lanes
                .min(lane_pool.lanes.len())
                .max(1),
            lane_pool.lanes.len(),
            lane_pool.worker_window
        );

        async {
            for idx in upload_indices {
                if *cancel_rx.borrow() {
                    return Err("Transfer cancelled".to_string());
                }

                let offset = (idx as u64 - 1) * part_size;
                let len = part_size.min(size - offset);
                let (doc_name, base_caption) = if total_parts == 1 {
                    (file_name.clone(), String::new())
                } else {
                    let part_name = split_part_name(&file_name, idx, total_parts as u32);
                    (part_name.clone(), part_name)
                };

                let send_name = doc_name.clone();
                let (uploaded_file, hash) = upload_range_fast(
                    &lane_pool,
                    &path,
                    offset,
                    len,
                    doc_name,
                    bytes_counter.clone(),
                    cancel_rx.clone(),
                    &net_config,
                    total_parts > 1,
                )
                .await?;

                let chunk_hash = hash.clone();
                let caption = match hash.as_deref() {
                    Some(hash) if !base_caption.is_empty() => {
                        format!("{}#{}", base_caption, hash)
                    }
                    _ => base_caption,
                };

                let message_id = send_uploaded_part(
                    &client,
                    &net_config,
                    uploaded_file,
                    send_name,
                    caption,
                    &peer,
                )
                .await?;
                journal.verify_source(&path).await?;
                let chunk = crate::commands::logical_files::ManifestChunkV1 {
                    index: idx,
                    message_id,
                    size: len,
                    sha256: chunk_hash,
                };
                journal.record(chunk.clone())?;
                manifest_chunks.insert(idx, chunk);
            }
            Ok(())
        }
        .await
    };

    // Stop progress reporter and drop the cancellation entry
    if let Some(t) = progress_task {
        t.abort();
    }
    if !tid.is_empty() {
        get_upload_cancellations().lock().unwrap().remove(&tid);
    }

    if let Err(err) = upload_result {
        log::error!(
            "Upload failed: path={}, folder_id={:?}, transport={}, error={}",
            path,
            folder_id,
            if tdlib_required {
                "TDLib/C++"
            } else {
                "MTProto"
            },
            err
        );
        if err == "Transfer cancelled" {
            state.cancelled_transfers.write().await.remove(&tid);
        }
        // Uploaded parts are intentionally kept: re-uploading the same file
        // resumes from them instead of starting over
        bw_state.release_up(size.saturating_sub(done_bytes));
        return Err(err);
    }

    journal.verify_source(&path).await.map_err(|error| {
        bw_state.release_up(size.saturating_sub(done_bytes));
        error
    })?;
    let mut published_manifest_id = None;

    // TeraRelay channels publish one small hidden manifest after all physical
    // Telegram documents are present. The manifest is the cross-device source
    // of truth for the logical filename, size, chunk order, IDs and checksums.
    if let (Some(logical_channel_id), Some(file_id)) =
        (logical_channel_id.as_ref(), logical_file_id.as_ref())
    {
        if manifest_chunks.len() != total_parts as usize {
            return Err(format!(
                "Upload data completed but the logical manifest is incomplete: expected {} chunks, recorded {}. Retry the upload; existing chunks will be reused.",
                total_parts,
                manifest_chunks.len()
            ));
        }

        let chunks: Vec<_> = manifest_chunks.into_values().collect();
        let manifest = crate::commands::logical_files::LogicalFileManifestV1 {
            schema_version: crate::commands::logical_files::MANIFEST_SCHEMA_VERSION,
            file_id: file_id.clone(),
            logical_channel_id: logical_channel_id.clone(),
            original_name: file_name.clone(),
            total_size: size,
            mime_type: mime_guess::from_path(&file_name)
                .first()
                .map(|mime| mime.essence_str().to_string()),
            created_at: chrono::Utc::now().timestamp(),
            whole_sha256: None,
            chunks,
        };

        // A crash after publication but before queue removal must recover the
        // same logical file rather than publish a second logical file ID.
        let existing_manifest = if journal.resumed {
            matching_upload_manifest(
                &client,
                &peer,
                &manifest,
                journal.snapshot.manifest_message_id,
            )
            .await?
        } else {
            None
        };
        published_manifest_id = Some(match existing_manifest {
            Some(id) => id,
            None => upload_logical_manifest_document(
                &client, &app_handle, &manifest, &peer,
            ).await.map_err(|error| {
                format!(
                    "File data is stored, but TeraRelay could not publish its logical manifest: {error}. Retry the upload to recover without re-uploading valid chunks."
                )
            })?,
        });

        let persist_result = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::logical_files::persist_manifest(&conn, &manifest)
        };
        if let Err(error) = persist_result {
            // The remote manifest is authoritative and can rebuild this cache
            // on the next listing/restart, so do not turn a completed upload
            // into a duplicate-producing retry.
            log::warn!(
                "Remote TeraRelay manifest was published but local cache update failed: {}",
                error
            );
        }
    }

    journal.complete(published_manifest_id)?;

    // Bandwidth was already reserved by try_reserve_up at start
    if !tid.is_empty() {
        let _ = app_handle.emit(
            "upload-progress",
            ProgressPayload {
                id: tid,
                percent: 100,
                uploaded_bytes: size,
                total_bytes: size,
                speed_bytes_per_sec: 0,
            },
        );
    }
    Ok("File uploaded successfully".to_string())
}

fn message_id_from_update(update: &tl::enums::Update, random_id: i64) -> Option<i32> {
    match update {
        tl::enums::Update::MessageId(value) if value.random_id == random_id => Some(value.id),
        tl::enums::Update::NewMessage(value) => Some(value.message.id()),
        tl::enums::Update::NewChannelMessage(value) => Some(value.message.id()),
        _ => None,
    }
}

fn sent_message_id(updates: &tl::enums::Updates, random_id: i64) -> Option<i32> {
    match updates {
        tl::enums::Updates::UpdateShortSentMessage(value) => Some(value.id),
        tl::enums::Updates::UpdateShortMessage(value) => Some(value.id),
        tl::enums::Updates::UpdateShortChatMessage(value) => Some(value.id),
        tl::enums::Updates::UpdateShort(value) => message_id_from_update(&value.update, random_id),
        tl::enums::Updates::Combined(value) => value
            .updates
            .iter()
            .find_map(|update| match update {
                tl::enums::Update::MessageId(id) if id.random_id == random_id => Some(id.id),
                _ => None,
            })
            .or_else(|| {
                value
                    .updates
                    .iter()
                    .find_map(|update| message_id_from_update(update, random_id))
            }),
        tl::enums::Updates::Updates(value) => value
            .updates
            .iter()
            .find_map(|update| match update {
                tl::enums::Update::MessageId(id) if id.random_id == random_id => Some(id.id),
                _ => None,
            })
            .or_else(|| {
                value
                    .updates
                    .iter()
                    .find_map(|update| message_id_from_update(update, random_id))
            }),
        tl::enums::Updates::TooLong => None,
    }
}

/// Sends a previously uploaded raw Telegram file as a document and returns the
/// exact Telegram message ID assigned to that storage object.
async fn send_uploaded_part(
    client: &grammers_client::Client,
    net_config: &NetworkConfig,
    uploaded_file: tl::enums::InputFile,
    file_name: String,
    caption: String,
    peer: &Peer,
) -> Result<i64, String> {
    let input_peer = match peer {
        Peer::User(u) => {
            let (id, access_hash) = match &u.raw {
                tl::enums::User::User(user) => (user.id, user.access_hash.unwrap_or(0)),
                tl::enums::User::Empty(user) => (user.id, 0),
            };
            tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: id,
                access_hash,
            })
        }
        Peer::Channel(channel) => tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: channel.raw.id,
            access_hash: channel
                .raw
                .access_hash
                .ok_or_else(|| "No access hash for Telegram channel".to_string())?,
        }),
        _ => return Err("Unsupported Telegram upload destination".to_string()),
    };

    let mime_type = mime_guess::from_path(&file_name)
        .first()
        .map(|mime| mime.essence_str().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let media: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        force_file: true,
        spoiler: false,
        file: uploaded_file,
        thumb: None,
        mime_type,
        attributes: vec![tl::types::DocumentAttributeFilename {
            file_name: file_name.clone(),
        }
        .into()],
        stickers: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    }
    .into();

    let max_retries = net_config.retry_attempts();
    let base_ms = net_config.retry_base_backoff_ms();
    let max_ms = net_config.retry_max_backoff_ms();
    let respect_flood = net_config.should_respect_flood_wait();
    let random_id = rand::random::<i64>();
    let mut last_err = String::new();

    for attempt in 0..=max_retries {
        let request = tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: input_peer.clone(),
            reply_to: None,
            media: media.clone(),
            message: caption.clone(),
            random_id,
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
        };

        match client.invoke(&request).await {
            Ok(updates) => {
                if let Some(message_id) = sent_message_id(&updates, random_id) {
                    return Ok(message_id as i64);
                }

                // A successful send must never be retried merely because a rare
                // Updates shape omitted updateMessageID. Resolve the just-sent
                // object from the latest messages instead, preventing duplicates.
                let expected_caption = caption.as_str();
                let expected_name = file_name.as_str();
                let mut recent = client.iter_messages(peer).limit(12);
                while let Some(message) = recent.next().await.map_err(|e| e.to_string())? {
                    let display = message_display_name(&message);
                    let matches = if expected_caption.is_empty() {
                        match message.media() {
                            Some(Media::Document(document)) => document.name() == expected_name,
                            _ => false,
                        }
                    } else {
                        display == expected_caption
                    };
                    if matches {
                        return Ok(message.id() as i64);
                    }
                }
                return Err(
                    "Telegram accepted the document but TeraRelay could not resolve its message ID"
                        .to_string(),
                );
            }
            Err(e) => {
                let err = map_error(e);
                log::warn!(
                    "send uploaded document attempt {}/{}: {}",
                    attempt + 1,
                    max_retries + 1,
                    err
                );

                if respect_flood && err.starts_with("FLOOD_WAIT_") {
                    if let Ok(secs) = err.trim_start_matches("FLOOD_WAIT_").parse::<u64>() {
                        let wait = secs.min(300);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        last_err = err;
                        continue;
                    }
                }

                last_err = err;
                if attempt < max_retries {
                    let delay = backoff_ms(attempt, base_ms, max_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }

    Err(format!(
        "Upload send failed after {} attempts: {}",
        max_retries + 1,
        last_err
    ))
}

async fn find_logical_manifest_message_ids(
    client: &grammers_client::Client,
    peer: &Peer,
    file_id: &str,
) -> Result<Vec<i32>, String> {
    let mut ids = Vec::new();
    let mut messages = client.iter_messages(peer);
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        let Some(Media::Document(document)) = message.media() else {
            continue;
        };
        if crate::commands::logical_files::manifest_file_id(document.name(), message.text())
            .as_deref()
            == Some(file_id)
        {
            ids.push(message.id());
        }
    }
    Ok(ids)
}

async fn matching_upload_manifest(
    client: &grammers_client::Client,
    peer: &Peer,
    manifest: &crate::commands::logical_files::LogicalFileManifestV1,
    known_message_id: Option<i64>,
) -> Result<Option<i64>, String> {
    let ids = match known_message_id {
        Some(id) => {
            vec![i32::try_from(id).map_err(|_| "Invalid saved manifest message ID".to_string())?]
        }
        None => find_logical_manifest_message_ids(client, peer, &manifest.file_id).await?,
    };
    let messages = get_messages_in_batches(client, peer, &ids).await?;
    for message in messages.into_iter().flatten() {
        let Some(media @ Media::Document(_)) = message.media() else {
            continue;
        };
        let mut existing = crate::commands::logical_files::download_manifest(
            client,
            &media,
            Some(&manifest.file_id),
            Some(&manifest.logical_channel_id),
        )
        .await?;
        // Retry time is not file identity. All data, order and checksum fields
        // must still agree before an already-published manifest is accepted.
        existing.created_at = manifest.created_at;
        if &existing == manifest {
            return Ok(Some(i64::from(message.id())));
        }
    }
    Ok(None)
}

async fn upload_logical_manifest_document(
    client: &grammers_client::Client,
    app_handle: &tauri::AppHandle,
    manifest: &crate::commands::logical_files::LogicalFileManifestV1,
    peer: &Peer,
) -> Result<i64, String> {
    let (temp_path, manifest_size) =
        crate::commands::logical_files::write_manifest_temp(app_handle, manifest).await?;
    let file_name = crate::commands::logical_files::manifest_file_name(&manifest.file_id);
    let caption = crate::commands::logical_files::manifest_caption(&manifest.file_id);

    let result = async {
        let mut file = tokio::fs::File::open(&temp_path)
            .await
            .map_err(|e| format!("Failed to open staged TeraRelay manifest: {e}"))?;
        let uploaded = client
            .upload_stream(&mut file, manifest_size as usize, file_name)
            .await
            .map_err(map_error)?;
        let message = InputMessage::new().text(caption).file(uploaded);
        let sent = client
            .send_message(peer, message)
            .await
            .map_err(map_error)?;
        Ok(sent.id() as i64)
    }
    .await;

    let _ = tokio::fs::remove_file(&temp_path).await;
    result
}

/// Display name of a message: caption if set (rename mechanism), else the
/// document's filename attribute. Mirrors the listing logic in cmd_get_files.
fn message_display_name(msg: &grammers_client::types::Message) -> String {
    let caption = msg.text();
    if !caption.is_empty() {
        return caption.to_string();
    }
    match msg.media() {
        Some(Media::Document(d)) => d.name().to_string(),
        _ => String::new(),
    }
}

/// Scans the chat for part messages matching (base, total).
/// Returns part index -> (message id, document size, optional SHA-256).
/// Same O(n) cost as the folder listing.
fn legacy_source_part_matches(
    expected_size: u64,
    expected_hash: Option<&str>,
    actual_size: u64,
    actual_hash: Option<&str>,
    owned: bool,
) -> Result<(), String> {
    let hash_agrees = match (expected_hash, actual_hash) {
        (Some(a), Some(b)) => a == b,
        (_, None) => owned,
        _ => false,
    };
    if expected_size == actual_size && hash_agrees {
        Ok(())
    } else {
        Err("A different or unverified multipart file already uses this filename. Choose another filename before uploading.".to_string())
    }
}

fn insert_part_candidate(
    found: &mut HashMap<u32, (i32, u64, Option<String>)>,
    index: u32,
    candidate: (i32, u64, Option<String>),
    reject_conflicts: bool,
) -> Result<(), String> {
    if let Some(existing) = found.get(&index) {
        let same_data = existing.1 == candidate.1
            && match (&existing.2, &candidate.2) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
        if reject_conflicts && existing.0 != candidate.0 && !same_data {
            return Err(format!(
                "Multipart part {index} has conflicting versions with the same filename. Cannot safely combine these parts; use the original logical file or upload with a different filename."
            ));
        }
    } else {
        found.insert(index, candidate);
    }
    Ok(())
}

async fn find_parts(
    client: &grammers_client::Client,
    peer: &Peer,
    base: &str,
    total: u32,
    reject_conflicts: bool,
) -> Result<HashMap<u32, (i32, u64, Option<String>)>, String> {
    let mut found: HashMap<u32, (i32, u64, Option<String>)> = HashMap::new();
    let mut msgs = client.iter_messages(peer);
    while let Some(m) = msgs.next().await.map_err(|e| e.to_string())? {
        let doc_size = match m.media() {
            Some(Media::Document(d)) => d.size() as u64,
            _ => continue,
        };
        if let Some((b, i, t, hash)) = parse_part_name(&message_display_name(&m)) {
            if b == base && t == total {
                insert_part_candidate(
                    &mut found,
                    i,
                    (m.id(), doc_size, hash.map(str::to_string)),
                    reject_conflicts,
                )?;
                // Explicit manifest uploads can select the newest candidates.
                // Legacy reconstruction must scan older duplicates as well.
                if !reject_conflicts && found.len() == total as usize {
                    break;
                }
            }
        }
    }
    Ok(found)
}

/// Decides which parts still need uploading given what already exists on
/// Telegram (upload resume). A part is reusable when its size matches the
/// expected size exactly; wrong-size parts are deleted and re-uploaded.
/// Returns (part indices to upload, message ids to delete, bytes already done).
/// ponytail: same name + same part layout => same file; a remote part can't
/// be re-hashed without downloading it, so size is the resume criterion.
fn parts_to_upload(
    size: u64,
    part_size: u64,
    total: u32,
    existing: &HashMap<u32, (i32, u64, Option<String>)>,
) -> (Vec<u32>, Vec<i32>, u64) {
    let mut to_upload = Vec::new();
    let mut to_delete = Vec::new();
    let mut done_bytes: u64 = 0;
    for idx in 1..=total {
        let expected = part_size.min(size - (idx as u64 - 1) * part_size);
        match existing.get(&idx) {
            Some((_, sz, _)) if *sz == expected => done_bytes += expected,
            Some((id, _, _)) => {
                to_delete.push(*id);
                to_upload.push(idx);
            }
            None => to_upload.push(idx),
        }
    }
    (to_upload, to_delete, done_bytes)
}

/// Resolves a message id to the ordered message ids making up the file:
/// [message_id] for regular files, all sibling ".tgdpart" messages (sorted by
/// part index) for split files. With require_complete, errors if a part is
/// missing; otherwise returns whatever parts exist (used by delete).
fn ordered_part_ids(
    base: &str,
    total: u32,
    found: &HashMap<u32, (i32, u64, Option<String>)>,
    require_complete: bool,
) -> Result<Vec<i32>, String> {
    // Allocate and iterate only actual messages, never an untrusted caption's
    // declared total (which can now contain more than three digits).
    let mut parts: Vec<_> = found.iter().collect();
    parts.sort_unstable_by_key(|(index, _)| **index);
    if require_complete {
        for (position, (index, _)) in parts.iter().enumerate() {
            let expected = position as u64 + 1;
            if u64::from(**index) != expected {
                return Err(format!(
                    "Split file '{}': part {}/{} is missing",
                    base, expected, total
                ));
            }
        }
        if parts.len() as u64 != u64::from(total) {
            return Err(format!(
                "Split file '{}': part {}/{} is missing",
                base,
                parts.len() as u64 + 1,
                total
            ));
        }
    }
    Ok(parts.into_iter().map(|(_, (id, _, _))| *id).collect())
}

pub(crate) async fn resolve_parts(
    client: &grammers_client::Client,
    peer: &Peer,
    message_id: i32,
    require_complete: bool,
) -> Result<Vec<i32>, String> {
    let messages = client
        .get_messages_by_id(peer, &[message_id])
        .await
        .map_err(|e| e.to_string())?;
    let msg = messages
        .into_iter()
        .flatten()
        .next()
        .ok_or_else(|| "Message not found".to_string())?;
    let name = message_display_name(&msg);
    let (base, _, total, _) = match parse_part_name(&name) {
        Some(p) => p,
        None => return Ok(vec![message_id]),
    };

    let found = find_parts(client, peer, base, total, true).await?;

    ordered_part_ids(base, total, &found, require_complete)
}

#[tauri::command]
pub async fn initiate_upload(
    path: String,
    folder_id: Option<i64>,
    transfer_id: Option<String>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    tdlib_state: State<'_, crate::tdlib_fast::TdlibFastState>,
    db_pool: State<'_, DbConnection>,
    bw_state: State<'_, Arc<BandwidthManager>>,
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<String, String> {
    crate::upload_service::start_foreground_service();
    cmd_upload_file(
        path,
        folder_id,
        transfer_id,
        app_handle,
        state,
        tdlib_state,
        db_pool,
        bw_state,
        net_config,
    )
    .await
}

#[tauri::command]
pub async fn cmd_rename_file(
    message_id: i32,
    folder_id: Option<i64>,
    new_name: String,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let logical_manifest = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, folder_id)?;
        if let Some(backing_channel_id) = folder_id {
            match crate::commands::logical_channels::logical_channel_id_for_backing(
                &conn,
                backing_channel_id,
            )? {
                Some(logical_channel_id) => {
                    crate::commands::logical_files::cached_manifest_for_first_message(
                        &conn,
                        &logical_channel_id,
                        message_id as i64,
                    )?
                }
                None => None,
            }
        } else {
            None
        }
    };

    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!("[MOCK] Renamed message {} to {}", message_id, new_name);
        return Ok(true);
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;

    if let Some(mut manifest) = logical_manifest {
        let old_manifest_ids =
            find_logical_manifest_message_ids(&client, &peer, &manifest.file_id).await?;
        manifest.original_name = new_name;

        let new_manifest_message_id =
            upload_logical_manifest_document(&client, &app_handle, &manifest, &peer).await?;

        let stale_manifest_ids: Vec<i32> = old_manifest_ids
            .into_iter()
            .filter(|id| *id != new_manifest_message_id as i32)
            .collect();
        if !stale_manifest_ids.is_empty() {
            if let Err(error) = client.delete_messages(&peer, &stale_manifest_ids).await {
                // The newest manifest is authoritative and listing ignores older
                // duplicates after seeing it, so cleanup failure is recoverable.
                log::warn!(
                    "Logical rename published the new manifest but could not remove {} stale manifest(s): {}",
                    stale_manifest_ids.len(),
                    error
                );
            }
        }

        let persist_result = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::logical_files::persist_manifest(&conn, &manifest)
        };
        if let Err(error) = persist_result {
            log::warn!(
                "Logical rename succeeded remotely but local manifest cache update failed: {}",
                error
            );
        }
        return Ok(true);
    }

    // Verify the message exists before attempting to edit it.
    // This avoids a cryptic MESSAGE_ID_INVALID RPC error when the message
    // was moved (forwarded → new ID) or deleted since the file list was loaded.
    let messages = client
        .get_messages_by_id(&peer, &[message_id])
        .await
        .map_err(|e| format!("Failed to fetch message for rename: {}", e))?;
    let msg = match messages.iter().flatten().next() {
        Some(m) => m,
        None => return Err(format!(
            "Message {} not found in folder {:?}. The file may have been moved or deleted. Please refresh the folder.",
            message_id, folder_id
        )),
    };

    // Split files: rewrite every part's caption, preserving the part suffix
    // and its checksum
    let edits: Vec<(i32, String)> = if parse_part_name(&message_display_name(msg)).is_some() {
        let ids = resolve_parts(&client, &peer, message_id, true).await?;
        let part_msgs = client
            .get_messages_by_id(&peer, &ids)
            .await
            .map_err(|e| e.to_string())?;
        let mut edits = Vec::with_capacity(ids.len());
        for m in part_msgs.iter().flatten() {
            let name = message_display_name(m);
            let (_, idx, total, hash) = parse_part_name(&name).ok_or_else(|| {
                "A part of this file lost its name. Please refresh the folder.".to_string()
            })?;
            edits.push((m.id(), split_part_caption(&new_name, idx, total, hash)));
        }
        edits
    } else {
        vec![(message_id, new_name)]
    };

    let input_peer = match &peer {
        Peer::User(u) => {
            let (id, access_hash) = match &u.raw {
                tl::enums::User::User(usr) => (usr.id, usr.access_hash.unwrap_or(0)),
                tl::enums::User::Empty(usr) => (usr.id, 0),
            };
            tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: id,
                access_hash,
            })
        }
        Peer::Channel(c) => tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: c.raw.id,
            access_hash: c.raw.access_hash.ok_or("No access hash for channel")?,
        }),
        _ => return Err("Unsupported peer type".to_string()),
    };

    for (id, caption) in edits {
        client
            .invoke(&tl::functions::messages::EditMessage {
                peer: input_peer.clone(),
                id,
                no_webpage: false,
                invert_media: false,
                message: Some(caption),
                media: None,
                reply_markup: None,
                entities: None,
                schedule_date: None,
                quick_reply_shortcut_id: None,
                schedule_repeat_period: None,
            })
            .await
            .map_err(|e| format!("Failed to rename file: {}", e))?;
    }

    Ok(true)
}

#[tauri::command]
pub async fn cmd_delete_file(
    message_id: i32,
    folder_id: Option<i64>,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let logical_manifest = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, folder_id)?;
        if let Some(backing_channel_id) = folder_id {
            match crate::commands::logical_channels::logical_channel_id_for_backing(
                &conn,
                backing_channel_id,
            )? {
                Some(logical_channel_id) => {
                    crate::commands::logical_files::cached_manifest_for_first_message(
                        &conn,
                        &logical_channel_id,
                        message_id as i64,
                    )?
                }
                None => None,
            }
        } else {
            None
        }
    };

    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!(
            "[MOCK] Deleted message {} from folder {:?}",
            message_id,
            folder_id
        );
        return Ok(true);
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;

    if let Some(manifest) = logical_manifest {
        let mut delete_ids: Vec<i32> = manifest
            .chunks
            .iter()
            .map(|chunk| chunk.message_id as i32)
            .collect();
        delete_ids
            .extend(find_logical_manifest_message_ids(&client, &peer, &manifest.file_id).await?);
        delete_ids.sort_unstable();
        delete_ids.dedup();

        if !delete_ids.is_empty() {
            client
                .delete_messages(&peer, &delete_ids)
                .await
                .map_err(|e| format!("Failed to delete logical file storage: {e}"))?;
        }

        let cache_result = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            crate::commands::logical_files::delete_cached_file(&conn, &manifest.file_id)
        };
        if let Err(error) = cache_result {
            // Remote state is authoritative; a future channel refresh will drop
            // the stale local row because no manifest is present remotely.
            log::warn!(
                "Logical file deleted remotely but local cache cleanup failed: {}",
                error
            );
        }
        return Ok(true);
    }

    // Verify the message exists before attempting to delete it.
    // This avoids a cryptic MESSAGE_ID_INVALID RPC error when the message
    // was already moved or deleted since the file list was loaded.
    let messages = client
        .get_messages_by_id(&peer, &[message_id])
        .await
        .map_err(|e| format!("Failed to fetch message for delete: {}", e))?;
    if messages.iter().flatten().next().is_none() {
        return Err(format!(
            "Message {} not found in folder {:?}. The file may have already been moved or deleted. Please refresh the folder.",
            message_id, folder_id
        ));
    }

    // Split files: delete every part (tolerating already-missing ones)
    let part_ids = resolve_parts(&client, &peer, message_id, false).await?;
    client
        .delete_messages(&peer, &part_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Downloads one part into its byte region [region_start, region_start + expected_len)
/// of the destination file, with per-chunk cancellation, retry and throttling.
async fn download_part_to_region(
    client: &grammers_client::Client,
    media: &Media,
    save_path: &str,
    region_start: u64,
    expected_len: u64,
    expected_hash: Option<&str>,
    part_no: usize,
    part_count: usize,
    counter: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    cancelled: &std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    tid: &str,
    net_config: &NetworkConfig,
    started: std::time::Instant,
) -> Result<(), String> {
    use sha2::Digest;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(save_path)
        .await
        .map_err(|e| e.to_string())?;
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(region_start))
        .await
        .map_err(|e| e.to_string())?;

    let mut download_iter = client.iter_download(media);
    let mut written: u64 = 0;
    let mut retry_budget = net_config.retry_attempts();
    let mut hasher = expected_hash.map(|_| sha2::Sha256::new());

    while let Some(chunk) = download_iter.next().await.transpose() {
        if cancelled.read().await.contains(tid) {
            return Err("Transfer cancelled".to_string());
        }
        let bytes = match chunk {
            Ok(b) => {
                retry_budget = net_config.retry_attempts();
                b
            }
            Err(e) => {
                let err = map_error(&e);
                if retry_budget > 0 {
                    retry_budget -= 1;
                    log::warn!(
                        "Download chunk error part {}/{} (retries left: {}): {}",
                        part_no,
                        part_count,
                        retry_budget,
                        err
                    );
                    let delay = backoff_ms(
                        0,
                        net_config.retry_base_backoff_ms(),
                        net_config.retry_max_backoff_ms(),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                return Err(format!("Download chunk error: {}", err));
            }
        };
        tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(h) = hasher.as_mut() {
            h.update(&bytes);
        }
        written += bytes.len() as u64;
        let total_so_far = counter
            .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed)
            + bytes.len() as u64;

        let dl_limit = net_config.download_limit_bytes_per_sec();
        if dl_limit > 0 {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let rate = total_so_far as f64 / elapsed;
            if rate > dl_limit as f64 {
                let ideal_elapsed = total_so_far as f64 / dl_limit as f64;
                let sleep_ms = ((ideal_elapsed - elapsed) * 1000.0) as u64;
                if sleep_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms.min(5000))).await;
                }
            }
        }
    }

    if expected_len > 0 && written != expected_len {
        return Err(format!(
            "Incomplete download of part {}/{}: expected {} bytes, received {} bytes",
            part_no, part_count, expected_len, written
        ));
    }

    if let (Some(expected), Some(h)) = (expected_hash, hasher) {
        let actual = format!("{:x}", h.finalize());
        if actual != expected {
            return Err(format!(
                "Corrupted part {}/{}: checksum mismatch (the stored data does not match what was uploaded)",
                part_no, part_count
            ));
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Downloads all parts of a split file concurrently into a preallocated
/// destination file. Progress and speed are reported by a single 250ms task
/// reading the shared byte counter, so they stay accurate regardless of how
/// the parts interleave. Returns the total bytes downloaded.
async fn download_parts_parallel(
    client: &grammers_client::Client,
    cancelled: &std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    net_config: &NetworkConfig,
    app_handle: &tauri::AppHandle,
    tid: &str,
    save_path: &str,
    parts: &[(Media, Option<u64>, Option<String>)],
    total_size: u64,
) -> Result<u64, String> {
    // Byte regions per part; every split part is a document with a known size
    let mut jobs: Vec<(usize, &Media, u64, u64, Option<&str>)> = Vec::with_capacity(parts.len());
    let mut region_start: u64 = 0;
    for (i, (media, expected, hash)) in parts.iter().enumerate() {
        let len = expected.ok_or_else(|| "Split part without a known size".to_string())?;
        jobs.push((i, media, region_start, len, hash.as_deref()));
        region_start += len;
    }

    // Preallocate so each part can write straight into its region
    let file = tokio::fs::File::create(save_path)
        .await
        .map_err(|e| e.to_string())?;
    file.set_len(total_size).await.map_err(|e| e.to_string())?;
    drop(file);

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = std::time::Instant::now();

    let emitter = if !tid.is_empty() {
        let emit_counter = counter.clone();
        let emit_tid = tid.to_string();
        let emit_handle = app_handle.clone();
        Some(tokio::spawn(async move {
            let mut last_bytes: u64 = 0;
            let mut last_time = std::time::Instant::now();
            let mut speed_smoother = SpeedWindow::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let current = emit_counter.load(std::sync::atomic::Ordering::Relaxed);
                let now = std::time::Instant::now();
                let dt = now.duration_since(last_time).as_secs_f64();
                let speed = speed_smoother.update(current.saturating_sub(last_bytes), dt);
                let percent = if total_size > 0 {
                    ((current as f64 / total_size as f64) * 100.0).min(99.0) as u8
                } else {
                    0
                };
                let _ = emit_handle.emit(
                    "download-progress",
                    ProgressPayload {
                        id: emit_tid.clone(),
                        percent,
                        uploaded_bytes: current,
                        total_bytes: total_size,
                        speed_bytes_per_sec: speed,
                    },
                );
                last_bytes = current;
                last_time = now;
                if current >= total_size {
                    break;
                }
            }
        }))
    } else {
        None
    };

    use futures::TryStreamExt;
    let part_count = parts.len();
    let result: Result<(), String> = futures::stream::iter(jobs.into_iter().map(Ok::<_, String>))
        .try_for_each_concurrent(download_parallel_parts(), |(i, media, start, len, hash)| {
            let counter = counter.clone();
            let cancelled = cancelled.clone();
            async move {
                download_part_to_region(
                    client,
                    media,
                    save_path,
                    start,
                    len,
                    hash,
                    i + 1,
                    part_count,
                    &counter,
                    &cancelled,
                    tid,
                    net_config,
                    started,
                )
                .await
            }
        })
        .await;

    if let Some(t) = emitter {
        t.abort();
    }
    result?;

    Ok(counter.load(std::sync::atomic::Ordering::Relaxed))
}

#[derive(Debug, serde::Deserialize)]
pub struct DownloadFileRequest {
    message_id: i32,
    save_path: String,
    folder_id: Option<i64>,
    transfer_id: Option<String>,
}

#[tauri::command]
pub async fn cmd_download_file(
    req: DownloadFileRequest,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    tdlib_state: State<'_, crate::tdlib_fast::TdlibFastState>,
    db_pool: State<'_, DbConnection>,
    bw_state: State<'_, Arc<BandwidthManager>>,
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<String, String> {
    let tid = req.transfer_id.unwrap_or_default();
    let save_path = req.save_path;
    let folder_id = req.folder_id;
    let message_id = req.message_id;

    #[cfg(target_os = "android")]
    let (actual_save_path, android_output_uri) = {
        use tauri::Manager;

        let normalized = clean_android_path(&save_path);
        if normalized.starts_with("content://") {
            let cache_dir = app_handle
                .path()
                .app_cache_dir()
                .map_err(|e| format!("Failed to get cache dir: {}", e))?;
            std::fs::create_dir_all(&cache_dir)
                .map_err(|e| format!("Failed to create Android cache dir: {}", e))?;

            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let cache_path = cache_dir
                .join(format!("download_{}_{}.part", message_id, nonce))
                .to_string_lossy()
                .to_string();

            log::info!(
                "Android download: using save-dialog URI with temporary cache '{}'",
                cache_path
            );
            (cache_path, Some(normalized))
        } else {
            let direct_path = clean_file_uri(&normalized);
            if !std::path::Path::new(&direct_path).is_absolute() {
                return Err(
                    "Android save destination was not an absolute path or content URI".to_string(),
                );
            }
            (direct_path, None)
        }
    };

    #[cfg(target_os = "ios")]
    let actual_save_path = clean_file_uri(&save_path);

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let actual_save_path = save_path.clone();

    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!(
            "[MOCK] Downloaded message {} from {:?} to {}",
            message_id,
            folder_id,
            actual_save_path
        );
        if let Err(e) = tokio::fs::write(&actual_save_path, b"Mock Content").await {
            return Err(e.to_string());
        }
        return Ok("Download successful".to_string());
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;

    // Manifest-first resolution: for TeraRelay channels the local cache was
    // rebuilt from the remote manifest during listing, so the visible entry's
    // ID resolves to the exact ordered Telegram chunk IDs. Legacy files fall
    // back to the existing .tgdpart resolver unchanged.
    let logical_manifest = if let Some(backing_channel_id) = folder_id {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        match crate::commands::logical_channels::logical_channel_id_for_backing(
            &conn,
            backing_channel_id,
        )? {
            Some(logical_channel_id) => {
                crate::commands::logical_files::cached_manifest_for_first_message(
                    &conn,
                    &logical_channel_id,
                    message_id as i64,
                )?
            }
            None => None,
        }
    } else {
        None
    };

    let part_ids: Vec<i32> = if let Some(manifest) = logical_manifest.as_ref() {
        manifest
            .chunks
            .iter()
            .map(|chunk| chunk.message_id as i32)
            .collect()
    } else {
        resolve_parts(&client, &peer, message_id, true).await?
    };
    let messages = get_messages_in_batches(&client, &peer, &part_ids).await?;

    // Telegram normally returns requested messages in order, but logical file
    // reconstruction must not depend on that implementation detail. Reorder
    // explicitly by the manifest/legacy part ID sequence.
    let mut message_map: HashMap<i32, _> = messages
        .into_iter()
        .flatten()
        .map(|message| (message.id(), message))
        .collect();
    let mut part_msgs = Vec::with_capacity(part_ids.len());
    for part_id in &part_ids {
        let message = message_map.remove(part_id).ok_or_else(|| {
            format!(
                "Stored chunk message {} is missing. Refresh the TeraRelay channel and retry.",
                part_id
            )
        })?;
        part_msgs.push(message);
    }
    if part_msgs.is_empty() {
        return Err("Message not found".to_string());
    }

    // Media + expected size + checksum per chunk. Manifest-backed files use
    // manifest metadata as the source of truth; legacy files retain the old
    // caption-based checksum behavior.
    let mut parts: Vec<(Media, Option<u64>, Option<String>)> = Vec::with_capacity(part_msgs.len());
    let mut total_size: u64 = 0;
    let mut expected_total: Option<u64> = Some(0);
    for (index, message) in part_msgs.iter().enumerate() {
        let media = message
            .media()
            .ok_or_else(|| "No media in stored chunk message".to_string())?;
        let telegram_size = match &media {
            Media::Document(document) => Some(document.size() as u64),
            _ => None,
        };

        let (part_expected, part_hash) = if let Some(manifest) = logical_manifest.as_ref() {
            let chunk = manifest
                .chunks
                .get(index)
                .ok_or_else(|| "Logical manifest chunk order is incomplete".to_string())?;
            let actual_size = telegram_size.ok_or_else(|| {
                format!(
                    "Logical chunk {} is no longer a Telegram document",
                    chunk.index
                )
            })?;
            if actual_size != chunk.size {
                return Err(format!(
                    "Logical chunk {}/{} size mismatch: manifest says {} bytes, Telegram has {} bytes",
                    chunk.index,
                    manifest.chunks.len(),
                    chunk.size,
                    actual_size
                ));
            }
            (Some(chunk.size), chunk.sha256.clone())
        } else {
            let hash = parse_part_name(&message_display_name(message))
                .and_then(|(_, _, _, hash)| hash.map(String::from));
            (telegram_size, hash)
        };

        total_size = total_size
            .checked_add(part_expected.unwrap_or(match &media {
                Media::Photo(_) => 1024 * 1024,
                _ => 0,
            }))
            .ok_or_else(|| "Download file size overflow".to_string())?;
        expected_total = match (expected_total, part_expected) {
            (Some(acc), Some(size)) => Some(
                acc.checked_add(size)
                    .ok_or_else(|| "Download file size overflow".to_string())?,
            ),
            _ => None,
        };
        parts.push((media, part_expected, part_hash));
    }
    let expected_file_size = logical_manifest
        .as_ref()
        .map(|manifest| manifest.total_size)
        .or(expected_total);

    bw_state.try_reserve_down(total_size)?;

    let mut output = crate::download_output::DownloadOutput::create(&actual_save_path, &tid)
        .await
        .map_err(|error| {
            bw_state.release_down(total_size);
            error
        })?;
    let staged_save_path = output.path().to_string();

    // Emit start
    if !tid.is_empty() {
        let _ = app_handle.emit(
            "download-progress",
            ProgressPayload {
                id: tid.clone(),
                percent: 0,
                uploaded_bytes: 0,
                total_bytes: total_size,
                speed_bytes_per_sec: 0,
            },
        );
    }

    let tdlib_download = cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && parts.iter().all(|(media, expected, _)| {
            matches!(media, Media::Document(_)) && expected.unwrap_or(0) > 0
        });
    let mut tdlib_average_bytes_per_sec: Option<u64> = None;
    let mut tdlib_network_seconds: Option<f64> = None;

    let tdlib_downloaded = if tdlib_download {
        if !crate::tdlib_fast::tdlib_is_ready(tdlib_state.inner()) {
            bw_state.release_down(total_size);
            return Err("TDLIB_SETUP_REQUIRED".to_string());
        }

        let chunks = part_ids
            .iter()
            .zip(parts.iter())
            .map(|(message_id, (_, expected, sha256))| {
                Ok(crate::tdlib_fast::FastDownloadChunk {
                    message_id: i64::from(*message_id),
                    size: expected.ok_or_else(|| {
                        "TDLib download requires a known Telegram document size".to_string()
                    })?,
                    sha256: sha256.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let destination = match folder_id {
            Some(id) => crate::tdlib_fast::FastDestination::Channel(id),
            None => crate::tdlib_fast::FastDestination::SavedMessages,
        };

        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let network_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let emitter = if !tid.is_empty() {
            let emit_counter = counter.clone();
            let emit_network_counter = network_counter.clone();
            let emit_tid = tid.clone();
            let emit_handle = app_handle.clone();
            Some(tokio::spawn(async move {
                let mut last_speed_bytes = 0u64;
                let mut last_time = std::time::Instant::now();
                let mut speed_smoother = SpeedWindow::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    let current = emit_counter.load(std::sync::atomic::Ordering::Relaxed);
                    let speed_bytes =
                        emit_network_counter.load(std::sync::atomic::Ordering::Relaxed);
                    let now = std::time::Instant::now();
                    let dt = now.duration_since(last_time).as_secs_f64();
                    let speed =
                        speed_smoother.update(speed_bytes.saturating_sub(last_speed_bytes), dt);
                    let percent = if total_size > 0 {
                        ((current as f64 / total_size as f64) * 100.0).min(99.0) as u8
                    } else {
                        0
                    };
                    let _ = emit_handle.emit(
                        "download-progress",
                        ProgressPayload {
                            id: emit_tid.clone(),
                            percent,
                            uploaded_bytes: current,
                            total_bytes: total_size,
                            speed_bytes_per_sec: speed,
                        },
                    );
                    last_speed_bytes = speed_bytes;
                    last_time = now;
                    if current >= total_size {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        if !tid.is_empty() {
            get_upload_cancellations()
                .lock()
                .unwrap()
                .insert(tid.clone(), cancel_tx.clone());
        }
        if !tid.is_empty() && state.cancelled_transfers.read().await.contains(&tid) {
            state.cancelled_transfers.write().await.remove(&tid);
            get_upload_cancellations().lock().unwrap().remove(&tid);
            if let Some(task) = emitter {
                task.abort();
            }
            bw_state.release_down(total_size);
            return Err("Transfer cancelled".to_string());
        }

        log::info!(
            "Download transport: TDLib/C++ native, chunks={}, bytes={}",
            chunks.len(),
            total_size
        );
        let result = crate::tdlib_fast::download_documents(
            tdlib_state.inner(),
            destination,
            staged_save_path.clone(),
            chunks,
            counter,
            network_counter,
            cancel_rx,
            false,
        )
        .await;

        if !tid.is_empty() {
            get_upload_cancellations().lock().unwrap().remove(&tid);
            state.cancelled_transfers.write().await.remove(&tid);
        }
        if let Some(task) = emitter {
            task.abort();
        }

        match result {
            Ok(outcome) => {
                tdlib_average_bytes_per_sec = Some(outcome.average_bytes_per_sec);
                tdlib_network_seconds = Some(outcome.network_seconds);
                Some(outcome.bytes_downloaded)
            }
            Err(error) => {
                bw_state.release_down(total_size);
                return Err(error);
            }
        }
    } else {
        None
    };

    let downloaded: u64 = if let Some(downloaded) = tdlib_downloaded {
        downloaded
    } else if parts.len() > 1 {
        // Split file fallback: download parts concurrently into a preallocated file.
        match download_parts_parallel(
            &client,
            &state.cancelled_transfers,
            &net_config,
            &app_handle,
            &tid,
            &staged_save_path,
            &parts,
            total_size,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                if e == "Transfer cancelled" {
                    state.cancelled_transfers.write().await.remove(&tid);
                }
                bw_state.release_down(total_size);
                return Err(e);
            }
        }
    } else {
        // Single document/photo: stream sequentially with inline progress
        // (photos have no exact expected size, so no preallocation here)
        let (media, part_expected, _) = &parts[0];
        let mut file = tokio::fs::File::create(&staged_save_path)
            .await
            .map_err(|e| {
                bw_state.release_down(total_size);
                e.to_string()
            })?;
        let mut downloaded: u64 = 0;
        let mut last_emit_time = std::time::Instant::now();
        let mut last_emit_bytes: u64 = 0;
        let mut speed_smoother = SpeedWindow::new();
        let mut chunk_retry_budget = net_config.retry_attempts();

        let mut download_iter = client.iter_download(media);

        while let Some(chunk) = download_iter.next().await.transpose() {
            // Check cancellation
            if state.cancelled_transfers.read().await.contains(&tid) {
                state.cancelled_transfers.write().await.remove(&tid);
                drop(file);
                bw_state.release_down(total_size);
                return Err("Transfer cancelled".to_string());
            }

            let bytes = match chunk {
                Ok(b) => {
                    chunk_retry_budget = net_config.retry_attempts(); // reset on success
                    b
                }
                Err(e) => {
                    let err = map_error(&e);
                    if chunk_retry_budget > 0 {
                        chunk_retry_budget -= 1;
                        log::warn!(
                            "Download chunk error (retries left: {}): {}",
                            chunk_retry_budget,
                            err
                        );
                        let delay = backoff_ms(
                            0,
                            net_config.retry_base_backoff_ms(),
                            net_config.retry_max_backoff_ms(),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        continue;
                    }
                    drop(file);
                    bw_state.release_down(total_size);
                    return Err(format!("Download chunk error: {}", err));
                }
            };
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
                .await
                .map_err(|e| e.to_string())?;
            downloaded += bytes.len() as u64;

            // Time-based progress emission (every 250ms)
            if !tid.is_empty() {
                let now = std::time::Instant::now();
                let dt = now.duration_since(last_emit_time).as_secs_f64();
                if dt >= 0.25 || downloaded >= total_size {
                    let speed =
                        speed_smoother.update(downloaded.saturating_sub(last_emit_bytes), dt);
                    let percent = if total_size > 0 {
                        ((downloaded as f64 / total_size as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };
                    let _ = app_handle.emit(
                        "download-progress",
                        ProgressPayload {
                            id: tid.clone(),
                            percent,
                            uploaded_bytes: downloaded,
                            total_bytes: total_size,
                            speed_bytes_per_sec: speed,
                        },
                    );
                    last_emit_time = now;
                    last_emit_bytes = downloaded;
                }
            }

            // Bandwidth throttle: if download limit is set, sleep to maintain rate
            let dl_limit = net_config.download_limit_bytes_per_sec();
            if dl_limit > 0 {
                let elapsed = last_emit_time.elapsed().as_secs_f64().max(0.001);
                let current_rate = (downloaded - last_emit_bytes) as f64 / elapsed;
                if current_rate > dl_limit as f64 {
                    let sleep_ms =
                        ((current_rate / dl_limit as f64 - 1.0) * elapsed * 1000.0) as u64;
                    if sleep_ms > 0 && sleep_ms < 5000 {
                        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                    }
                }
            }
        }

        if let Some(expected) = part_expected {
            if *expected > 0 && downloaded != *expected {
                drop(file);
                bw_state.release_down(total_size);
                return Err(format!(
                    "Incomplete download before saving: expected {} bytes, received {} bytes",
                    expected, downloaded
                ));
            }
        }

        // Explicitly flush, sync, and close the file before JNI/MediaStore copies it.
        if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
            drop(file);
            bw_state.release_down(total_size);
            return Err(format!("Failed to flush downloaded file: {}", e));
        }
        if let Err(e) = file.sync_all().await {
            drop(file);
            bw_state.release_down(total_size);
            return Err(format!("Failed to sync downloaded file: {}", e));
        }
        drop(file);
        downloaded
    };

    let actual_written = tokio::fs::metadata(&staged_save_path)
        .await
        .map_err(|e| format!("Downloaded file missing before save: {}", e))?
        .len();
    if actual_written == 0 {
        bw_state.release_down(total_size);
        return Err("Downloaded file was empty before saving".to_string());
    }
    if actual_written != downloaded {
        bw_state.release_down(total_size);
        return Err(format!(
            "Downloaded file size mismatch before saving: streamed {} bytes, file has {} bytes",
            downloaded, actual_written
        ));
    }
    if let Some(expected) = expected_file_size {
        if expected > 0 && downloaded != expected {
            bw_state.release_down(total_size);
            return Err(format!(
                "Incomplete download before saving: expected {} bytes, received {} bytes",
                expected, downloaded
            ));
        }
    }
    output.publish().await.map_err(|error| {
        bw_state.release_down(total_size);
        error
    })?;

    log::info!(
        "Download completed to cache path {} ({} bytes)",
        actual_save_path,
        actual_written
    );
    if let (Some(average), Some(seconds)) = (tdlib_average_bytes_per_sec, tdlib_network_seconds) {
        log::info!(
            "TDLib measured download: {:.2} MiB/s average over {:.2}s",
            average as f64 / (1024.0 * 1024.0),
            seconds
        );
    }

    // Emit completion
    if !tid.is_empty() {
        let _ = app_handle.emit(
            "download-progress",
            ProgressPayload {
                id: tid,
                percent: 100,
                uploaded_bytes: downloaded,
                total_bytes: total_size,
                speed_bytes_per_sec: tdlib_average_bytes_per_sec.unwrap_or(0),
            },
        );
    }

    #[cfg(target_os = "android")]
    {
        if let Some(destination_uri) = android_output_uri.as_deref() {
            if let Err(e) = copy_file_to_android_uri(&actual_save_path, destination_uri) {
                // Preserve the cache copy on failure so the downloaded bytes are not lost.
                log::error!(
                    "Android save-dialog write failed; cache preserved at '{}': {}",
                    actual_save_path,
                    e
                );
                bw_state.release_down(total_size);
                return Err(format!("Failed to save downloaded file: {}", e));
            }

            let _ = tokio::fs::remove_file(&actual_save_path).await;
            log::info!(
                "Android save-dialog write completed and cache was cleaned: {}",
                actual_save_path
            );
        }
    }

    Ok("Download successful".to_string())
}

#[tauri::command]
pub async fn cmd_move_files(
    message_ids: Vec<i32>,
    source_folder_id: Option<i64>,
    target_folder_id: Option<i64>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<bool, String> {
    let (source_logical_channel_id, target_logical_channel_id) = {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, source_folder_id)?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, target_folder_id)?;

        let source = match source_folder_id {
            Some(backing_channel_id) => {
                crate::commands::logical_channels::logical_channel_id_for_backing(
                    &conn,
                    backing_channel_id,
                )?
            }
            None => None,
        };
        let target = match target_folder_id {
            Some(backing_channel_id) => {
                crate::commands::logical_channels::logical_channel_id_for_backing(
                    &conn,
                    backing_channel_id,
                )?
            }
            None => None,
        };
        (source, target)
    };

    if source_folder_id == target_folder_id {
        return Ok(true);
    }
    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!(
            "[MOCK] Moved msgs {:?} from {:?} to {:?}",
            message_ids,
            source_folder_id,
            target_folder_id
        );
        return Ok(true);
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let source_peer = resolve_peer(&client, source_folder_id, &state.peer_cache).await?;
    let target_peer = resolve_peer(&client, target_folder_id, &state.peer_cache).await?;

    // Separate manifest-backed logical files from legacy Telegram messages.
    // The visible ID of a logical file is its first chunk message ID.
    let mut logical_manifests = Vec::new();
    let mut manifest_seed_ids = std::collections::HashSet::new();
    let mut seen_file_ids = std::collections::HashSet::new();
    if let Some(source_logical_channel_id) = source_logical_channel_id.as_deref() {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        for message_id in &message_ids {
            if let Some(manifest) =
                crate::commands::logical_files::cached_manifest_for_first_message(
                    &conn,
                    source_logical_channel_id,
                    *message_id as i64,
                )?
            {
                manifest_seed_ids.insert(*message_id);
                if seen_file_ids.insert(manifest.file_id.clone()) {
                    logical_manifests.push(manifest);
                }
            }
        }
    }

    let mut legacy_message_ids: Vec<i32> = message_ids
        .into_iter()
        .filter(|id| !manifest_seed_ids.contains(id))
        .collect();
    let mut source_manifest_cleanup_ids = Vec::new();

    if let Some(target_logical_channel_id) = target_logical_channel_id.as_deref() {
        // Logical channel -> logical channel: forward only the physical chunks,
        // then publish a target manifest containing the NEW Telegram message IDs.
        for mut manifest in logical_manifests {
            let source_manifest_ids =
                find_logical_manifest_message_ids(&client, &source_peer, &manifest.file_id).await?;
            let source_chunk_ids: Vec<i32> = manifest
                .chunks
                .iter()
                .map(|chunk| chunk.message_id as i32)
                .collect();

            let forwarded = client
                .forward_messages(&target_peer, &source_chunk_ids, &source_peer)
                .await
                .map_err(|e| format!("Forward logical file failed: {e}"))?;

            let mut target_chunk_ids = Vec::with_capacity(forwarded.len());
            for forwarded_message in forwarded {
                match forwarded_message {
                    Some(message) => target_chunk_ids.push(message.id()),
                    None => {
                        if !target_chunk_ids.is_empty() {
                            let _ = client
                                .delete_messages(&target_peer, &target_chunk_ids)
                                .await;
                        }
                        return Err(
                            "Telegram did not forward every logical file chunk; the target copy was rolled back."
                                .to_string(),
                        );
                    }
                }
            }
            if target_chunk_ids.len() != manifest.chunks.len() {
                if !target_chunk_ids.is_empty() {
                    let _ = client
                        .delete_messages(&target_peer, &target_chunk_ids)
                        .await;
                }
                return Err(
                    "Forwarded logical file chunk count did not match the manifest; the target copy was rolled back."
                        .to_string(),
                );
            }

            manifest.logical_channel_id = target_logical_channel_id.to_string();
            for (chunk, new_message_id) in manifest.chunks.iter_mut().zip(target_chunk_ids.iter()) {
                chunk.message_id = *new_message_id as i64;
            }

            let target_manifest_id = match upload_logical_manifest_document(
                &client,
                &app_handle,
                &manifest,
                &target_peer,
            )
            .await
            {
                Ok(id) => id as i32,
                Err(error) => {
                    let _ = client
                        .delete_messages(&target_peer, &target_chunk_ids)
                        .await;
                    return Err(format!(
                        "Forwarded chunks were rolled back because the target TeraRelay manifest could not be published: {error}"
                    ));
                }
            };

            let mut source_delete_ids = source_chunk_ids;
            source_delete_ids.extend(source_manifest_ids);
            source_delete_ids.sort_unstable();
            source_delete_ids.dedup();

            if let Err(error) = client
                .delete_messages(&source_peer, &source_delete_ids)
                .await
            {
                let mut rollback_ids = target_chunk_ids;
                rollback_ids.push(target_manifest_id);
                rollback_ids.sort_unstable();
                rollback_ids.dedup();
                let _ = client.delete_messages(&target_peer, &rollback_ids).await;
                return Err(format!(
                    "Could not remove the source logical file; the target copy was rolled back: {error}"
                ));
            }

            let persist_result = {
                let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
                crate::commands::logical_files::persist_manifest(&conn, &manifest)
            };
            if let Err(error) = persist_result {
                log::warn!(
                    "Logical move completed remotely but local target manifest cache update failed: {}",
                    error
                );
            }
        }
    } else {
        // Moving out of a TeraRelay channel intentionally degrades to the
        // existing legacy representation: forward the physical chunks only and
        // remove the source manifest so it cannot reference deleted messages.
        for manifest in logical_manifests {
            legacy_message_ids.extend(manifest.chunks.iter().map(|chunk| chunk.message_id as i32));
            source_manifest_cleanup_ids.extend(
                find_logical_manifest_message_ids(&client, &source_peer, &manifest.file_id).await?,
            );
        }
    }

    if legacy_message_ids.is_empty() {
        return Ok(true);
    }

    // Legacy split files: expand each selected id to all its .tgdpart siblings.
    legacy_message_ids.sort_unstable();
    legacy_message_ids.dedup();
    let seed_msgs = client
        .get_messages_by_id(&source_peer, &legacy_message_ids)
        .await
        .map_err(|e| e.to_string())?;
    let seeds: Vec<(String, u32)> = seed_msgs
        .iter()
        .flatten()
        .filter_map(|message| {
            parse_part_name(&message_display_name(message))
                .map(|(base, _, total, _)| (base.to_string(), total))
        })
        .collect();
    if !seeds.is_empty() {
        let mut messages = client.iter_messages(&source_peer);
        while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
            if let Some((base, _, total, _)) = parse_part_name(&message_display_name(&message)) {
                if seeds
                    .iter()
                    .any(|(seed_base, seed_total)| *seed_base == base && *seed_total == total)
                    && !legacy_message_ids.contains(&message.id())
                {
                    legacy_message_ids.push(message.id());
                }
            }
        }
    }
    legacy_message_ids.sort_unstable();
    legacy_message_ids.dedup();

    let forwarded = client
        .forward_messages(&target_peer, &legacy_message_ids, &source_peer)
        .await
        .map_err(|e| format!("Forward failed: {e}"))?;
    let target_forwarded_ids: Vec<i32> = forwarded
        .into_iter()
        .flatten()
        .map(|message| message.id())
        .collect();
    if target_forwarded_ids.len() != legacy_message_ids.len() {
        if !target_forwarded_ids.is_empty() {
            let _ = client
                .delete_messages(&target_peer, &target_forwarded_ids)
                .await;
        }
        return Err(
            "Telegram did not forward every selected file; the target copy was rolled back."
                .to_string(),
        );
    }

    let mut source_delete_ids = legacy_message_ids;
    source_delete_ids.extend(source_manifest_cleanup_ids);
    source_delete_ids.sort_unstable();
    source_delete_ids.dedup();

    if let Err(error) = client
        .delete_messages(&source_peer, &source_delete_ids)
        .await
    {
        let _ = client
            .delete_messages(&target_peer, &target_forwarded_ids)
            .await;
        return Err(format!(
            "Delete original failed; the target copy was rolled back: {error}"
        ));
    }

    Ok(true)
}

#[tauri::command]
pub async fn cmd_get_files(
    folder_id: Option<i64>,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<Vec<FileMetadata>, String> {
    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!("[MOCK] Returning mock files for folder {:?}", folder_id);
        return Ok(Vec::new()); // No mock files for now
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;
    let mut files = Vec::new();

    let logical_channel_id = if let Some(backing_channel_id) = folder_id {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::logical_channel_id_for_backing(
            &conn,
            backing_channel_id,
        )?
    } else {
        None
    };
    let mut active_remote_manifest_ids = std::collections::HashSet::new();

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;

    // Part messages of split files, keyed by (base name, total parts):
    // (idx, message id, part size, mime, created_at)
    let mut part_groups: HashMap<(String, u32), Vec<(u32, i64, u64, Option<String>, String)>> =
        HashMap::new();

    let mut msgs = client.iter_messages(&peer);
    while let Some(msg) = msgs.next().await.map_err(|e| e.to_string())? {
        if let Some(media) = msg.media() {
            // Manifest documents are storage metadata, never user files. A
            // receiver with an empty local DB downloads them here and rebuilds
            // the exact same logical index from Telegram.
            if let (Some(logical_channel_id), Media::Document(document)) =
                (logical_channel_id.as_deref(), &media)
            {
                let document_name = document.name().to_string();
                if let Some(file_id) =
                    crate::commands::logical_files::manifest_file_id(&document_name, msg.text())
                {
                    if active_remote_manifest_ids.contains(&file_id) {
                        continue;
                    }

                    match crate::commands::logical_files::download_manifest(
                        &client,
                        &media,
                        Some(&file_id),
                        Some(logical_channel_id),
                    )
                    .await
                    {
                        Ok(manifest) => {
                            let persist_result = {
                                let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
                                crate::commands::logical_files::persist_manifest(&conn, &manifest)
                            };
                            match persist_result {
                                Ok(()) => {
                                    active_remote_manifest_ids.insert(file_id);
                                }
                                Err(error) => {
                                    log::warn!(
                                        "Ignoring TeraRelay manifest {} because local indexing failed: {}",
                                        file_id,
                                        error
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            log::warn!(
                                "Ignoring invalid TeraRelay manifest {} in channel {:?}: {}",
                                file_id,
                                folder_id,
                                error
                            );
                        }
                    }
                    continue;
                }
            }

            let (name, size, mime, ext) = match media {
                Media::Document(d) => {
                    let doc_name = d.name().to_string();
                    // Prefer the message caption (set by rename via EditMessage) over the
                    // document's built-in filename attribute, so renames persist across refreshes.
                    let caption = msg.text();
                    let display_name = if caption.is_empty() {
                        doc_name.clone()
                    } else {
                        caption.to_string()
                    };
                    if let Some((base, idx, total, _)) = parse_part_name(&display_name) {
                        part_groups
                            .entry((base.to_string(), total))
                            .or_default()
                            .push((
                                idx,
                                msg.id() as i64,
                                d.size() as u64,
                                d.mime_type().map(|s| s.to_string()),
                                msg.date().to_string(),
                            ));
                        continue;
                    }
                    let s = d.size();
                    let m = d.mime_type().map(|s| s.to_string());
                    // Extension always from the original document name for correct file-type icon
                    let e = std::path::Path::new(&doc_name)
                        .extension()
                        .map(|os| os.to_str().unwrap_or("").to_string());
                    (display_name, s, m, e)
                }
                Media::Photo(_) => (
                    "Photo.jpg".to_string(),
                    0,
                    Some("image/jpeg".into()),
                    Some("jpg".into()),
                ),
                _ => ("Unknown".to_string(), 0, None, None),
            };
            files.push(FileMetadata {
                id: msg.id() as i64,
                folder_id,
                name,
                size: size as u64,
                mime_type: mime,
                file_ext: ext,
                created_at: msg.date().to_string(),
                icon_type: "file".into(),
                is_split: false,
            });
        }
    }

    // Collapse each part group into a single entry, id = part 1's message id.
    // Incomplete groups (interrupted upload) are still shown; download reports
    // the exact missing part.
    for ((base, _total), mut parts) in part_groups {
        parts.sort_by_key(|p| p.0);
        let total_size: u64 = parts.iter().map(|p| p.2).sum();
        let first = &parts[0];
        let ext = std::path::Path::new(&base)
            .extension()
            .map(|os| os.to_str().unwrap_or("").to_string());
        files.push(FileMetadata {
            id: first.1,
            folder_id,
            name: base,
            size: total_size,
            mime_type: first.3.clone(),
            file_ext: ext,
            created_at: first.4.clone(),
            icon_type: "file".into(),
            is_split: true,
        });
    }

    // Manifest-first channels replace physical Telegram documents with one
    // cached logical entry. Every referenced chunk (including legacy-named
    // chunks) is removed from the fallback list. Files without a manifest stay
    // visible through the legacy path for backward compatibility.
    if let (Some(logical_channel_id), Some(backing_channel_id)) =
        (logical_channel_id.as_deref(), folder_id)
    {
        let (manifest_files, manifest_chunk_ids) = {
            let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
            let manifest_files = crate::commands::logical_files::cached_file_metadata(
                &conn,
                logical_channel_id,
                &active_remote_manifest_ids,
                backing_channel_id,
            )?;
            let manifest_chunk_ids = crate::commands::logical_files::cached_chunk_message_ids(
                &conn,
                logical_channel_id,
                &active_remote_manifest_ids,
            )?;
            (manifest_files, manifest_chunk_ids)
        };

        files.retain(|file| !manifest_chunk_ids.contains(&file.id));
        files.extend(manifest_files);
    }

    // iter_messages yields newest first (descending message id); keep that
    // order with collapsed entries positioned by their first part.
    files.sort_by(|a, b| b.id.cmp(&a.id));

    Ok(files)
}

/// Extract FileMetadata entries from a list of Telegram messages returned by SearchGlobal.
fn extract_search_files(msgs: &[tl::enums::Message]) -> Vec<FileMetadata> {
    let mut files = Vec::new();
    for msg in msgs {
        if let tl::enums::Message::Message(m) = msg {
            if let Some(tl::enums::MessageMedia::Document(d)) = &m.media {
                if let Some(tl::enums::Document::Document(doc)) = &d.document {
                    let doc_name = doc
                        .attributes
                        .iter()
                        .find_map(|a| match a {
                            tl::enums::DocumentAttribute::Filename(f) => Some(f.file_name.clone()),
                            _ => None,
                        })
                        .unwrap_or("Unknown".to_string());

                    // TeraRelay manifest documents are hidden storage metadata,
                    // never user-visible global-search results.
                    if crate::commands::logical_files::manifest_file_id(&doc_name, &m.message)
                        .is_some()
                    {
                        continue;
                    }

                    // Prefer the message caption over the built-in document filename
                    let name = if m.message.is_empty() {
                        doc_name.clone()
                    } else {
                        m.message.clone()
                    };
                    // Split files: represent the whole file by its first part, hide the rest.
                    // (Size shown is part 1's size only; the folder listing has the full sum.)
                    let (name, is_split) = match parse_part_name(&name) {
                        Some((base, 1, _, _)) => (base.to_string(), true),
                        Some(_) => continue,
                        None => (name, false),
                    };
                    let size = doc.size as u64;
                    let mime = doc.mime_type.clone();
                    let ext_source = if is_split { &name } else { &doc_name };
                    let ext = std::path::Path::new(ext_source)
                        .extension()
                        .map(|os| os.to_str().unwrap_or("").to_string());
                    let folder_id = match &m.peer_id {
                        tl::enums::Peer::Channel(c) => Some(c.channel_id),
                        tl::enums::Peer::User(u) => Some(u.user_id),
                        tl::enums::Peer::Chat(c) => Some(c.chat_id),
                    };
                    files.push(FileMetadata {
                        id: m.id as i64,
                        folder_id,
                        name,
                        size,
                        mime_type: Some(mime),
                        file_ext: ext,
                        created_at: m.date.to_string(),
                        icon_type: "file".into(),
                        is_split,
                    });
                }
            }
        }
    }
    files
}

#[tauri::command]
pub async fn cmd_search_global(
    query: String,
    state: State<'_, TelegramState>,
) -> Result<Vec<FileMetadata>, String> {
    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        return Ok(Vec::new());
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    log::info!("Searching global for: {}", query);

    let result = client
        .invoke(&tl::functions::messages::SearchGlobal {
            q: query,
            filter: tl::enums::MessagesFilter::InputMessagesFilterDocument,
            min_date: 0,
            max_date: 0,
            offset_rate: 0,
            offset_peer: tl::enums::InputPeer::Empty,
            offset_id: 0,
            limit: 50,
            folder_id: None,
            broadcasts_only: false,
            groups_only: false,
            users_only: false,
        })
        .await
        .map_err(map_error)?;

    let files = match result {
        tl::enums::messages::Messages::Messages(msgs) => extract_search_files(&msgs.messages),
        tl::enums::messages::Messages::Slice(msgs) => extract_search_files(&msgs.messages),
        _ => Vec::new(),
    };

    Ok(files)
}

#[tauri::command]
pub async fn cmd_scan_folders(
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<Vec<FolderMetadata>, String> {
    let client_opt = { state.client.lock().await.clone() };
    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        // If not connected, return whatever is already in the database
        return crate::commands::folder_groups::cmd_get_enriched_folders(db_pool).await;
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let mut folders = Vec::new();
    let mut dialogs = client.iter_dialogs();
    let mut discovered = HashMap::new();

    log::info!("Starting Folder Scan...");

    while let Some(dialog) = dialogs.next().await.map_err(|e| e.to_string())? {
        // Populate peer cache for every dialog we encounter (free priming)
        match &dialog.peer {
            Peer::Channel(c) => {
                let id = c.raw.id;
                discovered.insert(id, dialog.peer.clone());

                let name = c.raw.title.clone();
                let access_hash = c.raw.access_hash.unwrap_or(0);

                log::debug!("[SCAN] Processing Channel: '{}' (ID: {})", name, id);

                // Strategy 1: Title
                if {
                    let lower = name.to_lowercase();
                    lower.contains("[tr]") || lower.contains("[td]")
                } {
                    log::info!(" -> MATCH via Title: {}", name);
                    let display_name = name
                        .replace(" [TR]", "")
                        .replace(" [tr]", "")
                        .replace("[TR]", "")
                        .replace("[tr]", "")
                        .replace(" [TD]", "")
                        .replace(" [td]", "")
                        .replace("[TD]", "")
                        .replace("[td]", "")
                        .trim()
                        .to_string();
                    let username = c.raw.username.clone();
                    let is_public = username.is_some();
                    folders.push(FolderMetadata {
                        id,
                        name: display_name,
                        parent_id: None,
                        username,
                        is_public,
                        group_id: None,
                        display_order: 0,
                        role: Some(if c.raw.creator { "owner" } else { "member" }.to_string()),
                    });
                    continue;
                }

                // Strategy 2: About (Only if we are the creator to avoid rate limits on third-party channels)
                if c.raw.creator {
                    let input_chan = tl::enums::InputChannel::Channel(tl::types::InputChannel {
                        channel_id: c.raw.id,
                        access_hash,
                    });

                    match client
                        .invoke(&tl::functions::channels::GetFullChannel {
                            channel: input_chan,
                        })
                        .await
                    {
                        Ok(tl::enums::messages::ChatFull::Full(f)) => {
                            if let tl::enums::ChatFull::Full(cf) = f.full_chat {
                                if cf.about.contains("[terarelay-folder]") {
                                    log::info!(" -> MATCH via About: {}", name);
                                    let username = c.raw.username.clone();
                                    let is_public = username.is_some();
                                    folders.push(FolderMetadata {
                                        id,
                                        name: name.clone(),
                                        parent_id: None,
                                        username,
                                        is_public,
                                        group_id: None,
                                        display_order: 0,
                                        role: Some("owner".to_string()),
                                    });
                                }
                            }
                        }
                        Err(e) => log::warn!(" -> Failed to get full info: {}", e),
                    }
                }
            }
            Peer::User(u) => {
                discovered.insert(u.raw.id(), dialog.peer.clone());
                log::debug!("[SCAN] Cached User Peer: {}", u.raw.id());
            }
            peer => {
                log::debug!("[SCAN] Skipped Peer: {:?}", peer);
            }
        }
    }

    {
        let mut cache = state.peer_cache.write().await;
        cache.extend(discovered);
    }

    let cache_len = state.peer_cache.read().await.len();
    log::info!(
        "Scan complete. Found {} folders. Peer cache size: {}.",
        folders.len(),
        cache_len
    );

    // Enrich folders via the local DB
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    let enriched = crate::commands::folder_groups::get_enriched_folders_internal(&conn, folders)?;
    Ok(enriched)
}

/// Zip a folder's contents into a temp file and return the path.
/// The resulting zip preserves the relative directory structure.
#[tauri::command]
pub async fn cmd_zip_folder(folder_path: String) -> Result<String, String> {
    let folder_path = if cfg!(target_os = "android") {
        clean_android_path(&folder_path)
    } else {
        folder_path
    };

    let src = std::path::Path::new(&folder_path)
        .canonicalize()
        .map_err(|e| format!("Invalid folder path: {}", e))?;
    if !src.is_dir() {
        return Err(format!("'{}' is not a directory", folder_path));
    }

    let folder_name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "folder".to_string());

    let zip_dir =
        std::env::temp_dir().join(format!("terarelay-zip-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&zip_dir)
        .map_err(|e| format!("Failed to create archive directory: {e}"))?;
    let zip_path = zip_dir.join(format!("{}.zip", folder_name));
    let src_owned = src.clone();
    let out_path = zip_path.clone();
    let archive_dir = zip_dir.clone();

    // Run blocking I/O on a dedicated thread so we don't stall the async runtime
    let archive_result = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::create(&out_path)
            .map_err(|e| format!("Failed to create zip file: {}", e))?;
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);

        for entry in walkdir::WalkDir::new(&src_owned)
            .into_iter()
            .filter_entry(|entry| entry.path() != archive_dir)
        {
            let entry = entry.map_err(|e| format!("Failed to read folder entry: {e}"))?;
            let path = entry.path();
            let relative = path.strip_prefix(&src_owned).unwrap_or(path);

            if path.is_file() {
                let name = relative.to_string_lossy().to_string();
                zip_writer
                    .start_file(&name, options)
                    .map_err(|e| format!("Failed to add '{}': {}", name, e))?;
                let mut f = std::fs::File::open(path)
                    .map_err(|e| format!("Failed to open '{}': {}", name, e))?;
                std::io::copy(&mut f, &mut zip_writer)
                    .map_err(|e| format!("Failed to write '{}': {}", name, e))?;
            } else if path.is_dir() && path != src_owned {
                let dir_name = format!("{}/", relative.to_string_lossy());
                zip_writer
                    .add_directory(&dir_name, options)
                    .map_err(|e| format!("Failed to add dir '{}': {}", dir_name, e))?;
            }
        }

        zip_writer
            .finish()
            .map_err(|e| format!("Failed to finalize zip: {}", e))?;
        let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
        Ok::<(String, u64), String>((out_path.to_string_lossy().to_string(), size))
    })
    .await;
    let (zip_path_str, zip_size) = match archive_result {
        Ok(Ok(archive)) => archive,
        result => {
            // Only remove the output owned by this archive attempt.
            let _ = std::fs::remove_file(&zip_path);
            let _ = std::fs::remove_dir(&zip_dir);
            return Err(match result {
                Ok(Err(error)) => error,
                Err(error) => format!("Zip task panicked: {}", error),
                Ok(Ok(_)) => unreachable!(),
            });
        }
    };

    log::info!(
        "Zipped '{}' -> '{}' ({} bytes)",
        folder_name,
        zip_path_str,
        zip_size
    );

    Ok(zip_path_str)
}

/// Delete a temporary zip file created by cmd_zip_folder.
#[tauri::command]
pub async fn cmd_delete_temp_zip(path: String) -> Result<(), String> {
    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || {
        let p = std::path::Path::new(&path_clone);
        if !p.exists() {
            return Ok(());
        }
        let canonical_p = p
            .canonicalize()
            .map_err(|e| format!("Invalid path: {}", e))?;
        let tmp = std::env::temp_dir()
            .canonicalize()
            .map_err(|e| format!("Could not resolve temp directory: {}", e))?;
        if !canonical_p.starts_with(&tmp) {
            return Err("Refusing to delete file outside temp directory".to_string());
        }
        std::fs::remove_file(&canonical_p).map_err(|e| e.to_string())?;
        if let Some(parent) = canonical_p.parent() {
            if parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("terarelay-zip-"))
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        log::info!("Cleaned up temp zip: {}", path_clone);
        Ok(())
    })
    .await
    .map_err(|e| format!("Task panicked: {}", e))?
}

#[cfg(test)]
mod focused_transfer_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::time::{Duration, Instant};

    #[test]
    fn bursty_network_updates_keep_a_measured_average_between_reports() {
        let now = Instant::now();
        let bytes = 32 * 1024 * 1024;
        let samples: VecDeque<_> = (0..=48)
            .map(|tick| {
                (
                    now - Duration::from_millis((48 - tick) * 250),
                    if tick < 8 { 0 } else { bytes },
                )
            })
            .collect();
        let mut window = SpeedWindow {
            total: bytes,
            samples,
        };
        let speed = window.update(0, 0.25);
        assert!(
            speed > 2 * 1024 * 1024 && speed < 4 * 1024 * 1024,
            "Coalesced reporting gap produced {speed} B/s instead of the measured average"
        );
    }

    #[test]
    fn measured_average_eventually_reaches_zero_after_a_real_stall() {
        let now = Instant::now();
        let bytes = 32 * 1024 * 1024;
        let samples: VecDeque<_> = (0..=80)
            .map(|tick| {
                (
                    now - Duration::from_millis((80 - tick) * 250),
                    if tick < 8 { 0 } else { bytes },
                )
            })
            .collect();
        let mut window = SpeedWindow {
            total: bytes,
            samples,
        };
        assert_eq!(window.update(0, 0.25), 0);
    }

    struct Fixture {
        root: std::path::PathBuf,
        archives: Vec<std::path::PathBuf>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for path in &self.archives {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn folder_archives_reserve_zip64_and_preserve_large_entry_contents() {
        use std::io::{Seek, SeekFrom, Write};
        let mut fixture = Fixture {
            root: std::env::temp_dir()
                .join(format!("terarelay-zip64-qa-{}", rand::random::<u64>())),
            archives: Vec::new(),
        };
        std::fs::create_dir_all(&fixture.root).unwrap();
        // The normal suite verifies the ZIP64 wire format with a small file.
        // A targeted run can exercise the real 4 GiB boundary with a sparse source.
        let size = std::env::var("TERA_QA_ZIP64_BYTES")
            .ok()
            .map(|value| value.parse::<u64>().unwrap())
            .unwrap_or(1024);
        let first = b"zip64-first";
        let last = b"zip64-last";
        assert!(size > (first.len() + last.len()) as u64);
        let mut source = std::fs::File::create(fixture.root.join("large.bin")).unwrap();
        source.set_len(size).unwrap();
        source.write_all(first).unwrap();
        source.seek(SeekFrom::End(-(last.len() as i64))).unwrap();
        source.write_all(last).unwrap();
        drop(source);
        let path = cmd_zip_folder(fixture.root.to_string_lossy().into())
            .await
            .unwrap();
        fixture.archives.push(path.clone().into());
        let encoded = std::fs::read(&path).unwrap();
        assert_eq!(&encoded[..4], b"PK\x03\x04");
        assert_eq!(
            u32::from_le_bytes(encoded[18..22].try_into().unwrap()),
            u32::MAX,
            "Folder archive lacks the ZIP64 compressed-size reservation"
        );
        assert_eq!(
            u32::from_le_bytes(encoded[22..26].try_into().unwrap()),
            u32::MAX,
            "Folder archive lacks the ZIP64 uncompressed-size reservation"
        );
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut entry = archive.by_name("large.bin").unwrap();
        assert_eq!(entry.size(), size);
        let mut prefix = vec![0; first.len()];
        entry.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, first);
        let middle = size - (first.len() + last.len()) as u64;
        assert_eq!(
            std::io::copy(&mut (&mut entry).take(middle), &mut std::io::sink()).unwrap(),
            middle
        );
        let mut suffix = vec![0; last.len()];
        entry.read_exact(&mut suffix).unwrap();
        assert_eq!(suffix, last);
        assert_eq!(entry.read(&mut [0]).unwrap(), 0);
        println!("PASS actual folder ZIP64 round trip: {size} bytes");
    }

    #[tokio::test]
    async fn folder_uploads_with_the_same_name_keep_independent_archives() {
        let name = format!("terarelay-folder-qa-{}", rand::random::<u64>());
        let mut fixture = Fixture {
            root: std::env::temp_dir().join(format!("{name}-root")),
            archives: Vec::new(),
        };
        let first = fixture.root.join("one").join(&name);
        let second = fixture.root.join("two").join(&name);
        std::fs::create_dir_all(first.join("nested")).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("nested/data.txt"), b"first folder").unwrap();
        std::fs::write(second.join("other.txt"), b"second folder").unwrap();
        let first_zip = cmd_zip_folder(first.to_string_lossy().into())
            .await
            .unwrap();
        fixture.archives.push(first_zip.clone().into());
        let second_zip = cmd_zip_folder(second.to_string_lossy().into())
            .await
            .unwrap();
        fixture.archives.push(second_zip.clone().into());
        assert_ne!(
            first_zip, second_zip,
            "Second folder overwrote an archive queued for upload"
        );
        let mut archive = zip::ZipArchive::new(std::fs::File::open(first_zip).unwrap()).unwrap();
        let mut contents = String::new();
        archive
            .by_name("nested/data.txt")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "first folder");
    }
}

/// Toggle a folder (channel) between private and public.
/// When making public, a username is generated from the channel title.
/// When making private, the username is removed.
#[tauri::command]
pub async fn cmd_toggle_folder_visibility(
    folder_id: i64,
    make_public: bool,
    desired_username: Option<String>,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<FolderMetadata, String> {
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::require_owner_role(&conn, folder_id)?;
    }

    let client_opt = { state.client.lock().await.clone() };

    let mut folder = if client_opt.is_none() {
        log::info!(
            "[MOCK] Toggle visibility for folder {}. Public: {}",
            folder_id,
            make_public
        );
        FolderMetadata {
            id: folder_id,
            name: "Mock Folder".to_string(),
            parent_id: None,
            username: if make_public { desired_username } else { None },
            is_public: make_public,
            group_id: None,
            display_order: 0,
            role: None,
        }
    } else {
        let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

        let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
        let (channel_id, access_hash) = match &peer {
            Peer::Channel(c) => (
                c.raw.id,
                c.raw.access_hash.ok_or("No access hash for channel")?,
            ),
            _ => return Err("Only channels (folders) can be toggled.".to_string()),
        };

        let input_channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
            channel_id,
            access_hash,
        });

        // Extract channel name from the resolved peer for the return value
        let channel_name = match &peer {
            Peer::Channel(c) => c
                .raw
                .title
                .replace(" [TR]", "")
                .replace(" [tr]", "")
                .replace(" [TD]", "")
                .replace(" [td]", "")
                .trim()
                .to_string(),
            _ => "Folder".to_string(),
        };

        if make_public {
            // Generate a username from the desired_username or channel title.
            // If desired_username is provided AND non-empty, use it directly;
            // otherwise auto-generate from the channel title.
            let username = if let Some(ref u) = desired_username {
                if !u.is_empty() {
                    Some(u.clone())
                } else {
                    None // empty string → fall through to auto-generation below
                }
            } else {
                None
            };

            let username = match username {
                Some(given) => {
                    // User-provided username: check availability first
                    let available = client
                        .invoke(&tl::functions::channels::CheckUsername {
                            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                                channel_id,
                                access_hash,
                            }),
                            username: given.clone(),
                        })
                        .await
                        .map_err(|e| {
                            format!("Failed to check username availability: {}", map_error(e))
                        })?;
                    if !available {
                        return Err(format!(
                            "Username '{}' is not available. Try a different one.",
                            given
                        ));
                    }
                    given
                }
                None => {
                    // Auto-generate username from channel title
                    // channel_name already has [TD] stripped above
                    let mut base = channel_name
                        .clone()
                        .to_lowercase()
                        .chars()
                        .filter(|c| c.is_alphanumeric() || *c == '_')
                        .take(30)
                        .collect::<String>();
                    if base.len() < 5 {
                        let suffix: String = (0..6)
                            .map(|_| char::from(b'a' + (rand::random::<u8>() % 26)))
                            .collect();
                        base = format!("{}_{}", base, suffix);
                    }
                    // Try to find an available username
                    let mut candidate = base.clone();
                    for attempt in 1..=10 {
                        match client
                            .invoke(&tl::functions::channels::CheckUsername {
                                channel: tl::enums::InputChannel::Channel(
                                    tl::types::InputChannel {
                                        channel_id,
                                        access_hash,
                                    },
                                ),
                                username: candidate.clone(),
                            })
                            .await
                        {
                            Ok(true) => break,
                            _ => {
                                candidate = format!("{}{}", base, attempt);
                                if attempt == 10 {
                                    return Err(
                                        "Could not find an available username after 10 attempts"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                    candidate
                }
            };

            log::info!("Setting channel {} username to '{}'", channel_id, username);
            client
                .invoke(&tl::functions::channels::UpdateUsername {
                    channel: input_channel,
                    username: username.clone(),
                })
                .await
                .map_err(|e| format!("Failed to set username: {}", map_error(e)))?;

            FolderMetadata {
                id: channel_id,
                name: channel_name,
                parent_id: None,
                username: Some(username),
                is_public: true,
                group_id: None,
                display_order: 0,
                role: None,
            }
        } else {
            // Make private: remove username
            log::info!("Removing username from channel {}", channel_id);
            client
                .invoke(&tl::functions::channels::UpdateUsername {
                    channel: input_channel,
                    username: String::new(),
                })
                .await
                .map_err(|e| format!("Failed to remove username: {}", map_error(e)))?;

            FolderMetadata {
                id: channel_id,
                name: channel_name,
                parent_id: None,
                username: None,
                is_public: false,
                group_id: None,
                display_order: 0,
                role: None,
            }
        }
    };

    // Update SQLite cache
    let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
    let mut stmt = conn
        .prepare("UPDATE folder_metadata SET username = ?, is_public = ? WHERE channel_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, folder.username.as_deref()))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((2, if folder.is_public { 1 } else { 0 }))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((3, folder.id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.next().map_err(|e: sqlite::Error| e.to_string())?;

    // Retrieve group_id and display_order from DB to ensure they are returned correctly
    let mut fm_stmt = conn
        .prepare("SELECT group_id, display_order FROM folder_metadata WHERE channel_id = ?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    fm_stmt
        .bind((1, folder.id))
        .map_err(|e: sqlite::Error| e.to_string())?;
    if let sqlite::State::Row = fm_stmt.next().map_err(|e: sqlite::Error| e.to_string())? {
        folder.group_id = fm_stmt
            .read::<Option<i64>, _>("group_id")
            .ok()
            .flatten()
            .map(|id| id as i32);
        folder.display_order = fm_stmt
            .read::<i64, _>("display_order")
            .map_err(|e: sqlite::Error| e.to_string())? as i32;
    }

    Ok(folder)
}

/// Export a Telegram invite link for a folder (channel).
/// For public channels, returns the t.me/username link directly.
/// For private channels, exports a hash-based invite link via the API.
#[derive(Debug, Serialize)]
pub struct FolderInviteInfo {
    pub link: String,
    pub is_public: bool,
    pub username: Option<String>,
}

#[tauri::command]
pub async fn cmd_export_folder_invite(
    folder_id: i64,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
) -> Result<FolderInviteInfo, String> {
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::require_owner_role(&conn, folder_id)?;
    }

    let client_opt = { state.client.lock().await.clone() };

    #[cfg(debug_assertions)]
    if client_opt.is_none() {
        log::info!("[MOCK] Export invite for folder {}", folder_id);
        return Ok(FolderInviteInfo {
            link: "https://t.me/joinchat/mock-invite-hash".to_string(),
            is_public: false,
            username: None,
        });
    }
    let client = client_opt.ok_or_else(|| "Client not connected".to_string())?;

    let peer = resolve_peer(&client, Some(folder_id), &state.peer_cache).await?;
    let (channel_id, access_hash) = match &peer {
        Peer::Channel(c) => (
            c.raw.id,
            c.raw.access_hash.ok_or("No access hash for channel")?,
        ),
        _ => return Err("Only channels (folders) can have invite links.".to_string()),
    };

    // Check if channel already has a public username (use the resolved peer directly)
    let username: Option<String> = match &peer {
        Peer::Channel(c) => c.raw.username.clone(),
        _ => None,
    };

    if let Some(ref uname) = username {
        // Public channel: return the t.me/username link
        Ok(FolderInviteInfo {
            link: format!("https://t.me/{}", uname),
            is_public: true,
            username: Some(uname.clone()),
        })
    } else {
        // Private channel: export an invite link
        let result = client
            .invoke(&tl::functions::messages::ExportChatInvite {
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id,
                    access_hash,
                }),
                legacy_revoke_permanent: false,
                request_needed: false,
                expire_date: None,
                usage_limit: None,
                title: None,
                subscription_pricing: None,
            })
            .await
            .map_err(|e| format!("Failed to export invite: {}", map_error(e)))?;

        let link = match result {
            tl::enums::ExportedChatInvite::ChatInviteExported(c) => c.link,
            tl::enums::ExportedChatInvite::ChatInvitePublicJoinRequests => {
                return Err("Public join request channels do not have a custom private invite link. Share the public username directly instead.".to_string());
            }
        };

        Ok(FolderInviteInfo {
            link,
            is_public: false,
            username: None,
        })
    }
}

#[derive(Clone, serde::Serialize)]
struct RemoteProgressPayload {
    id: String,
    phase: &'static str,
    percent: u8,
    speed: u64,
    uploaded_bytes: u64,
    total_bytes: u64,
}

#[tauri::command]
pub async fn cmd_upload_from_url(
    url: String,
    folder_id: Option<i64>,
    transfer_id: String,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    db_pool: State<'_, DbConnection>,
    bw_state: State<'_, Arc<BandwidthManager>>,
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<String, String> {
    {
        let conn = db_pool.lock().map_err(|_| "DB poisoned".to_string())?;
        crate::commands::logical_channels::ensure_channel_can_upload(&conn, folder_id)?;
    }

    let mut client_builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10));

    if net_config.is_proxy_active() {
        if let Some(proxy_addr) = net_config.proxy_addr() {
            let proxy_obj = {
                let proxy_cfg = net_config.proxy.read().unwrap();
                if !proxy_cfg.username.is_empty() {
                    let encoded_user = urlencoding::encode(&proxy_cfg.username);
                    let encoded_pass = urlencoding::encode(&proxy_cfg.password);
                    format!("socks5://{}:{}@{}", encoded_user, encoded_pass, proxy_addr)
                } else {
                    format!("socks5://{}", proxy_addr)
                }
            };
            if let Ok(p) = reqwest::Proxy::all(&proxy_obj) {
                client_builder = client_builder.proxy(p);
            }
        }
    }

    let client = client_builder.build().map_err(|e| e.to_string())?;

    let res = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let headers = res.headers();

    // Reject HTML pages — they're download gateways, not actual files
    if let Some(ct) = headers.get(reqwest::header::CONTENT_TYPE) {
        let ct_str = ct.to_str().unwrap_or_default().to_lowercase();
        if ct_str.contains("text/html") {
            return Err("URL returned an HTML page, not a downloadable file. The server may require a direct download link or authentication.".to_string());
        }
    }

    // Prefer Content-Disposition filename over URL path extraction
    let server_filename: Option<String> = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|header_value| {
            // Parse RFC 6266/5987 Content-Disposition: attachment; filename="..." or filename*=UTF-8''...
            // Look for filename* first (RFC 5987), then filename
            if let Some(encoded) = header_value
                .split(';')
                .map(|p| p.trim())
                .find(|p| p.starts_with("filename*="))
                .and_then(|p| p.strip_prefix("filename*="))
            {
                // filename*=UTF-8''percent%20encoded
                if let Some((_charset, value)) = encoded.split_once('\'') {
                    let value = value.split('\'').last().unwrap_or(value);
                    urlencoding::decode(value)
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map(|s| s.into_owned())
                } else {
                    None
                }
            } else {
                header_value
                    .split(';')
                    .map(|p| p.trim())
                    .find(|p| p.starts_with("filename="))
                    .and_then(|p| p.strip_prefix("filename="))
                    .map(|f| f.trim_matches('"').to_string())
                    .filter(|f| !f.is_empty())
            }
        });

    let known_size: Option<u64> = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let temp_dir = std::env::temp_dir();

    if let Some(sz) = known_size {
        if sz > 2_147_483_648 {
            return Err("Exceeds 2GB Telegram limit.".into());
        }
        let free_space = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            move || {
                let disks = sysinfo::Disks::new_with_refreshed_list();
                disks
                    .iter()
                    .filter(|d| temp_dir.starts_with(d.mount_point()))
                    .map(|d| d.available_space())
                    .next()
                    .unwrap_or(u64::MAX)
            }
        })
        .await
        .map_err(|e| format!("Disk check panicked: {}", e))?;
        if free_space < sz + 52_428_800 {
            return Err("Insufficient disk space in temp directory.".to_string());
        }
        bw_state.try_reserve_down(sz)?;
        if let Err(e) = bw_state.try_reserve_up(sz) {
            bw_state.release_down(sz);
            return Err(e);
        }
    }

    let display_total = known_size.unwrap_or(0); // 0 = unknown size to frontend
    let _ = app_handle.emit(
        "remote-upload-progress",
        RemoteProgressPayload {
            id: transfer_id.clone(),
            phase: "downloading",
            percent: 0,
            speed: 0,
            uploaded_bytes: 0,
            total_bytes: display_total,
        },
    );

    let temp_file_path = temp_dir.join(format!("tg_drive_{}.tmp", transfer_id));
    let temp_file_str = temp_file_path.to_string_lossy().to_string();

    let mut downloaded = 0u64;
    let mut range_supported = false;

    if let Some(accept_ranges) = headers.get(reqwest::header::ACCEPT_RANGES) {
        if accept_ranges.to_str().unwrap_or_default() == "bytes" {
            range_supported = true;
        }
    }

    if temp_file_path.exists() {
        if range_supported && known_size.is_some() {
            if let Ok(metadata) = std::fs::metadata(&temp_file_path) {
                downloaded = metadata.len();
                let sz = known_size.unwrap();
                if downloaded >= sz {
                    downloaded = sz;
                }
            }
        } else {
            // No resumption without both range support and a known total size
            let _ = std::fs::remove_file(&temp_file_path);
        }
    }

    let need_download = known_size.map_or(true, |sz| downloaded < sz);

    let stream_res = if downloaded > 0 && need_download {
        let req = client
            .get(&url)
            .header(reqwest::header::RANGE, format!("bytes={}-", downloaded));
        match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if let Some(sz) = known_size {
                    bw_state.release_down(sz);
                    bw_state.release_up(sz);
                }
                return Err(e.to_string());
            }
        }
    } else {
        res
    };

    let mut file = if downloaded > 0 && need_download {
        let status = stream_res.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            match tokio::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&temp_file_path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    if let Some(sz) = known_size {
                        bw_state.release_down(sz);
                        bw_state.release_up(sz);
                    }
                    return Err(e.to_string());
                }
            }
        } else {
            downloaded = 0;
            match tokio::fs::File::create(&temp_file_path).await {
                Ok(f) => f,
                Err(e) => {
                    if let Some(sz) = known_size {
                        bw_state.release_down(sz);
                        bw_state.release_up(sz);
                    }
                    return Err(e.to_string());
                }
            }
        }
    } else if !need_download {
        match tokio::fs::OpenOptions::new()
            .read(true)
            .open(&temp_file_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                if let Some(sz) = known_size {
                    bw_state.release_down(sz);
                    bw_state.release_up(sz);
                }
                return Err(e.to_string());
            }
        }
    } else {
        match tokio::fs::File::create(&temp_file_path).await {
            Ok(f) => f,
            Err(e) => {
                if let Some(sz) = known_size {
                    bw_state.release_down(sz);
                    bw_state.release_up(sz);
                }
                return Err(e.to_string());
            }
        }
    };

    if need_download {
        let mut stream = stream_res.bytes_stream();
        let mut last_emit_time = std::time::Instant::now();
        let mut last_emit_bytes = downloaded;

        while let Some(chunk_result) = futures::StreamExt::next(&mut stream).await {
            if state
                .cancelled_transfers
                .read()
                .await
                .contains(&transfer_id)
            {
                state.cancelled_transfers.write().await.remove(&transfer_id);
                drop(file);
                let _ = tokio::fs::remove_file(&temp_file_path).await;
                if let Some(sz) = known_size {
                    bw_state.release_down(sz);
                    bw_state.release_up(sz);
                }
                return Err("Transfer cancelled".to_string());
            }

            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    if let Some(sz) = known_size {
                        bw_state.release_down(sz);
                        bw_state.release_up(sz);
                    }
                    return Err(e.to_string());
                }
            };

            if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
                if let Some(sz) = known_size {
                    bw_state.release_down(sz);
                    bw_state.release_up(sz);
                }
                return Err(e.to_string());
            }
            downloaded += chunk.len() as u64;

            // Dynamic 2GB check when total size is unknown
            if known_size.is_none() && downloaded > 2_147_483_648 {
                drop(file);
                let _ = tokio::fs::remove_file(&temp_file_path).await;
                return Err("Downloaded file exceeds 2GB Telegram limit.".to_string());
            }

            let now = std::time::Instant::now();
            let dt = now.duration_since(last_emit_time).as_secs_f64();
            let emit_total = known_size.unwrap_or(downloaded);
            let emit_done = known_size.map_or(false, |sz| downloaded >= sz);
            if dt >= 0.25 || emit_done {
                let speed = if dt > 0.0 {
                    ((downloaded - last_emit_bytes) as f64 / dt) as u64
                } else {
                    0
                };
                let percent = if let Some(sz) = known_size {
                    if sz > 0 {
                        ((downloaded as f64 / sz as f64) * 100.0).min(99.0) as u8
                    } else {
                        0
                    }
                } else {
                    0u8
                };

                let _ = app_handle.emit(
                    "remote-upload-progress",
                    RemoteProgressPayload {
                        id: transfer_id.clone(),
                        phase: "downloading",
                        percent,
                        speed,
                        uploaded_bytes: downloaded,
                        total_bytes: emit_total,
                    },
                );
                last_emit_time = now;
                last_emit_bytes = downloaded;
            }

            let dl_limit = net_config.download_limit_bytes_per_sec();
            if dl_limit > 0 {
                let elapsed = last_emit_time.elapsed().as_secs_f64().max(0.001);
                let current_rate = (downloaded - last_emit_bytes) as f64 / elapsed;
                if current_rate > dl_limit as f64 {
                    let sleep_ms =
                        ((current_rate / dl_limit as f64 - 1.0) * elapsed * 1000.0) as u64;
                    if sleep_ms > 0 && sleep_ms < 5000 {
                        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                    }
                }
            }
        }

        if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
            if let Some(sz) = known_size {
                bw_state.release_down(sz);
                bw_state.release_up(sz);
            }
            return Err(e.to_string());
        }
        if let Err(e) = file.sync_all().await {
            if let Some(sz) = known_size {
                bw_state.release_down(sz);
                bw_state.release_up(sz);
            }
            return Err(e.to_string());
        }
    }

    drop(file);
    if let Some(sz) = known_size {
        bw_state.release_down(sz);
        // Release the upfront upload reservation — we'll re-reserve based on actual size below
        bw_state.release_up(sz);
    }

    // Determine actual file size from disk (authoritative, works even without Content-Length)
    let actual_size = tokio::fs::metadata(&temp_file_path)
        .await
        .map_err(|e| format!("Failed to read downloaded file metadata: {}", e))?
        .len();

    if actual_size == 0 {
        let _ = tokio::fs::remove_file(&temp_file_path).await;
        return Err("Downloaded file is empty".to_string());
    }

    if actual_size > 2_147_483_648 {
        let _ = tokio::fs::remove_file(&temp_file_path).await;
        return Err("Downloaded file exceeds 2GB Telegram limit.".to_string());
    }

    // Reserve upload bandwidth based on the real file size (handles both known and unknown upfront)
    if let Err(e) = bw_state.try_reserve_up(actual_size) {
        let _ = tokio::fs::remove_file(&temp_file_path).await;
        return Err(e);
    }

    let client_opt = { state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => {
            bw_state.release_up(actual_size);
            let _ = tokio::fs::remove_file(&temp_file_path).await;
            return Err("Client not connected".to_string());
        }
    };

    let _ = app_handle.emit(
        "remote-upload-progress",
        RemoteProgressPayload {
            id: transfer_id.clone(),
            phase: "uploading",
            percent: 0,
            speed: 0,
            uploaded_bytes: 0,
            total_bytes: actual_size,
        },
    );

    let (mut reader, file_size, bytes_counter) = match ProgressReader::new(&temp_file_str).await {
        Ok(res) => res,
        Err(e) => {
            bw_state.release_up(actual_size);
            let _ = tokio::fs::remove_file(&temp_file_path).await;
            return Err(e);
        }
    };

    let cancelled = state.cancelled_transfers.clone();
    let progress_tid = transfer_id.clone();
    let progress_handle = app_handle.clone();
    let progress_counter = bytes_counter.clone();
    let progress_task = tokio::spawn(async move {
        let mut last_bytes: u64 = 0;
        let mut last_time = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let current = progress_counter.load(std::sync::atomic::Ordering::Relaxed);
            let now = std::time::Instant::now();
            let dt = now.duration_since(last_time).as_secs_f64();
            let speed = if dt > 0.0 {
                ((current - last_bytes) as f64 / dt) as u64
            } else {
                0
            };
            let percent = if file_size > 0 {
                ((current as f64 / file_size as f64) * 100.0).min(99.0) as u8
            } else {
                0
            };

            let _ = progress_handle.emit(
                "remote-upload-progress",
                RemoteProgressPayload {
                    id: progress_tid.clone(),
                    phase: "uploading",
                    percent,
                    speed,
                    uploaded_bytes: current,
                    total_bytes: file_size,
                },
            );

            last_bytes = current;
            last_time = now;

            if current >= file_size {
                break;
            }
            if cancelled.read().await.contains(&progress_tid) {
                break;
            }
        }
    });

    if state
        .cancelled_transfers
        .read()
        .await
        .contains(&transfer_id)
    {
        state.cancelled_transfers.write().await.remove(&transfer_id);
        progress_task.abort();
        bw_state.release_up(actual_size);
        let _ = tokio::fs::remove_file(&temp_file_path).await;
        return Err("Transfer cancelled".to_string());
    }

    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    get_upload_cancellations()
        .lock()
        .unwrap()
        .insert(transfer_id.clone(), cancel_tx);

    let client_clone = client.clone();
    let file_name = server_filename.unwrap_or_else(|| {
        reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| {
                u.path_segments()
                    .and_then(|segs| segs.last())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "remote_file".to_string())
    });

    let mut upload_task = tokio::spawn(async move {
        client_clone
            .upload_stream(&mut reader, file_size as usize, file_name)
            .await
    });

    let uploaded_file = {
        tokio::select! {
            res = &mut upload_task => {
                get_upload_cancellations().lock().unwrap().remove(&transfer_id);
                match res {
                    Ok(Ok(file)) => file,
                    Ok(Err(e)) => {
                        bw_state.release_up(actual_size);
                        progress_task.abort();
                        let _ = tokio::fs::remove_file(&temp_file_path).await;
                        return Err(map_error(e));
                    }
                    Err(e) => {
                        bw_state.release_up(actual_size);
                        progress_task.abort();
                        let _ = tokio::fs::remove_file(&temp_file_path).await;
                        return Err(format!("Task join error: {}", e));
                    }
                }
            }
            _ = cancel_rx.changed() => {
                log::info!("Aborting remote upload task for transfer ID: {}", transfer_id);
                upload_task.abort();
                state.cancelled_transfers.write().await.remove(&transfer_id);
                progress_task.abort();
                bw_state.release_up(actual_size);
                let _ = tokio::fs::remove_file(&temp_file_path).await;
                return Err("Transfer cancelled".to_string());
            }
        }
    };

    progress_task.abort();

    let message = InputMessage::new().text("").file(uploaded_file);

    let peer = match resolve_peer(&client, folder_id, &state.peer_cache).await {
        Ok(p) => p,
        Err(e) => {
            bw_state.release_up(actual_size);
            let _ = tokio::fs::remove_file(&temp_file_path).await;
            return Err(e);
        }
    };

    let max_retries = net_config.retry_attempts();
    let base_ms = net_config.retry_base_backoff_ms();
    let max_ms = net_config.retry_max_backoff_ms();
    let respect_flood = net_config.should_respect_flood_wait();
    let mut last_err = String::new();
    let mut send_success = false;

    for attempt in 0..=max_retries {
        match client.send_message(&peer, message.clone()).await {
            Ok(_) => {
                send_success = true;
                break;
            }
            Err(e) => {
                let err = map_error(e);
                log::warn!(
                    "send_message attempt {}/{}: {}",
                    attempt + 1,
                    max_retries + 1,
                    err
                );

                if respect_flood && err.starts_with("FLOOD_WAIT_") {
                    if let Ok(secs) = err.trim_start_matches("FLOOD_WAIT_").parse::<u64>() {
                        let wait = secs.min(300);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        last_err = err;
                        continue;
                    }
                }

                last_err = err;
                if attempt < max_retries {
                    let delay = backoff_ms(attempt, base_ms, max_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }

    let _ = tokio::fs::remove_file(&temp_file_path).await;

    if send_success {
        let _ = app_handle.emit(
            "remote-upload-progress",
            RemoteProgressPayload {
                id: transfer_id,
                phase: "uploading",
                percent: 100,
                speed: 0,
                uploaded_bytes: actual_size,
                total_bytes: actual_size,
            },
        );
        Ok("File uploaded successfully".to_string())
    } else {
        bw_state.release_up(actual_size);
        Err(format!(
            "Upload failed after {} attempts: {}",
            max_retries + 1,
            last_err
        ))
    }
}

#[cfg(test)]
mod split_tests {
    use super::{
        adapt_upload_parallelism, checked_upload_part_count, clean_file_uri, ordered_part_ids,
        parse_part_name, split_part_caption, split_part_name, telegram_upload_chunk_size,
        upload_pool_plan, SPLIT_PART_SIZE, UPLOAD_MAIN_SINGLE_WORKERS, UPLOAD_PART_WORKERS,
    };
    use std::collections::HashMap;

    #[test]
    fn file_uri_normalization_decodes_ios_paths() {
        assert_eq!(
            clean_file_uri("file:///var/mobile/Containers/Data/My%20File.pdf"),
            "/var/mobile/Containers/Data/My File.pdf"
        );
        assert_eq!(
            clean_file_uri("/var/mobile/Containers/Data/plain.txt"),
            "/var/mobile/Containers/Data/plain.txt"
        );
    }

    #[test]
    fn upload_pool_matches_reference_connection_policy() {
        assert_eq!(
            upload_pool_plan(true, 8, 1),
            ("media-pool", 8, 4, false, UPLOAD_PART_WORKERS)
        );
        assert_eq!(
            upload_pool_plan(false, 8, 3),
            ("tmp-session-pool", 3, 3, true, UPLOAD_PART_WORKERS)
        );
        assert_eq!(
            upload_pool_plan(false, 8, 1),
            ("main-single", 1, 1, true, UPLOAD_MAIN_SINGLE_WORKERS)
        );
    }

    #[test]
    fn single_file_parallelism_scales_like_reference_uploader() {
        assert_eq!(
            adapt_upload_parallelism("media-pool", 4, 16, 8, None, 10.0),
            (5, 20)
        );
        assert_eq!(
            adapt_upload_parallelism("media-pool", 5, 20, 8, Some(10.0), 10.0),
            (6, 25)
        );
        assert_eq!(
            adapt_upload_parallelism("media-pool", 6, 25, 8, Some(10.0), 8.0),
            (5, 19)
        );
        assert_eq!(
            adapt_upload_parallelism("main-single", 1, 5, 1, None, 10.0),
            (1, 6)
        );
        assert_eq!(
            adapt_upload_parallelism("main-single", 1, 6, 1, Some(10.0), 8.0),
            (1, 5)
        );
    }

    #[test]
    fn main_single_uses_telethon_adaptive_part_sizes() {
        assert_eq!(
            telegram_upload_chunk_size(100 * 1024 * 1024, "main-single"),
            128 * 1024
        );
        assert_eq!(
            telegram_upload_chunk_size(101 * 1024 * 1024, "main-single"),
            256 * 1024
        );
        assert_eq!(
            telegram_upload_chunk_size(750 * 1024 * 1024, "main-single"),
            256 * 1024
        );
        assert_eq!(
            telegram_upload_chunk_size(751 * 1024 * 1024, "main-single"),
            512 * 1024
        );
        assert_eq!(telegram_upload_chunk_size(1, "media-pool"), 512 * 1024);
    }

    #[test]
    fn split_size_boundaries_are_unambiguous() {
        let decimal_4gb = 4_000_000_000u64;
        assert_eq!(SPLIT_PART_SIZE, 2_000_000_000);
        assert_eq!(decimal_4gb.div_ceil(SPLIT_PART_SIZE), 2);
        assert_eq!(decimal_4gb % SPLIT_PART_SIZE, 0);

        // 4 GiB is 4,294,967,296 bytes, which is larger than decimal 4 GB.
        // With a safe 2,000,000,000-byte Telegram document size it therefore
        // requires a third storage document. TeraRelay still presents one
        // logical 4.29 GB file to the user.
        let binary_4gib = 4u64 * 1024 * 1024 * 1024;
        assert_eq!(binary_4gib, 4_294_967_296);
        assert_eq!(binary_4gib.div_ceil(SPLIT_PART_SIZE), 3);
        assert_eq!(binary_4gib - (2 * SPLIT_PART_SIZE), 294_967_296);
    }

    #[test]
    fn large_part_numbers_round_trip_without_a_three_digit_ceiling() {
        for (caption, index, total) in [
            ("movie.mkv.tgdpart1000-1001", 1000, 1001),
            ("backup.tar.zst.tgdpart001-5000", 1, 5000),
            ("backup.tar.zst.tgdpart5000-5000", 5000, 5000),
            ("data.bin.tgdpart4294967295-4294967295", u32::MAX, u32::MAX),
        ] {
            let base = caption.split(".tgdpart").next().unwrap();
            assert_eq!(parse_part_name(caption), Some((base, index, total, None)));
            assert_eq!(split_part_name(base, index, total), caption);
        }
        let hash = "b".repeat(64);
        let caption = format!("movie.mkv.tgdpart1100-1100#{hash}");
        assert_eq!(
            parse_part_name(&caption),
            Some(("movie.mkv", 1100, 1100, Some(hash.as_str())))
        );
        assert_eq!(
            parse_part_name("data.bin.tgdpart4294967296-4294967296"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn large_sparse_file_ranges_keep_offsets_above_two_terabytes() {
        use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let path =
            std::env::temp_dir().join(format!("terarelay-sparse-{}.bin", rand::random::<u64>()));
        let _cleanup = Cleanup(path.clone());
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.set_len(2_199_023_255_552).await.unwrap();
        let offset = 2_199_023_255_520;
        let marker = b"terarelay-large-offset-marker-12";
        assert_eq!(marker.len(), 32);
        file.seek(std::io::SeekFrom::Start(offset)).await.unwrap();
        file.write_all(marker).await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut reader = super::ProgressReader::new_range(
            path.to_str().unwrap(),
            offset,
            32,
            counter.clone(),
            true,
        )
        .await
        .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, marker);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 32);
        use sha2::Digest;
        let expected = format!("{:x}", sha2::Sha256::digest(marker));
        assert_eq!(reader.finalize_hash(), Some(expected));
        assert_eq!(
            tokio::fs::metadata(&path).await.unwrap().len(),
            2_199_023_255_552
        );
    }

    #[test]
    fn legacy_part_candidates_reject_mixed_generations_and_unproven_duplicates() {
        let mut found = HashMap::new();
        super::insert_part_candidate(&mut found, 1, (20, 10, Some("b".repeat(64))), true).unwrap();
        super::insert_part_candidate(&mut found, 2, (12, 5, Some("a".repeat(64))), true).unwrap();
        // An older conflicting candidate must still be checked after all
        // declared indices were found in the newest messages.
        assert!(
            super::insert_part_candidate(&mut found, 1, (11, 10, Some("a".repeat(64))), true)
                .is_err()
        );
        let mut identical = HashMap::new();
        super::insert_part_candidate(&mut identical, 1, (20, 10, Some("a".repeat(64))), true)
            .unwrap();
        super::insert_part_candidate(&mut identical, 1, (11, 10, Some("a".repeat(64))), true)
            .unwrap();
        assert_eq!(identical[&1].0, 20);
        assert!(super::insert_part_candidate(
            &mut identical,
            1,
            (10, 9, Some("a".repeat(64))),
            true
        )
        .is_err());
        assert!(super::insert_part_candidate(&mut identical, 1, (9, 10, None), true).is_err());
        let mut old = HashMap::new();
        super::insert_part_candidate(&mut old, 1, (10, 10, None), true).unwrap();
        assert!(super::insert_part_candidate(&mut old, 1, (9, 10, None), true).is_err());
        assert!(super::legacy_source_part_matches(10, Some("a"), 10, Some("b"), false).is_err());
        assert!(super::legacy_source_part_matches(10, Some("a"), 10, None, false).is_err());
        super::legacy_source_part_matches(10, Some("a"), 10, Some("a"), false).unwrap();
        super::legacy_source_part_matches(10, None, 10, None, true).unwrap();
    }

    #[test]
    fn large_part_ordering_uses_actual_messages_and_reports_gaps() {
        let mut found = HashMap::new();
        found.insert(u32::MAX, (99, 1, None));
        found.insert(2, (22, 1, None));
        found.insert(1, (11, 1, None));
        assert_eq!(
            ordered_part_ids("file", u32::MAX, &found, false).unwrap(),
            vec![11, 22, 99]
        );
        assert!(ordered_part_ids("file", u32::MAX, &found, true)
            .unwrap_err()
            .contains("part 3/"));
        let complete: HashMap<_, _> = (1..=5000).rev().map(|i| (i, (i as i32, 1, None))).collect();
        assert_eq!(
            ordered_part_ids("file", 5000, &complete, true).unwrap(),
            (1..=5000).collect::<Vec<i32>>()
        );
        assert_eq!(
            checked_upload_part_count(10_000_000_000_000, 2_000_000_000).unwrap(),
            5000
        );
        assert!(checked_upload_part_count(u64::MAX, 1).is_err());
        assert!(checked_upload_part_count(1, 0).is_err());
    }

    #[test]
    fn part_name_round_trip() {
        assert_eq!(
            split_part_name("movie.mkv", 2, 5),
            "movie.mkv.tgdpart002-005"
        );
        assert_eq!(
            parse_part_name("movie.mkv.tgdpart002-005"),
            Some(("movie.mkv", 2, 5, None))
        );
        assert_eq!(
            parse_part_name(&split_part_name("a b (1).tar.gz", 999, 999)),
            Some(("a b (1).tar.gz", 999, 999, None))
        );
    }

    #[test]
    fn part_name_rejects_invalid() {
        assert_eq!(parse_part_name("movie.mkv"), None);
        assert_eq!(parse_part_name("movie.mkv.tgdpart000-005"), None); // idx 0
        assert_eq!(parse_part_name("movie.mkv.tgdpart006-005"), None); // idx > total
        assert_eq!(parse_part_name("movie.mkv.tgdpart01-005"), None); // not 3 digits
        assert_eq!(parse_part_name("movie.mkv.tgdpart001-0055"), None); // trailing junk
        assert_eq!(parse_part_name("movie.mkv.tgdpartabc-005"), None);
        assert_eq!(parse_part_name(".tgdpart001-005"), None); // empty base
    }

    #[test]
    fn part_name_with_checksum() {
        let hash = "a".repeat(64);
        let caption = split_part_caption("movie.mkv", 2, 5, Some(&hash));
        assert_eq!(caption, format!("movie.mkv.tgdpart002-005#{}", hash));
        assert_eq!(
            parse_part_name(&caption),
            Some(("movie.mkv", 2, 5, Some(hash.as_str())))
        );
        // Without hash, split_part_caption == split_part_name
        assert_eq!(
            split_part_caption("movie.mkv", 2, 5, None),
            split_part_name("movie.mkv", 2, 5)
        );

        // Malformed hashes invalidate the whole part name
        assert_eq!(
            parse_part_name(&format!("movie.mkv.tgdpart002-005#{}", "a".repeat(63))),
            None
        ); // too short
        assert_eq!(
            parse_part_name(&format!("movie.mkv.tgdpart002-005#{}", "A".repeat(64))),
            None
        ); // uppercase
        assert_eq!(
            parse_part_name(&format!("movie.mkv.tgdpart002-005#{}", "g".repeat(64))),
            None
        ); // not hex
        assert_eq!(parse_part_name("movie.mkv.tgdpart002-005#"), None); // empty hash
    }

    #[test]
    fn resume_skips_valid_parts() {
        use std::collections::HashMap;
        // 35 bytes, parts of 10 -> expected sizes 10,10,10,5
        let (size, part_size, total) = (35u64, 10u64, 4u32);

        // Nothing on Telegram yet: upload everything
        let empty = HashMap::new();
        assert_eq!(
            super::parts_to_upload(size, part_size, total, &empty),
            (vec![1, 2, 3, 4], vec![], 0)
        );

        // Parts 1 and 4 already uploaded with correct sizes: skip them
        let mut existing = HashMap::new();
        existing.insert(1u32, (101i32, 10u64, None));
        existing.insert(4u32, (104i32, 5u64, None));
        assert_eq!(
            super::parts_to_upload(size, part_size, total, &existing),
            (vec![2, 3], vec![], 15)
        );

        // Part 2 exists but with a wrong size: delete and re-upload it
        existing.insert(2u32, (102i32, 7u64, None));
        assert_eq!(
            super::parts_to_upload(size, part_size, total, &existing),
            (vec![2, 3], vec![102], 15)
        );

        // Everything valid: nothing to upload
        let mut all = HashMap::new();
        for (i, sz) in [(1u32, 10u64), (2, 10), (3, 10), (4, 5)] {
            all.insert(i, (100 + i as i32, sz, None));
        }
        assert_eq!(
            super::parts_to_upload(size, part_size, total, &all),
            (vec![], vec![], 35)
        );
    }

    /// Split a file into range readers (the upload path) and concatenate what
    /// they yield (the download path): must reproduce the file byte-for-byte.
    #[tokio::test]
    async fn range_readers_reassemble_file() {
        use tokio::io::AsyncReadExt;

        let path = std::env::temp_dir().join("tgd_split_roundtrip_test.bin");
        let path_str = path.to_string_lossy().to_string();

        // 35_003 bytes of deterministic data, 10_000-byte parts -> 4 parts,
        // last one short (5_003 bytes)
        let original: Vec<u8> = (0..35_003u64)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8)
            .collect();
        tokio::fs::write(&path, &original).await.unwrap();

        let size = original.len() as u64;
        let part_size: u64 = 10_000;
        let total_parts = size.div_ceil(part_size);
        assert_eq!(total_parts, 4);

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut merged: Vec<u8> = Vec::with_capacity(original.len());
        for idx in 1..=total_parts {
            let offset = (idx - 1) * part_size;
            let len = part_size.min(size - offset);
            let mut reader =
                super::ProgressReader::new_range(&path_str, offset, len, counter.clone(), true)
                    .await
                    .unwrap();
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf.len() as u64, len, "part {} length", idx);

            // The reader's checksum must match hashing the range directly
            use sha2::Digest;
            let expected = format!("{:x}", sha2::Sha256::digest(&buf));
            assert_eq!(
                reader.finalize_hash().as_deref(),
                Some(expected.as_str()),
                "part {} hash",
                idx
            );

            merged.extend_from_slice(&buf);
        }

        assert_eq!(merged, original);
        // Shared counter must report the whole file (drives the progress bar)
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), size);

        let _ = tokio::fs::remove_file(&path).await;
    }
}
