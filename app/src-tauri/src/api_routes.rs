use crate::bandwidth::BandwidthManager;
use crate::commands::preview::THUMBNAIL_EXTS;
use crate::commands::utils::{map_error, resolve_peer};
use crate::commands::TelegramState;
use crate::commands::{create_folder_inner, delete_folder_inner, rename_folder_inner};
use crate::models::FolderMetadata;
use crate::vpn_optimizer::NetworkConfig;
use actix_multipart::Multipart;
use actix_web::web::Bytes;
use actix_web::{delete, get, patch, post, web, HttpRequest, HttpResponse, Responder};
use futures::{StreamExt, TryStreamExt};
use grammers_client::types::{Media, Peer};
use grammers_client::InputMessage;
use grammers_tl_types as tl;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWriteExt};

/// Shared state for the API server — holds the key hash for auth checks
pub struct ApiState {
    pub key_hash: Option<String>,
}

/// Cache directory paths used by the API server for cleanup operations.
/// The thumbnail and preview caches live on disk and can become stale
/// when files are moved (forwarded → new message IDs).
pub struct CacheDirs {
    pub thumbnail_dir: std::path::PathBuf,
    pub preview_dir: std::path::PathBuf,
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
}

fn json_error(code: &str, message: &str, status: u16) -> HttpResponse {
    let body = ErrorBody {
        error: ErrorDetail {
            code: code.to_string(),
            message: message.to_string(),
        },
    };
    HttpResponse::build(actix_web::http::StatusCode::from_u16(status).unwrap()).json(body)
}

// Telegram's iterator can return clean EOF after fewer bytes than a
// document advertises. Never package an incomplete (or oversized) document
// as a successful ZIP entry. Photos have no reliable declared byte count.
fn archive_document_length_matches(expected: Option<u64>, received: usize) -> bool {
    expected.is_none_or(|bytes| u64::try_from(received).ok() == Some(bytes))
}

// Telegram supplies the document's display name. A ZIP member name is a
// portable extraction path, so never include directory components, Windows
// separators, device names or control characters from that untrusted source.
fn safe_archive_entry_name(raw: &str, message_id: i32) -> String {
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = leaf
        .chars()
        .take(180)
        .map(|character| {
            if character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    let name = name.trim_end_matches([' ', '.']);
    if name.is_empty() || name == "." || name == ".." {
        return format!("file_{message_id}.bin");
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return format!("_{name}");
    }
    name.to_string()
}

// ZIP allows duplicate member names but ordinary extractors may overwrite
// earlier entries. Preserve the first safe name and give later collisions a
// stable message-ID suffix; compare case-insensitively for Windows clients.
fn unique_archive_entry_name(
    safe_name: String,
    message_id: i32,
    used: &mut HashSet<String>,
) -> String {
    if used.insert(safe_name.to_lowercase()) {
        return safe_name;
    }
    let (stem, extension) = match safe_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, format!(".{ext}")),
        _ => (safe_name.as_str(), String::new()),
    };
    // There are only used.len() occupied names. Among the next len+1
    // distinct candidates at least one must be free.
    for collision in 0..=used.len() {
        let candidate = if collision == 0 {
            format!("{stem}_{message_id}{extension}")
        } else {
            format!("{stem}_{message_id}_{collision}{extension}")
        };
        if used.insert(candidate.to_lowercase()) {
            return candidate;
        }
    }
    unreachable!("A finite set cannot cover all generated ZIP member names")
}

#[cfg(test)]
mod archive_integrity_tests {
    #[test]
    fn rest_archive_download_must_not_turn_transport_errors_into_end_of_file() {
        let source = include_str!("api_routes.rs");
        let handler = source
            .split_once("\"archive\" => {")
            .unwrap()
            .1
            .split_once("let temp_zip_path =")
            .unwrap()
            .0;
        assert!(
            !handler.contains("download_iter.next().await.ok().flatten()"),
            "archive download errors must fail the request, not generate a truncated ZIP"
        );
    }

    #[test]
    fn rest_archive_must_reject_a_short_clean_document_download() {
        let source = include_str!("api_routes.rs");
        let archive = source
            .split_once("\"archive\" => {")
            .unwrap()
            .1
            .split_once("let temp_zip_path =")
            .unwrap()
            .0;
        assert!(
            archive.contains("ARCHIVE_INCOMPLETE_DOWNLOAD"),
            "clean Telegram EOF before a document's declared size must fail the entire ZIP"
        );
    }

    #[test]
    fn document_length_check_accepts_only_exact_declared_size() {
        use super::archive_document_length_matches;
        assert!(archive_document_length_matches(Some(0), 0));
        assert!(archive_document_length_matches(Some(5), 5));
        assert!(!archive_document_length_matches(Some(5), 4));
        assert!(!archive_document_length_matches(Some(5), 6));
        assert!(!archive_document_length_matches(Some(0), 1));
        assert!(archive_document_length_matches(None, 5));
    }

    #[test]
    fn rest_archive_must_store_safe_leaf_names_not_remote_paths() {
        let source = include_str!("api_routes.rs");
        let archive = source
            .split_once("\"archive\" => {")
            .unwrap()
            .1
            .split_once("let temp_zip_path =")
            .unwrap()
            .0;
        assert!(
            archive.contains("safe_archive_entry_name("),
            "ZIP members must use sanitized leaf names, not a remote document's path"
        );
    }

    #[test]
    fn archive_zip_member_names_are_safe_and_portable() {
        use super::safe_archive_entry_name;
        assert_eq!(safe_archive_entry_name("notes.txt", 17), "notes.txt");
        assert_eq!(safe_archive_entry_name("folder/notes.txt", 17), "notes.txt");
        assert_eq!(
            safe_archive_entry_name("folder\\notes.txt", 17),
            "notes.txt"
        );
        assert_eq!(safe_archive_entry_name("folder/", 17), "file_17.bin");
        assert_eq!(safe_archive_entry_name("..", 17), "file_17.bin");
        assert_eq!(safe_archive_entry_name("", 17), "file_17.bin");
        assert_eq!(safe_archive_entry_name("CON.txt", 17), "_CON.txt");
        assert_eq!(
            safe_archive_entry_name("file:name?.txt", 17),
            "file_name_.txt"
        );
        assert_eq!(safe_archive_entry_name("notes.txt. ", 17), "notes.txt");
        assert_eq!(safe_archive_entry_name("bad\0name.txt", 17), "bad_name.txt");
    }

    #[test]
    fn rest_archive_must_disambiguate_duplicate_zip_member_names() {
        let source = include_str!("api_routes.rs");
        let archive = source
            .split_once("\"archive\" => {")
            .unwrap()
            .1
            .split_once("let temp_zip_path =")
            .unwrap()
            .0;
        assert!(
            archive.contains("unique_archive_entry_name("),
            "same-named documents must not share a ZIP member path and overwrite when extracted"
        );
    }

    #[test]
    fn archive_member_names_remain_distinct_for_same_named_documents() {
        use super::unique_archive_entry_name;
        let mut used = std::collections::HashSet::new();
        assert_eq!(
            unique_archive_entry_name("report.txt".into(), 11, &mut used),
            "report.txt"
        );
        assert_eq!(
            unique_archive_entry_name("REPORT.TXT".into(), 12, &mut used),
            "REPORT_12.TXT",
            "case-insensitive extraction destinations must not collide"
        );
        assert_eq!(
            unique_archive_entry_name("report.txt".into(), 11, &mut used),
            "report_11.txt"
        );
        assert_eq!(
            unique_archive_entry_name("report.txt".into(), 11, &mut used),
            "report_11_1.txt"
        );
        assert_eq!(
            unique_archive_entry_name("other.txt".into(), 13, &mut used),
            "other.txt",
            "unrelated files should keep their display filenames"
        );
    }

    #[test]
    fn rest_archive_must_not_silently_skip_requested_messages() {
        let source = include_str!("api_routes.rs");
        let archive = source
            .split_once("\"archive\" => {")
            .unwrap()
            .1
            .split_once("let temp_zip_path =")
            .unwrap()
            .0;
        assert!(
            !archive.contains("Err(_) => continue"),
            "Telegram lookup failure must not silently omit a requested archive file"
        );
        assert!(
            archive.contains("ARCHIVE_MESSAGE_MISSING"),
            "missing/deleted/non-media entries must not yield a silently incomplete ZIP"
        );
    }
}

#[cfg(test)]
mod file_pagination_tests {
    #[test]
    fn rest_listing_telegram_iteration_errors_must_not_look_like_complete_results() {
        let source = include_str!("api_routes.rs");
        let listing = source
            .rsplit_once("async fn api_list_files(")
            .unwrap()
            .1
            .split_once("async fn api_get_file(")
            .unwrap()
            .0;
        assert!(
            !listing.contains(".next().await.ok().flatten()"),
            "a failed Telegram listing iterator must not look like complete EOF"
        );
        assert!(
            listing.contains("LIST_FILES_FAILED"),
            "Telegram listing failures must produce an explicit error, not incomplete files"
        );
    }

    #[test]
    fn listing_rejects_malformed_folder_identifiers_instead_of_scanning_home() {
        let source = include_str!("api_routes.rs");
        let listing = source
            .rsplit_once("async fn api_list_files(")
            .unwrap()
            .1
            .split_once("async fn api_get_file(")
            .unwrap()
            .0;
        assert!(
            listing.contains("parse_requested_file_listing_folder_id("),
            "malformed folder_id must return 400 instead of silently becoming Home"
        );
    }

    #[test]
    fn listing_folder_parser_preserves_home_and_valid_ids_but_rejects_malformed_values() {
        use super::parse_requested_file_listing_folder_id;
        for home in [None, Some(""), Some("null"), Some("none"), Some("None")] {
            assert_eq!(parse_requested_file_listing_folder_id(home).unwrap(), None);
        }
        for id in [0i64, -123, i64::MAX, i64::MIN] {
            assert_eq!(
                parse_requested_file_listing_folder_id(Some(&id.to_string())).unwrap(),
                Some(id)
            );
        }
        for invalid in [
            "not-an-id",
            "123=456",
            "9223372036854775808",
            "--1",
            "1.2",
            "NaN",
        ] {
            assert!(
                parse_requested_file_listing_folder_id(Some(invalid)).is_err(),
                "{invalid}: invalid folder IDs must not become Home"
            );
        }
    }

    #[test]
    fn listing_page_offsets_must_not_overflow_before_converting_to_usize() {
        let source = include_str!("api_routes.rs");
        let listing = source
            .rsplit_once("async fn api_list_files(")
            .unwrap()
            .1
            .split_once("async fn api_get_file(")
            .unwrap()
            .0;
        assert!(
            !listing.contains("((page - 1) * limit) as usize"),
            "client-controlled file page and limit must not overflow u32 before pagination"
        );
    }

    #[test]
    fn listing_offset_preserves_large_pages_without_wrapping() {
        assert_eq!(super::files_page_offset(1, 100), 0);
        assert_eq!(super::files_page_offset(3, 20), 40);
        assert_eq!(super::files_page_offset(0, 100), 0);
        assert_eq!(
            super::files_page_offset(u32::MAX, 100),
            usize::try_from(u64::from(u32::MAX - 1) * 100).unwrap_or(usize::MAX),
            "large page indexes must be monotonic and never wrap to an early page"
        );
    }
}

#[cfg(test)]
mod message_id_safety_tests {
    #[test]
    fn rest_message_id_routes_must_not_truncate_large_numeric_ids() {
        let source = include_str!("api_routes.rs");
        let get = source
            .split_once("#[get(\"/api/v1/files/{message_id}\")]\nasync fn api_get_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/files/{message_id}/download\")]")
            .unwrap()
            .0;
        let download = source
            .split_once(
                "#[get(\"/api/v1/files/{message_id}/download\")]\nasync fn api_download_file(",
            )
            .unwrap()
            .1
            .split_once("fn checked_telegram_message_id(")
            .unwrap()
            .0;
        let bulk = source
            .split_once("#[post(\"/api/v1/files/bulk\")]\nasync fn api_bulk_files(")
            .unwrap()
            .1
            .split_once("match body.action.as_str()")
            .unwrap()
            .0;
        assert!(
            !get.contains("path.into_inner() as i32"),
            "GET must reject overflowing Telegram IDs"
        );
        assert!(
            !download.contains("path.into_inner() as i32"),
            "download must reject overflowing Telegram IDs"
        );
        assert!(
            !bulk.contains("Some(i as i32)"),
            "bulk delete/move must not mutate a wrapped message ID"
        );
    }

    #[test]
    fn rest_bulk_folder_ids_must_not_downgrade_malformed_values_to_home() {
        let source = include_str!("api_routes.rs");
        let bulk = source
            .rsplit_once("async fn api_bulk_files(")
            .unwrap()
            .1
            .split_once("match body.action.as_str()")
            .unwrap()
            .0;
        assert!(
            bulk.contains("parse_optional_bulk_folder_id("),
            "invalid source/target folder IDs must return an error before delete or move"
        );
        assert!(
            !bulk.contains("body.folder_id.as_ref().and_then("),
            "malformed source folders must not silently become the default folder"
        );
    }

    #[test]
    fn bulk_folder_parser_preserves_absent_and_valid_ids_but_rejects_bad_values() {
        use super::parse_optional_bulk_folder_id;
        use serde_json::json;
        assert_eq!(parse_optional_bulk_folder_id(None).unwrap(), None);
        for id in [0_i64, -123, i64::MAX, i64::MIN] {
            assert_eq!(
                parse_optional_bulk_folder_id(Some(&json!(id))).unwrap(),
                Some(id)
            );
            assert_eq!(
                parse_optional_bulk_folder_id(Some(&json!(id.to_string()))).unwrap(),
                Some(id)
            );
        }
        for invalid in [
            json!("not-a-folder"),
            json!(""),
            json!(true),
            json!([]),
            json!(1.25),
            json!("9223372036854775808"),
            json!(u64::MAX),
        ] {
            assert!(
                parse_optional_bulk_folder_id(Some(&invalid)).is_err(),
                "{invalid}: malformed folder IDs must not default to home"
            );
        }
    }

    #[test]
    fn rest_message_id_conversion_rejects_overflow_and_preserves_valid_ids() {
        use super::{checked_telegram_message_id, parse_bulk_telegram_message_ids};
        use serde_json::json;

        assert_eq!(checked_telegram_message_id(1).unwrap(), 1);
        assert_eq!(
            checked_telegram_message_id(i32::MAX as i64).unwrap(),
            i32::MAX
        );
        for invalid in [
            0,
            -1,
            i32::MIN as i64,
            i32::MAX as i64 + 1,
            i64::MAX,
            i64::MIN,
        ] {
            assert!(
                checked_telegram_message_id(invalid).is_err(),
                "{invalid}: invalid or overflowing Telegram IDs must never wrap"
            );
        }
        assert_eq!(
            parse_bulk_telegram_message_ids(&[json!(1), json!("2147483647")]).unwrap(),
            vec![1, i32::MAX],
        );
        for invalid in [
            json!(0),
            json!(-1),
            json!(2147483648_i64),
            json!(4294967297_u64),
            json!(i64::MAX),
            json!("2147483648"),
            json!("not-a-message"),
            json!(null),
            json!(true),
            json!([1]),
        ] {
            assert!(
                parse_bulk_telegram_message_ids(&[json!(17), invalid.clone()]).is_err(),
                "invalid bulk ID {invalid} must fail the entire request"
            );
        }
    }
}

struct CleanupStream {
    file: tokio::fs::File,
    path: std::path::PathBuf,
}

impl futures::Stream for CleanupStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut buf = [0u8; 16384];
        let mut read_buf = tokio::io::ReadBuf::new(&mut buf);
        // SAFETY: we project the pin to the file field — no other field is moved.
        let file_pin = unsafe { self.map_unchecked_mut(|s| &mut s.file) };
        match file_pin.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Bytes::copy_from_slice(filled))))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for CleanupStream {
    fn drop(&mut self) {
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(path).await;
        });
    }
}

fn peer_to_input_peer(peer: &Peer) -> Result<tl::enums::InputPeer, String> {
    match peer {
        Peer::User(u) => {
            let (id, access_hash) = match &u.raw {
                tl::enums::User::User(usr) => (usr.id, usr.access_hash.unwrap_or(0)),
                tl::enums::User::Empty(usr) => (usr.id, 0),
            };
            Ok(tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: id,
                access_hash,
            }))
        }
        Peer::Channel(c) => Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: c.raw.id,
            access_hash: c.raw.access_hash.ok_or("No access hash for channel")?,
        })),
        _ => Err("Unsupported peer type".to_string()),
    }
}

/// Spawn a blocking task to delete stale thumbnail and preview cache entries
/// for the given message IDs in the given source folder.
/// Best-effort: failures are silently ignored since cache cleanup is non-critical.
fn spawn_cache_cleanup(
    thumb_dir: std::path::PathBuf,
    prev_dir: std::path::PathBuf,
    ids: Vec<i32>,
    folder_key: String,
) {
    tokio::task::spawn_blocking(move || {
        for mid in &ids {
            for ext in THUMBNAIL_EXTS {
                let path = thumb_dir.join(format!("{}_{}.{}", folder_key, mid, ext));
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
            }
            let prefix = format!("{}_{}.", folder_key, mid);
            if let Ok(entries) = std::fs::read_dir(&prev_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                        if fname.starts_with(&prefix) {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    });
}

/// Validate X-API-Key header against stored hash
fn check_auth(req: &HttpRequest, api_state: &web::Data<ApiState>) -> Result<(), Box<HttpResponse>> {
    let key_hash = match &api_state.key_hash {
        Some(h) => h,
        None => {
            return Err(Box::new(json_error(
                "NO_KEY_CONFIGURED",
                "No API key has been configured. Generate one in Settings.",
                401,
            )))
        }
    };

    let provided = req.headers().get("X-API-Key").and_then(|v| v.to_str().ok());

    match provided {
        Some(key) if crate::commands::api_settings::verify_key(key, key_hash) => Ok(()),
        Some(_) => Err(Box::new(json_error("UNAUTHORIZED", "Invalid API key", 401))),
        None => Err(Box::new(json_error(
            "UNAUTHORIZED",
            "Missing X-API-Key header",
            401,
        ))),
    }
}

// ──────────────────────────────── Endpoints ────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
}

#[get("/api/v1/health")]
async fn api_health() -> impl Responder {
    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// Page and limit arrive as u32 request parameters, but their product can
// exceed u32::MAX. Compute the offset in u64 and make oversized offsets
// behave as an empty page instead of wrapping or panicking.
fn files_page_offset(page: u32, limit: u32) -> usize {
    let offset = u64::from(page.saturating_sub(1)) * u64::from(limit);
    usize::try_from(offset).unwrap_or(usize::MAX)
}

// Telegram supplies signed document lengths. Reject malformed negative
// values before they become huge unsigned REST file sizes or storage totals.
fn validated_telegram_document_size<T: TryInto<u64>>(size: T) -> Result<u64, String> {
    size.try_into()
        .map_err(|_| "Telegram document has invalid negative size metadata".to_string())
}

// Explicit empty/null folder IDs historically select Home. Everything else
// must parse without loss; a malformed folder request cannot silently list
// Home and appear to have found the caller's selected folder.
fn parse_requested_file_listing_folder_id(raw: Option<&str>) -> Result<Option<i64>, String> {
    match raw {
        None | Some("" | "null" | "none" | "None") => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| "Telegram folder ID must be a valid 64-bit integer".to_string()),
    }
}

#[derive(serde::Deserialize, Clone)]
struct FilesQuery {
    #[allow(dead_code)]
    folder_id: Option<String>,
    page: Option<u32>,
    limit: Option<u32>,
    search: Option<String>,
    offset_id: Option<i32>,
    sort: Option<String>,
    order: Option<String>,
    mime_type: Option<String>,
    created_after: Option<String>,
    created_before: Option<String>,
    size_min: Option<u64>,
    size_max: Option<u64>,
    fields: Option<String>,
}

#[derive(Serialize)]
struct FilesResponse {
    data: Vec<serde_json::Value>,
    files: Vec<serde_json::Value>, // For backwards compatibility
    page: u32,
    limit: u32,
    total: usize,
    pagination: PaginationInfo,
}

#[derive(Serialize)]
struct PaginationInfo {
    page: u32,
    limit: u32,
    total: usize,
    total_pages: u32,
    has_next: bool,
    has_prev: bool,
}

#[derive(Serialize, Clone)]
struct ApiFile {
    id: i64,
    folder_id: Option<i64>,
    name: String,
    size: u64,
    mime_type: Option<String>,
    created_at: String,
}

#[get("/api/v1/files")]
async fn api_list_files(
    req: HttpRequest,
    query: web::Query<FilesQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let query_string = req.query_string();
    let has_folder_id = query_string
        .split('&')
        .any(|p| p.starts_with("folder_id=") || p == "folder_id");

    let mut peers_to_scan = Vec::new();
    if !has_folder_id {
        // Return files from ALL folders: scan dialogs + root folder.
        // Missing peers or a failed enumeration must not look like an
        // authoritative complete list of all the user's files.
        let me_peer = match resolve_peer(&client, None, &tg_state.peer_cache).await {
            Ok(peer) => peer,
            Err(error) => return json_error("LIST_FILES_FAILED", &error, 502),
        };
        peers_to_scan.push((None, me_peer));
        let mut dialogs = client.iter_dialogs();
        loop {
            let dialog = match dialogs.next().await {
                Ok(Some(dialog)) => dialog,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "LIST_FILES_FAILED",
                        &format!("Could not enumerate Telegram folders: {error}"),
                        502,
                    );
                }
            };
            if let Peer::Channel(ref c) = dialog.peer {
                let name = c.raw.title.clone();
                let is_tera_folder = {
                    let lower = name.to_lowercase();
                    lower.contains("[tr]") || lower.contains("[td]")
                };
                if is_tera_folder {
                    peers_to_scan.push((Some(c.raw.id), dialog.peer.clone()));
                }
            }
        }
    } else {
        // Actix has already decoded the query parameter. Never turn a
        // malformed supplied folder ID into a successful listing of Home.
        let parsed_id = match parse_requested_file_listing_folder_id(query.folder_id.as_deref()) {
            Ok(id) => id,
            Err(error) => return json_error("INVALID_FOLDER_ID", &error, 400),
        };

        let resolved = match resolve_peer(&client, parsed_id, &tg_state.peer_cache).await {
            Ok(p) => p,
            Err(e) => return json_error("PEER_ERROR", &e, 400),
        };
        peers_to_scan.push((parsed_id, resolved));
    }

    let mut all_files: Vec<ApiFile> = Vec::new();
    for (fid, peer) in &peers_to_scan {
        let mut msgs = client.iter_messages(peer);
        if let Some(offset_id) = query.offset_id {
            msgs = msgs.offset_id(offset_id);
        }

        // When listing all, limit scan per folder to prevent rate limit timeouts
        if !has_folder_id {
            msgs = msgs.limit(100);
        } else if query.search.is_none() {
            let page = query.page.unwrap_or(1).clamp(1, u32::MAX);
            let limit = query.limit.unwrap_or(20).clamp(1, 100);
            if query.offset_id.is_some() {
                msgs = msgs.limit(limit as usize * 2);
            } else {
                msgs = msgs.limit(page as usize * limit as usize * 2);
            }
        } else {
            msgs = msgs.limit(2000);
        }

        loop {
            let msg = match msgs.next().await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "LIST_FILES_FAILED",
                        &format!("Could not enumerate Telegram files: {error}"),
                        502,
                    );
                }
            };
            if let Some(doc) = msg.media() {
                let (name, size, mime) = match doc {
                    Media::Document(d) => {
                        let doc_name = d.name().to_string();
                        // Prefer the message caption (set by rename via EditMessage)
                        let caption = msg.text();
                        let display_name = if caption.is_empty() {
                            doc_name
                        } else {
                            caption.to_string()
                        };
                        let size = match validated_telegram_document_size(d.size()) {
                            Ok(size) => size,
                            Err(error) => return json_error("LIST_FILES_FAILED", &error, 502),
                        };
                        (display_name, size, d.mime_type().map(|s| s.to_string()))
                    }
                    Media::Photo(_) => ("Photo.jpg".to_string(), 0, Some("image/jpeg".into())),
                    _ => ("Unknown".to_string(), 0, None),
                };

                all_files.push(ApiFile {
                    id: msg.id() as i64,
                    folder_id: *fid,
                    name,
                    size,
                    mime_type: mime,
                    created_at: msg.date().to_string(),
                });
            }
        }
    }

    // Apply filters
    let mut filtered_files: Vec<ApiFile> = Vec::new();
    for file in all_files {
        if let Some(ref search) = query.search {
            if !file.name.to_lowercase().contains(&search.to_lowercase()) {
                continue;
            }
        }
        if let Some(ref mt) = query.mime_type {
            if let Some(ref fmt) = file.mime_type {
                if !fmt.to_lowercase().contains(&mt.to_lowercase()) {
                    continue;
                }
            } else {
                continue;
            }
        }
        if let Some(min) = query.size_min {
            if file.size < min {
                continue;
            }
        }
        if let Some(max) = query.size_max {
            if file.size > max {
                continue;
            }
        }
        if let Some(ref after) = query.created_after {
            if file.created_at < *after {
                continue;
            }
        }
        if let Some(ref before) = query.created_before {
            if file.created_at > *before {
                continue;
            }
        }
        filtered_files.push(file);
    }

    // Sort
    let sort_field = query.sort.as_deref().unwrap_or("created_at");
    let sort_order = query.order.as_deref().unwrap_or("asc");
    filtered_files.sort_by(|a, b| {
        let cmp = match sort_field {
            "name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            "size" => a.size.cmp(&b.size),
            _ => a.created_at.cmp(&b.created_at),
        };
        if sort_order.to_lowercase() == "desc" {
            cmp.reverse()
        } else {
            cmp
        }
    });

    // Pagination
    let page = query.page.unwrap_or(1).max(1);
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let total = filtered_files.len();
    let total_pages = (total.div_ceil(limit as usize)) as u32;
    let start = files_page_offset(page, limit);

    let paginated_files: Vec<ApiFile> = filtered_files
        .into_iter()
        .skip(start)
        .take(limit as usize)
        .collect();

    let has_next = page < total_pages;
    let has_prev = page > 1;

    // Sparse fieldsets
    let mut final_data = Vec::new();
    let fields_list: Option<Vec<String>> = query
        .fields
        .as_ref()
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

    for file in paginated_files {
        let mut map = serde_json::Map::new();
        let include_all = fields_list.is_none();
        let fields = fields_list.as_ref();

        if include_all || fields.unwrap().contains(&"id".to_string()) {
            map.insert("id".to_string(), serde_json::json!(file.id));
        }
        if include_all || fields.unwrap().contains(&"folder_id".to_string()) {
            map.insert("folder_id".to_string(), serde_json::json!(file.folder_id));
        }
        if include_all || fields.unwrap().contains(&"name".to_string()) {
            map.insert("name".to_string(), serde_json::json!(file.name));
        }
        if include_all || fields.unwrap().contains(&"size".to_string()) {
            map.insert("size".to_string(), serde_json::json!(file.size));
        }
        if include_all || fields.unwrap().contains(&"mime_type".to_string()) {
            map.insert("mime_type".to_string(), serde_json::json!(file.mime_type));
        }
        if include_all || fields.unwrap().contains(&"created_at".to_string()) {
            map.insert("created_at".to_string(), serde_json::json!(file.created_at));
        }

        final_data.push(serde_json::Value::Object(map));
    }

    let res_body = FilesResponse {
        data: final_data.clone(),
        files: final_data,
        page,
        limit,
        total,
        pagination: PaginationInfo {
            page,
            limit,
            total,
            total_pages,
            has_next,
            has_prev,
        },
    };

    HttpResponse::Ok().json(res_body)
}

#[derive(serde::Deserialize)]
struct FolderQuery {
    folder_id: Option<i64>,
}

#[get("/api/v1/files/{message_id}")]
async fn api_get_file(
    req: HttpRequest,
    path: web::Path<i64>,
    query: web::Query<FolderQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let message_id = match checked_telegram_message_id(path.into_inner()) {
        Ok(id) => id,
        Err(message) => return json_error("INVALID_MESSAGE_ID", &message, 400),
    };
    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let peer = match resolve_peer(&client, query.folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("PEER_ERROR", &e, 400),
    };

    match client.get_messages_by_id(peer, &[message_id]).await {
        Ok(messages) => {
            if let Some(Some(msg)) = messages.first() {
                if let Some(doc) = msg.media() {
                    let (name, size, mime) = match doc {
                        Media::Document(d) => {
                            let doc_name = d.name().to_string();
                            let caption = msg.text();
                            let display_name = if caption.is_empty() {
                                doc_name
                            } else {
                                caption.to_string()
                            };
                            let size = match validated_telegram_document_size(d.size()) {
                                Ok(size) => size,
                                Err(error) => return json_error("FETCH_ERROR", &error, 502),
                            };
                            (display_name, size, d.mime_type().map(|s| s.to_string()))
                        }
                        Media::Photo(_) => ("Photo.jpg".to_string(), 0, Some("image/jpeg".into())),
                        _ => ("Unknown".to_string(), 0, None),
                    };
                    return HttpResponse::Ok().json(ApiFile {
                        id: msg.id() as i64,
                        folder_id: query.folder_id,
                        name,
                        size,
                        mime_type: mime,
                        created_at: msg.date().to_string(),
                    });
                }
            }
            json_error("NOT_FOUND", "File not found", 404)
        }
        Err(e) => json_error("FETCH_ERROR", &format!("Failed to fetch file: {}", e), 500),
    }
}

#[get("/api/v1/files/{message_id}/download")]
async fn api_download_file(
    req: HttpRequest,
    path: web::Path<i64>,
    query: web::Query<FolderQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let message_id = match checked_telegram_message_id(path.into_inner()) {
        Ok(id) => id,
        Err(message) => return json_error("INVALID_MESSAGE_ID", &message, 400),
    };
    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let peer = match resolve_peer(&client, query.folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("PEER_ERROR", &e, 400),
    };

    match client.get_messages_by_id(peer, &[message_id]).await {
        Ok(messages) => {
            if let Some(Some(msg)) = messages.first() {
                if let Some(media) = msg.media() {
                    let mime = match &media {
                        Media::Document(d) => d
                            .mime_type()
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                        Media::Photo(_) => "image/jpeg".to_string(),
                        _ => "application/octet-stream".to_string(),
                    };
                    let filename = match &media {
                        Media::Document(d) => d.name().to_string(),
                        Media::Photo(_) => "Photo.jpg".to_string(),
                        _ => "download".to_string(),
                    };

                    return crate::server::build_media_response(
                        &client,
                        &media,
                        &req,
                        &mime,
                        Some(&filename),
                        crate::server::StreamingExtras {
                            extra_headers: vec![],
                            log_label: "API download",
                        },
                    );
                }
            }
            json_error("NOT_FOUND", "File not found", 404)
        }
        Err(e) => json_error("FETCH_ERROR", &format!("Failed to fetch file: {}", e), 500),
    }
}

fn checked_telegram_message_id(raw: i64) -> Result<i32, String> {
    i32::try_from(raw)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| "Telegram message ID must be a positive 32-bit integer".to_string())
}

fn parse_bulk_telegram_message_ids(values: &[serde_json::Value]) -> Result<Vec<i32>, String> {
    values
        .iter()
        .map(|value| {
            let raw = match value {
                serde_json::Value::Number(number) => number.as_i64(),
                serde_json::Value::String(value) => value.parse::<i64>().ok(),
                _ => None,
            }
            .ok_or_else(|| "Telegram message ID must be a valid integer".to_string())?;
            checked_telegram_message_id(raw)
        })
        .collect()
}

fn parse_optional_bulk_folder_id(value: Option<&serde_json::Value>) -> Result<Option<i64>, String> {
    value
        .map(|value| {
            match value {
                serde_json::Value::Number(number) => number.as_i64(),
                serde_json::Value::String(value) => value.parse::<i64>().ok(),
                _ => None,
            }
            .ok_or_else(|| "Telegram folder ID must be a valid 64-bit integer".to_string())
        })
        .transpose()
}

#[derive(serde::Deserialize)]
struct BulkRequest {
    action: String,
    file_ids: Vec<serde_json::Value>,
    folder_id: Option<serde_json::Value>,
    payload: Option<BulkPayload>,
}

#[derive(serde::Deserialize)]
struct BulkPayload {
    folder_id: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct BulkResponse {
    success: bool,
    count: usize,
}

#[post("/api/v1/files/bulk")]
async fn api_bulk_files(
    req: HttpRequest,
    body: web::Json<BulkRequest>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
    net_config: web::Data<Arc<NetworkConfig>>,
    cache_dirs: web::Data<CacheDirs>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    // Reject the entire request if any supplied message ID would truncate
    // or is invalid. Silently dropping/rewrapping an ID can delete or move
    // a different Telegram message from the one the client selected.
    let ids = match parse_bulk_telegram_message_ids(&body.file_ids) {
        Ok(ids) => ids,
        Err(message) => return json_error("INVALID_MESSAGE_ID", &message, 400),
    };

    let source_folder = match parse_optional_bulk_folder_id(body.folder_id.as_ref()) {
        Ok(folder) => folder,
        Err(message) => return json_error("INVALID_FOLDER_ID", &message, 400),
    };
    let target_folder = match parse_optional_bulk_folder_id(
        body.payload
            .as_ref()
            .and_then(|payload| payload.folder_id.as_ref()),
    ) {
        Ok(folder) => folder,
        Err(message) => return json_error("INVALID_FOLDER_ID", &message, 400),
    };

    match body.action.as_str() {
        "delete" => {
            let peer = match resolve_peer(&client, source_folder, &tg_state.peer_cache).await {
                Ok(p) => p,
                Err(e) => return json_error("PEER_ERROR", &e, 400),
            };
            if let Err(e) = client.delete_messages(&peer, &ids).await {
                return json_error("DELETE_FAILED", &e.to_string(), 500);
            }

            // Clean up stale thumbnail and preview caches for deleted messages.
            let source_folder_key = source_folder
                .map(|id| id.to_string())
                .unwrap_or_else(|| "home".to_string());
            spawn_cache_cleanup(
                cache_dirs.thumbnail_dir.clone(),
                cache_dirs.preview_dir.clone(),
                ids.clone(),
                source_folder_key,
            );
        }
        "move" => {
            let source_peer = match resolve_peer(&client, source_folder, &tg_state.peer_cache).await
            {
                Ok(p) => p,
                Err(e) => return json_error("PEER_ERROR", &e, 400),
            };
            let target_peer = match resolve_peer(&client, target_folder, &tg_state.peer_cache).await
            {
                Ok(p) => p,
                Err(e) => return json_error("PEER_ERROR", &e, 400),
            };
            if source_folder != target_folder {
                if let Err(e) = client
                    .forward_messages(&target_peer, &ids, &source_peer)
                    .await
                {
                    return json_error(
                        "MOVE_FORWARD_FAILED",
                        &format!("Forward failed: {}", e),
                        500,
                    );
                }
                if let Err(e) = client.delete_messages(&source_peer, &ids).await {
                    return json_error(
                        "MOVE_DELETE_FAILED",
                        &format!("Delete original failed: {}", e),
                        500,
                    );
                }

                // Clean up stale thumbnail and preview caches for the old message IDs.
                // After a move (forward+delete), messages get new IDs in the target folder,
                // so any cached thumbnails/previews under the old IDs are orphaned.
                let source_folder_key = source_folder
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "home".to_string());
                spawn_cache_cleanup(
                    cache_dirs.thumbnail_dir.clone(),
                    cache_dirs.preview_dir.clone(),
                    ids.clone(),
                    source_folder_key,
                );
            }
        }
        "archive" => {
            let peer = match resolve_peer(&client, source_folder, &tg_state.peer_cache).await {
                Ok(p) => p,
                Err(e) => return json_error("PEER_ERROR", &e, 400),
            };

            // Download all files in async context, then delegate zip I/O
            // to spawn_blocking so we never block an Actix worker thread.
            let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
            let mut used_archive_names = HashSet::new();
            let mut total_bytes: u64 = 0;
            let max_bytes = net_config.archive_max_bytes();

            for mid in &ids {
                let messages = match client.get_messages_by_id(&peer, &[*mid]).await {
                    Ok(m) => m,
                    Err(error) => {
                        return json_error(
                            "ARCHIVE_LOOKUP_FAILED",
                            &format!("Could not fetch selected archive message {mid}: {error}"),
                            502,
                        );
                    }
                };
                let Some(m) = messages.into_iter().flatten().next() else {
                    return json_error(
                        "ARCHIVE_MESSAGE_MISSING",
                        &format!("Selected archive message {mid} no longer exists"),
                        404,
                    );
                };
                let Some(media) = m.media() else {
                    return json_error(
                        "ARCHIVE_MESSAGE_MISSING",
                        &format!("Selected archive message {mid} has no downloadable media"),
                        404,
                    );
                };
                let remote_filename = match &media {
                    Media::Document(d) => d.name().to_string(),
                    Media::Photo(_) => format!("photo_{}.jpg", mid),
                    _ => format!("file_{}.bin", mid),
                };
                let filename = unique_archive_entry_name(
                    safe_archive_entry_name(&remote_filename, *mid),
                    *mid,
                    &mut used_archive_names,
                );
                let expected_document_bytes = match &media {
                    Media::Document(d) => match u64::try_from(d.size()) {
                        Ok(size) => Some(size),
                        Err(_) => {
                            return json_error(
                                "ARCHIVE_INCOMPLETE_DOWNLOAD",
                                &format!(
                                    "Selected archive document {mid} has invalid size metadata"
                                ),
                                502,
                            );
                        }
                    },
                    _ => None,
                };

                let mut data = Vec::new();
                let mut download_iter = client.iter_download(&media);
                loop {
                    let chunk = match download_iter.next().await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(error) => {
                            return json_error(
                                "ARCHIVE_DOWNLOAD_FAILED",
                                &format!("Could not download archived media: {error}"),
                                502,
                            );
                        }
                    };
                    total_bytes = match total_bytes.checked_add(chunk.len() as u64) {
                        Some(total) => total,
                        None => {
                            return json_error(
                                "ARCHIVE_TOO_LARGE",
                                "Archive byte count overflow",
                                413,
                            );
                        }
                    };
                    if max_bytes > 0 && total_bytes > max_bytes {
                        return json_error(
                            "ARCHIVE_TOO_LARGE",
                            &format!(
                                "Archive exceeds the {} MiB limit",
                                max_bytes / (1024 * 1024)
                            ),
                            413,
                        );
                    }
                    data.extend_from_slice(&chunk);
                }
                if !archive_document_length_matches(expected_document_bytes, data.len()) {
                    return json_error(
                        "ARCHIVE_INCOMPLETE_DOWNLOAD",
                        &format!(
                            "Selected archive document {mid} ended before its declared length"
                        ),
                        502,
                    );
                }
                entries.push((filename, data));
            }

            let temp_zip_path = std::env::temp_dir().join(format!(
                "archive_{}_{}.zip",
                rand::random::<u32>(),
                rand::random::<u32>()
            ));
            let zip_path_for_task = temp_zip_path.clone();

            // All zip I/O runs on a blocking thread — never touches Actix workers.
            let archive_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let write_zip = || -> Result<(), String> {
                    let zip_file = std::fs::File::create(&zip_path_for_task)
                        .map_err(|e| format!("ZIP_CREATE_FAILED: {}", e))?;
                    let mut zip = zip::ZipWriter::new(zip_file);
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated);

                    for (filename, data) in &entries {
                        zip.start_file(filename, options)
                            .map_err(|e| format!("ZIP_ADD_FAILED: {}", e))?;
                        zip.write_all(data)
                            .map_err(|e| format!("ZIP_WRITE_FAILED: {}", e))?;
                    }

                    zip.finish()
                        .map_err(|e| format!("ZIP_FINISH_FAILED: {}", e))?;
                    Ok(())
                };

                match write_zip() {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&zip_path_for_task);
                        Err(e)
                    }
                }
            })
            .await;

            match archive_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return json_error("ARCHIVE_FAILED", &e, 500),
                Err(e) => return json_error("ARCHIVE_PANIC", &e.to_string(), 500),
            }

            let file = match tokio::fs::File::open(&temp_zip_path).await {
                Ok(f) => f,
                Err(e) => return json_error("OPEN_ZIP_FAILED", &e.to_string(), 500),
            };

            let stream = CleanupStream {
                file,
                path: temp_zip_path,
            };

            return HttpResponse::Ok()
                .content_type("application/zip")
                .insert_header((
                    actix_web::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"archive.zip\"",
                ))
                .streaming(stream);
        }
        _ => return json_error("INVALID_ACTION", "Unsupported bulk action", 400),
    }

    HttpResponse::Ok().json(BulkResponse {
        success: true,
        count: ids.len(),
    })
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
    #[allow(dead_code)]
    folder_id: Option<String>,
    #[allow(dead_code)]
    recursive: Option<bool>,
}

#[cfg(test)]
mod rest_enumeration_integrity_tests {
    #[test]
    fn telegram_document_size_validation_rejects_negative_lengths() {
        use super::validated_telegram_document_size;
        assert_eq!(validated_telegram_document_size(0_i64).unwrap(), 0);
        assert_eq!(validated_telegram_document_size(1024_i64).unwrap(), 1024);
        assert_eq!(
            validated_telegram_document_size(i64::MAX).unwrap(),
            i64::MAX as u64
        );
        assert!(validated_telegram_document_size(-1_i64).is_err());
        assert!(validated_telegram_document_size(i64::MIN).is_err());
    }

    #[test]
    fn rest_file_and_storage_routes_must_not_cast_negative_media_sizes_to_u64() {
        let source = include_str!("api_routes.rs");
        for (handler, end) in [
            ("api_list_files", "async fn api_get_file("),
            ("api_search_files", "async fn api_delete_file("),
            ("api_storage_stats", "async fn api_storage_duplicates("),
            ("api_storage_duplicates", "fn confirmed_empty_folder<"),
        ] {
            let marker = format!("async fn {handler}(");
            let route = source
                .rsplit_once(&marker)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0;
            assert!(
                !route.contains("d.size() as u64") && !route.contains("size: size as u64"),
                "{handler}: a negative Telegram document length cannot become a huge file or total"
            );
            assert!(
                route.contains("validated_telegram_document_size("),
                "{handler}: reject invalid signed Telegram document sizes before publishing data"
            );
        }
        let detail = source
            .split_once("#[get(\"/api/v1/files/{message_id}\")]\nasync fn api_get_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/files/{message_id}/download\")]")
            .unwrap()
            .0;
        assert!(
            !detail.contains("size: size as u64")
                && detail.contains("validated_telegram_document_size("),
            "GET file detail must fail closed on negative Telegram document size"
        );
    }

    #[test]
    fn search_folders_stats_and_duplicates_must_not_swallow_telegram_errors() {
        let source = include_str!("api_routes.rs");
        for (handler, end, error) in [
            ("api_search_files", "api_delete_file", "SEARCH_FILES_FAILED"),
            (
                "api_list_folders",
                "api_create_folder",
                "LIST_FOLDERS_FAILED",
            ),
            (
                "api_storage_stats",
                "api_storage_duplicates",
                "STORAGE_STATS_FAILED",
            ),
            (
                "api_storage_duplicates",
                "fn confirmed_empty_folder<",
                "DUPLICATES_FAILED",
            ),
        ] {
            let marker = format!("async fn {handler}(");
            let next = if end.starts_with("fn ") {
                end.to_string()
            } else {
                format!("async fn {end}(")
            };
            let route = source
                .rsplit_once(&marker)
                .unwrap()
                .1
                .split_once(&next)
                .unwrap()
                .0;
            assert!(
                !route.contains(".next().await.ok().flatten()"),
                "{handler}: Telegram iteration failures must not masquerade as complete results"
            );
            assert!(
                route.contains(error),
                "{handler}: Telegram failures must return an explicit failure response"
            );
        }
        let search = source
            .rsplit_once("async fn api_search_files(")
            .unwrap()
            .1
            .split_once("async fn api_delete_file(")
            .unwrap()
            .0;
        assert!(
            search.contains("parse_requested_file_listing_folder_id("),
            "search folder selection must reject malformed IDs rather than silently selecting Home"
        );
    }
}

#[get("/api/v1/files/search")]
async fn api_search_files(
    req: HttpRequest,
    query: web::Query<SearchQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let search_q = match query.q.as_deref() {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            return json_error(
                "INVALID_QUERY",
                "Search query parameter 'q' is required and cannot be empty",
                400,
            )
        }
    };

    let query_string = req.query_string();
    let has_folder_id = query_string
        .split('&')
        .any(|p| p.starts_with("folder_id=") || p == "folder_id");

    let mut peers_to_scan = Vec::new();
    if !has_folder_id {
        let me_peer = match resolve_peer(&client, None, &tg_state.peer_cache).await {
            Ok(peer) => peer,
            Err(error) => return json_error("SEARCH_FILES_FAILED", &error, 502),
        };
        peers_to_scan.push((None, me_peer));
        let mut dialogs = client.iter_dialogs();
        loop {
            let dialog = match dialogs.next().await {
                Ok(Some(dialog)) => dialog,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "SEARCH_FILES_FAILED",
                        &format!("Could not enumerate Telegram folders: {error}"),
                        502,
                    );
                }
            };
            if let Peer::Channel(ref c) = dialog.peer {
                let name = c.raw.title.clone();
                let is_tera_folder = {
                    let lower = name.to_lowercase();
                    lower.contains("[tr]") || lower.contains("[td]")
                };
                if is_tera_folder {
                    peers_to_scan.push((Some(c.raw.id), dialog.peer.clone()));
                }
            }
        }
    } else {
        let parsed_id = match parse_requested_file_listing_folder_id(query.folder_id.as_deref()) {
            Ok(id) => id,
            Err(error) => return json_error("INVALID_FOLDER_ID", &error, 400),
        };
        let resolved = match resolve_peer(&client, parsed_id, &tg_state.peer_cache).await {
            Ok(p) => p,
            Err(e) => return json_error("PEER_ERROR", &e, 400),
        };
        peers_to_scan.push((parsed_id, resolved));
    }

    let mut matching_files = Vec::new();
    for (fid, peer) in &peers_to_scan {
        let mut msgs = client.iter_messages(peer).limit(200);
        loop {
            let msg = match msgs.next().await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "SEARCH_FILES_FAILED",
                        &format!("Could not enumerate Telegram files: {error}"),
                        502,
                    );
                }
            };
            if let Some(doc) = msg.media() {
                let (name, size, mime) = match doc {
                    Media::Document(d) => {
                        let doc_name = d.name().to_string();
                        let caption = msg.text();
                        let display_name = if caption.is_empty() {
                            doc_name
                        } else {
                            caption.to_string()
                        };
                        let size = match validated_telegram_document_size(d.size()) {
                            Ok(size) => size,
                            Err(error) => return json_error("SEARCH_FILES_FAILED", &error, 502),
                        };
                        (display_name, size, d.mime_type().map(|s| s.to_string()))
                    }
                    Media::Photo(_) => ("Photo.jpg".to_string(), 0, Some("image/jpeg".into())),
                    _ => ("Unknown".to_string(), 0, None),
                };

                if name.to_lowercase().contains(&search_q.to_lowercase()) {
                    matching_files.push(ApiFile {
                        id: msg.id() as i64,
                        folder_id: *fid,
                        name,
                        size,
                        mime_type: mime,
                        created_at: msg.date().to_string(),
                    });
                }
            }
        }
    }

    HttpResponse::Ok().json(matching_files)
}

#[delete("/api/v1/files/{message_id}")]
async fn api_delete_file(
    req: HttpRequest,
    path: web::Path<i32>,
    query: web::Query<FolderQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let message_id = path.into_inner();
    let folder_id = query.folder_id;

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let peer = match resolve_peer(&client, folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("PEER_ERROR", &e, 400),
    };

    match client.delete_messages(&peer, &[message_id]).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({ "success": true })),
        Err(e) => json_error("DELETE_FAILED", &e.to_string(), 500),
    }
}

#[derive(serde::Deserialize)]
struct CopyRequest {
    folder_id: Option<i64>,
    source_folder_id: Option<i64>,
}

#[post("/api/v1/files/{message_id}/copy")]
async fn api_copy_file(
    req: HttpRequest,
    path: web::Path<i32>,
    body: web::Json<CopyRequest>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let message_id = path.into_inner();
    let source_folder_id = body.source_folder_id;
    let target_folder_id = body.folder_id;

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let source_peer = match resolve_peer(&client, source_folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("SOURCE_PEER_ERROR", &e, 400),
    };
    let target_peer = match resolve_peer(&client, target_folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("TARGET_PEER_ERROR", &e, 400),
    };

    match client
        .forward_messages(&target_peer, &[message_id], &source_peer)
        .await
    {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({ "success": true })),
        Err(e) => json_error("COPY_FAILED", &e.to_string(), 500),
    }
}

#[derive(serde::Deserialize)]
struct UpdateFileRequest {
    name: Option<String>,
    folder_id: Option<i64>,
    source_folder_id: Option<i64>,
}

#[patch("/api/v1/files/{message_id}")]
async fn api_update_file(
    req: HttpRequest,
    path: web::Path<i32>,
    body: web::Json<UpdateFileRequest>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
    cache_dirs: web::Data<CacheDirs>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let message_id = path.into_inner();

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    // Rename first — edits the original message's caption so the
    // updated name is carried over if a move (forward) follows.
    if let Some(ref new_name) = body.name {
        let rename_peer =
            match resolve_peer(&client, body.source_folder_id, &tg_state.peer_cache).await {
                Ok(p) => p,
                Err(e) => return json_error("PEER_ERROR", &e, 400),
            };

        // Verify the message exists before attempting to edit it.
        // This avoids a cryptic MESSAGE_ID_INVALID RPC error when the message
        // was moved or deleted since the file list was loaded.
        let messages = match client.get_messages_by_id(&rename_peer, &[message_id]).await {
            Ok(msgs) => msgs,
            Err(e) => {
                return json_error(
                    "FETCH_ERROR",
                    &format!("Failed to fetch message for rename: {}", e),
                    500,
                )
            }
        };
        if messages.iter().flatten().next().is_none() {
            return json_error(
                "MESSAGE_NOT_FOUND",
                &format!(
                    "Message {} not found in folder {:?}. The file may have been moved or deleted. Please refresh.",
                    message_id, body.source_folder_id
                ),
                404,
            );
        }

        let input_peer = match peer_to_input_peer(&rename_peer) {
            Ok(ip) => ip,
            Err(e) => return json_error("PEER_CONVERT_ERROR", &e, 400),
        };

        if let Err(e) = client
            .invoke(&tl::functions::messages::EditMessage {
                peer: input_peer,
                id: message_id,
                no_webpage: false,
                invert_media: false,
                message: Some(new_name.clone()),
                media: None,
                reply_markup: None,
                entities: None,
                schedule_date: None,
                quick_reply_shortcut_id: None,
                schedule_repeat_period: None,
            })
            .await
        {
            return json_error("RENAME_FAILED", &e.to_string(), 500);
        }
    }

    if let Some(target_folder_id) = body.folder_id {
        let source_folder_id = body.source_folder_id;
        if source_folder_id != body.folder_id {
            let source_peer =
                match resolve_peer(&client, source_folder_id, &tg_state.peer_cache).await {
                    Ok(p) => p,
                    Err(e) => return json_error("SOURCE_PEER_ERROR", &e, 400),
                };
            let target_peer =
                match resolve_peer(&client, Some(target_folder_id), &tg_state.peer_cache).await {
                    Ok(p) => p,
                    Err(e) => return json_error("TARGET_PEER_ERROR", &e, 400),
                };

            if let Err(e) = client
                .forward_messages(&target_peer, &[message_id], &source_peer)
                .await
            {
                return json_error("MOVE_FORWARD_FAILED", &e.to_string(), 500);
            }
            if let Err(e) = client.delete_messages(&source_peer, &[message_id]).await {
                return json_error("MOVE_DELETE_FAILED", &e.to_string(), 500);
            }

            // Clean up stale thumbnail and preview caches for the old message ID
            let source_folder_key = source_folder_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "home".to_string());
            spawn_cache_cleanup(
                cache_dirs.thumbnail_dir.clone(),
                cache_dirs.preview_dir.clone(),
                vec![message_id],
                source_folder_key,
            );
        }
    }

    HttpResponse::Ok().json(serde_json::json!({ "success": true }))
}

// Grammers accepts a platform-sized byte count. On 32-bit targets a large
// u64 disk file length must never silently wrap to a smaller upload size.
fn checked_telegram_upload_length(bytes: u64) -> Result<usize, String> {
    usize::try_from(bytes)
        .map_err(|_| "Upload exceeds this host's supported transfer length".to_string())
}

// REST upload staging uses an unpredictable filename, but collision safety
// must not rely on randomness alone. Exclusive creation also refuses a
// pre-existing symlink instead of following it to truncate another file.
async fn create_exclusive_upload_temp_file(
    path: &std::path::Path,
) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
}

#[cfg(test)]
mod upload_request_integrity_tests {
    #[test]
    fn rest_upload_multipart_failures_and_missing_files_cannot_be_successful() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        assert!(
            !upload.contains("while let Ok(Some(mut field)) = payload.try_next().await"),
            "malformed multipart must return an error, not masquerade as EOF"
        );
        assert!(
            upload.contains("MULTIPART_READ_FAILED"),
            "multipart transport errors must fail the upload"
        );
        assert!(
            upload.contains("MISSING_UPLOAD_FILE") && upload.contains("MULTIPLE_UPLOAD_FILES"),
            "missing or repeated file parts must not create empty or concatenated uploads"
        );
        assert!(
            upload.contains("parse_requested_file_listing_folder_id("),
            "invalid upload destination folder must not default to Home"
        );
    }

    #[test]
    fn rest_upload_tempfile_creation_must_never_replace_existing_paths() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        assert!(
            upload.contains("create_exclusive_upload_temp_file(&temp_path).await")
                && !upload.contains("tokio::fs::File::create(&temp_path)"),
            "upload staging must use exclusive creation and never truncate existing temp paths"
        );
    }

    #[actix_web::test]
    async fn exclusive_upload_creation_preserves_existing_files_and_symlinks() {
        let file_path = std::env::temp_dir().join(format!(
            "terarelay-exclusive-upload-qa-{:032x}",
            rand::random::<u128>()
        ));
        tokio::fs::write(&file_path, b"original owned fixture")
            .await
            .unwrap();
        let occupied = super::create_exclusive_upload_temp_file(&file_path).await;
        assert!(matches!(
            occupied,
            Err(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            tokio::fs::read(&file_path).await.unwrap(),
            b"original owned fixture"
        );

        #[cfg(unix)]
        {
            let link_path = file_path.with_extension("link");
            std::os::unix::fs::symlink(&file_path, &link_path).unwrap();
            let through_link = super::create_exclusive_upload_temp_file(&link_path).await;
            assert!(matches!(
                through_link,
                Err(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists
            ));
            assert_eq!(
                tokio::fs::read(&file_path).await.unwrap(),
                b"original owned fixture"
            );
            tokio::fs::remove_file(&link_path).await.unwrap();
        }
        tokio::fs::remove_file(&file_path).await.unwrap();
    }

    #[test]
    fn rest_upload_stream_length_must_not_truncate_on_32bit_hosts() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        assert!(
            !upload.contains("file_size as usize")
                && upload.contains("checked_telegram_upload_length(file_size)"),
            "a 64-bit upload size must never be silently truncated on 32-bit hosts"
        );
    }

    #[test]
    fn upload_stream_length_preserves_platform_width_without_truncation() {
        use super::checked_telegram_upload_length;
        assert_eq!(checked_telegram_upload_length(0).unwrap(), 0);
        assert_eq!(checked_telegram_upload_length(1024).unwrap(), 1024);
        assert_eq!(
            checked_telegram_upload_length(usize::MAX as u64).unwrap(),
            usize::MAX
        );
        #[cfg(target_pointer_width = "32")]
        assert!(checked_telegram_upload_length(u64::from(u32::MAX) + 1).is_err());
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            checked_telegram_upload_length(u64::MAX).unwrap(),
            usize::MAX
        );
    }

    #[test]
    fn rest_upload_filename_cannot_forward_untrusted_remote_paths() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        assert!(
            upload.contains("filename = safe_archive_entry_name(fname, 0);")
                && !upload.contains("filename = fname.to_string();"),
            "multipart path-like filenames must be normalized to a safe leaf before Telegram upload"
        );
    }

    #[test]
    fn rest_upload_closes_telegram_reader_before_removing_plaintext_tempfile() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        let transfer = upload
            .split_once("let upload_res = client")
            .unwrap()
            .1
            .split_once("let uploaded_file = match upload_res")
            .unwrap()
            .0;
        assert!(
            transfer.contains("drop(open_file);"),
            "close the staged upload reader before any error or successful cleanup, including on Windows"
        );
    }

    #[test]
    fn rest_upload_error_cleanup_must_close_tempfile_before_unlinking() {
        let source = include_str!("api_routes.rs");
        let upload = source
            .split_once("#[post(\"/api/v1/files\")]\nasync fn api_upload_file(")
            .unwrap()
            .1
            .split_once("#[get(\"/api/v1/folders\")]")
            .unwrap()
            .0;
        let file_field = upload
            .split_once("if name == \"file\" {")
            .unwrap()
            .1
            .split_once("} else if name == \"folder_id\" {")
            .unwrap()
            .0;
        assert!(
            !file_field
                .contains("Err(e) => {\n                        let _ = tokio::fs::remove_file"),
            "failed file-part reads must close the temporary upload before deletion"
        );
        assert!(
            !file_field.contains(
                "if let Err(e) = file.write_all(&data).await {\n                    let _ = tokio::fs::remove_file"
            ),
            "failed temp-file writes must close the file handle before deletion"
        );
        assert!(
            upload.contains("if let Err(e) = file.flush().await {\n        drop(file);"),
            "failed temp-file flush must close the file handle before deletion"
        );
    }
}

#[post("/api/v1/files")]
async fn api_upload_file(
    req: HttpRequest,
    mut payload: Multipart,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
    bw_manager: web::Data<Arc<BandwidthManager>>,
    net_config: web::Data<Arc<NetworkConfig>>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let temp_path = std::env::temp_dir().join(format!(
        "upload_{}_{}",
        rand::random::<u32>(),
        rand::random::<u32>()
    ));
    let mut file = match create_exclusive_upload_temp_file(&temp_path).await {
        Ok(f) => f,
        Err(e) => return json_error("TEMP_FILE_CREATE_FAILED", &e.to_string(), 500),
    };

    let mut folder_id: Option<i64> = None;
    let mut filename = "file".to_string();
    let mut field_mime: Option<String> = None;
    let mut saw_file = false;
    let mut saw_folder = false;

    loop {
        let mut field = match payload.try_next().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return json_error("MULTIPART_READ_FAILED", &error.to_string(), 400);
            }
        };
        let content_disposition = field.content_disposition();
        let name = content_disposition
            .and_then(|cd| cd.get_name())
            .unwrap_or("");

        if name == "file" {
            if saw_file {
                drop(file);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return json_error(
                    "MULTIPLE_UPLOAD_FILES",
                    "Each upload must contain exactly one file part",
                    400,
                );
            }
            saw_file = true;
            if let Some(fname) = content_disposition.and_then(|cd| cd.get_filename()) {
                // Browser-supplied filenames can contain paths, controls or
                // platform-reserved names. Reuse the tested archive leaf-name
                // sanitizer before passing this value to Telegram or JSON.
                filename = safe_archive_entry_name(fname, 0);
            }
            field_mime = field.content_type().map(|m| m.to_string());
            while let Some(chunk) = field.next().await {
                let data = match chunk {
                    Ok(d) => d,
                    Err(e) => {
                        drop(file);
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return json_error("READ_ERROR", &e.to_string(), 400);
                    }
                };
                if let Err(e) = file.write_all(&data).await {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return json_error("WRITE_ERROR", &e.to_string(), 500);
                }
            }
        } else if name == "folder_id" {
            if saw_folder {
                drop(file);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return json_error("INVALID_FOLDER_ID", "Duplicate upload folder fields", 400);
            }
            saw_folder = true;
            let mut bytes = Vec::new();
            while let Some(chunk) = field.next().await {
                let data = match chunk {
                    Ok(d) => d,
                    Err(e) => {
                        drop(file);
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return json_error("READ_ERROR", &e.to_string(), 400);
                    }
                };
                if bytes.len().saturating_add(data.len()) > 128 {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return json_error("INVALID_FOLDER_ID", "Upload folder ID is too long", 400);
                }
                bytes.extend_from_slice(&data);
            }
            let val_str = match std::str::from_utf8(&bytes) {
                Ok(value) => value.trim(),
                Err(_) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return json_error("INVALID_FOLDER_ID", "Invalid folder ID encoding", 400);
                }
            };
            folder_id = match parse_requested_file_listing_folder_id(Some(val_str)) {
                Ok(folder) => folder,
                Err(error) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return json_error("INVALID_FOLDER_ID", &error, 400);
                }
            };
        }
    }

    if !saw_file {
        drop(file);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return json_error(
            "MISSING_UPLOAD_FILE",
            "Upload request has no file part",
            400,
        );
    }

    if let Err(e) = file.flush().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return json_error("WRITE_ERROR", &e.to_string(), 500);
    }
    drop(file);

    let file_size = match tokio::fs::metadata(&temp_path).await {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return json_error("METADATA_ERROR", &e.to_string(), 500);
        }
    };

    let upload_length = match checked_telegram_upload_length(file_size) {
        Ok(length) => length,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return json_error("UPLOAD_TOO_LARGE", &error, 413);
        }
    };

    if let Err(e) = bw_manager.try_reserve_up(file_size) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return json_error("BANDWIDTH_LIMIT", &e, 400);
    }

    let peer = match resolve_peer(&client, folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => {
            bw_manager.release_up(file_size);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return json_error("PEER_ERROR", &e, 400);
        }
    };

    let mut open_file = match tokio::fs::File::open(&temp_path).await {
        Ok(f) => f,
        Err(e) => {
            bw_manager.release_up(file_size);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return json_error("OPEN_ERROR", &e.to_string(), 500);
        }
    };

    let upload_res = client
        .upload_stream(&mut open_file, upload_length, filename.clone())
        .await;
    // On Windows an open file handle prevents deleting the plaintext stage.
    // Close the Telegram reader before both the error and success cleanups.
    drop(open_file);
    let uploaded_file = match upload_res {
        Ok(uf) => uf,
        Err(e) => {
            bw_manager.release_up(file_size);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return json_error("UPLOAD_FAILED", &map_error(e), 500);
        }
    };

    let message = InputMessage::new().text("").file(uploaded_file);

    let max_retries = net_config.retry_attempts();
    let base_ms = net_config.retry_base_backoff_ms();
    let max_ms = net_config.retry_max_backoff_ms();
    let respect_flood = net_config.should_respect_flood_wait();
    let mut last_err = String::new();
    let mut sent_msg = None;

    for attempt in 0..=max_retries {
        match client.send_message(&peer, message.clone()).await {
            Ok(msg) => {
                sent_msg = Some(msg);
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
                        log::info!("Respecting FLOOD_WAIT: sleeping {}s", wait);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        last_err = err;
                        continue;
                    }
                }

                if attempt < max_retries {
                    let wait = crate::vpn_optimizer::backoff_ms(attempt, base_ms, max_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                }
                last_err = err;
            }
        }
    }

    let _ = tokio::fs::remove_file(&temp_path).await;

    let msg = match sent_msg {
        Some(m) => m,
        None => {
            bw_manager.release_up(file_size);
            return json_error("SEND_MESSAGE_FAILED", &last_err, 500);
        }
    };

    let response_file = ApiFile {
        id: msg.id() as i64,
        folder_id,
        name: filename,
        size: file_size,
        mime_type: field_mime,
        created_at: msg.date().to_string(),
    };

    HttpResponse::Ok().json(response_file)
}

#[get("/api/v1/folders")]
async fn api_list_folders(
    req: HttpRequest,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let mut folders = Vec::new();
    let mut dialogs = client.iter_dialogs();
    let mut discovered = HashMap::new();

    loop {
        let dialog = match dialogs.next().await {
            Ok(Some(dialog)) => dialog,
            Ok(None) => break,
            Err(error) => {
                return json_error(
                    "LIST_FOLDERS_FAILED",
                    &format!("Could not enumerate Telegram folders: {error}"),
                    502,
                );
            }
        };
        if let Peer::Channel(ref c) = dialog.peer {
            let id = c.raw.id;
            discovered.insert(id, dialog.peer.clone());
            let name = c.raw.title.clone();
            let is_tera_folder = {
                let lower = name.to_lowercase();
                lower.contains("[tr]") || lower.contains("[td]")
            };
            if is_tera_folder {
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
                    role: None,
                });
            }
        }
    }

    {
        let mut cache = tg_state.peer_cache.write().await;
        cache.extend(discovered);
    }

    HttpResponse::Ok().json(folders)
}

#[derive(serde::Deserialize)]
struct CreateFolderRequest {
    name: String,
}

#[post("/api/v1/folders")]
async fn api_create_folder(
    req: HttpRequest,
    body: web::Json<CreateFolderRequest>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    match create_folder_inner(&body.name, &client, &tg_state.peer_cache).await {
        Ok(folder) => HttpResponse::Ok().json(folder),
        Err(e) => json_error("CREATE_FOLDER_FAILED", &e, 500),
    }
}

#[derive(serde::Deserialize)]
struct RenameFolderRequest {
    name: String,
}

#[patch("/api/v1/folders/{folder_id}")]
async fn api_rename_folder(
    req: HttpRequest,
    path: web::Path<i64>,
    body: web::Json<RenameFolderRequest>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let folder_id = path.into_inner();

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    match rename_folder_inner(folder_id, &body.name, &client, &tg_state.peer_cache).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({ "success": true })),
        Err(e) => json_error("RENAME_FOLDER_FAILED", &e, 500),
    }
}

#[delete("/api/v1/folders/{folder_id}")]
async fn api_delete_folder(
    req: HttpRequest,
    path: web::Path<i64>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let folder_id = path.into_inner();

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    match delete_folder_inner(folder_id, &client, &tg_state.peer_cache).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({ "success": true })),
        Err(e) => json_error("DELETE_FOLDER_FAILED", &e, 500),
    }
}

#[derive(Serialize)]
struct FolderStat {
    id: Option<i64>,
    name: String,
    file_count: usize,
    size_bytes: u64,
}

#[derive(Serialize)]
struct MimeStat {
    mime_type: String,
    file_count: usize,
    size_bytes: u64,
}

#[derive(Serialize)]
struct StorageStatsResponse {
    total_storage_used_bytes: u64,
    total_file_count: usize,
    folders: Vec<FolderStat>,
    mime_types: Vec<MimeStat>,
}

#[get("/api/v1/storage/stats")]
async fn api_storage_stats(
    req: HttpRequest,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let mut peers_to_scan = Vec::new();
    let me_peer = match resolve_peer(&client, None, &tg_state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("STORAGE_STATS_FAILED", &error, 502),
    };
    peers_to_scan.push((None, "Saved Messages".to_string(), me_peer));
    let mut dialogs = client.iter_dialogs();
    loop {
        let dialog = match dialogs.next().await {
            Ok(Some(dialog)) => dialog,
            Ok(None) => break,
            Err(error) => {
                return json_error(
                    "STORAGE_STATS_FAILED",
                    &format!("Could not enumerate Telegram folders: {error}"),
                    502,
                );
            }
        };
        if let Peer::Channel(ref c) = dialog.peer {
            let name = c.raw.title.clone();
            let is_tera_folder = {
                let lower = name.to_lowercase();
                lower.contains("[tr]") || lower.contains("[td]")
            };
            if is_tera_folder {
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
                peers_to_scan.push((Some(c.raw.id), display_name, dialog.peer.clone()));
            }
        }
    }

    let mut total_storage_used_bytes: u64 = 0;
    let mut total_file_count: usize = 0;
    let mut folder_stats = Vec::new();
    let mut mime_map: HashMap<String, (usize, u64)> = HashMap::new();

    for (fid, folder_name, peer) in peers_to_scan {
        let mut file_count = 0;
        let mut size_bytes: u64 = 0;
        let mut msgs = client.iter_messages(peer).limit(200);
        loop {
            let msg = match msgs.next().await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "STORAGE_STATS_FAILED",
                        &format!("Could not enumerate Telegram files: {error}"),
                        502,
                    );
                }
            };
            if let Some(doc) = msg.media() {
                let (size, mime) = match doc {
                    Media::Document(d) => {
                        let size = match validated_telegram_document_size(d.size()) {
                            Ok(size) => size,
                            Err(error) => return json_error("STORAGE_STATS_FAILED", &error, 502),
                        };
                        (
                            size,
                            d.mime_type()
                                .unwrap_or("application/octet-stream")
                                .to_string(),
                        )
                    }
                    Media::Photo(_) => (0, "image/jpeg".to_string()),
                    _ => continue,
                };
                file_count += 1;
                size_bytes = match size_bytes.checked_add(size) {
                    Some(sum) => sum,
                    None => {
                        return json_error("STORAGE_STATS_FAILED", "Storage size overflow", 502)
                    }
                };

                let mime_entry = mime_map.entry(mime).or_insert((0, 0));
                mime_entry.0 += 1;
                mime_entry.1 = match mime_entry.1.checked_add(size) {
                    Some(sum) => sum,
                    None => return json_error("STORAGE_STATS_FAILED", "MIME size overflow", 502),
                };
            }
        }

        total_storage_used_bytes = match total_storage_used_bytes.checked_add(size_bytes) {
            Some(sum) => sum,
            None => return json_error("STORAGE_STATS_FAILED", "Total storage size overflow", 502),
        };
        total_file_count += file_count;

        folder_stats.push(FolderStat {
            id: fid,
            name: folder_name,
            file_count,
            size_bytes,
        });
    }

    let mime_types = mime_map
        .into_iter()
        .map(|(mime_type, (file_count, size_bytes))| MimeStat {
            mime_type,
            file_count,
            size_bytes,
        })
        .collect();

    HttpResponse::Ok().json(StorageStatsResponse {
        total_storage_used_bytes,
        total_file_count,
        folders: folder_stats,
        mime_types,
    })
}

#[derive(Serialize)]
struct DuplicateGroup {
    name: String,
    size: u64,
    files: Vec<ApiFile>,
    // Name/size is only a heuristic. Callers must verify actual content
    // before using a candidate group for destructive deduplication.
    verified: bool,
    match_basis: &'static str,
}

#[cfg(test)]
mod duplicate_candidate_safety_tests {
    #[test]
    fn candidate_duplicate_groups_disclose_lack_of_content_verification() {
        let group = super::DuplicateGroup {
            name: "report.txt".to_string(),
            size: 42,
            files: Vec::new(),
            verified: false,
            match_basis: "name_and_size_only",
        };
        let response = serde_json::to_value(group).unwrap();
        assert_eq!(response["verified"], false);
        assert_eq!(response["match_basis"], "name_and_size_only");
        let source = include_str!("api_routes.rs");
        let route = source
            .rsplit_once("async fn api_storage_duplicates(")
            .unwrap()
            .1
            .split_once("fn confirmed_empty_folder<")
            .unwrap()
            .0;
        assert!(
            route.contains("verified: false")
                && route.contains("match_basis: \"name_and_size_only\""),
            "unverified name-size candidates must not be advertised as content-verified duplicates"
        );
    }

    #[test]
    fn duplicate_detection_must_not_label_unknown_size_photos_as_matching_files() {
        let source = include_str!("api_routes.rs");
        let route = source
            .rsplit_once("async fn api_storage_duplicates(")
            .unwrap()
            .1
            .split_once("fn confirmed_empty_folder<")
            .unwrap()
            .0;
        assert!(
            !route.contains("Media::Photo(_) => (\"Photo.jpg\".to_string(), 0"),
            "unrelated Telegram photos share a placeholder name and zero size, not a content identity"
        );
    }
}

#[get("/api/v1/storage/duplicates")]
async fn api_storage_duplicates(
    req: HttpRequest,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let mut peers_to_scan = Vec::new();
    let me_peer = match resolve_peer(&client, None, &tg_state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("DUPLICATES_FAILED", &error, 502),
    };
    peers_to_scan.push((None, me_peer));
    let mut dialogs = client.iter_dialogs();
    loop {
        let dialog = match dialogs.next().await {
            Ok(Some(dialog)) => dialog,
            Ok(None) => break,
            Err(error) => {
                return json_error(
                    "DUPLICATES_FAILED",
                    &format!("Could not enumerate Telegram folders: {error}"),
                    502,
                );
            }
        };
        if let Peer::Channel(ref c) = dialog.peer {
            let name = c.raw.title.clone();
            let is_tera_folder = {
                let lower = name.to_lowercase();
                lower.contains("[tr]") || lower.contains("[td]")
            };
            if is_tera_folder {
                peers_to_scan.push((Some(c.raw.id), dialog.peer.clone()));
            }
        }
    }

    let mut file_groups: HashMap<(String, u64), Vec<ApiFile>> = HashMap::new();

    for (fid, peer) in peers_to_scan {
        let mut msgs = client.iter_messages(peer).limit(200);
        loop {
            let msg = match msgs.next().await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(error) => {
                    return json_error(
                        "DUPLICATES_FAILED",
                        &format!("Could not enumerate Telegram files: {error}"),
                        502,
                    );
                }
            };
            if let Some(doc) = msg.media() {
                let (name, size, mime) = match doc {
                    Media::Document(d) => {
                        let size = match validated_telegram_document_size(d.size()) {
                            Ok(size) => size,
                            Err(error) => return json_error("DUPLICATES_FAILED", &error, 502),
                        };
                        (
                            d.name().to_string(),
                            size,
                            d.mime_type().map(|s| s.to_string()),
                        )
                    }
                    // Telegram photo messages do not provide an exact
                    // document filename/size to compare. Their synthetic
                    // Photo.jpg/0 metadata would falsely group *every*
                    // unrelated photo as a duplicate.
                    Media::Photo(_) => continue,
                    _ => continue,
                };

                file_groups
                    .entry((name.clone(), size))
                    .or_default()
                    .push(ApiFile {
                        id: msg.id() as i64,
                        folder_id: fid,
                        name,
                        size,
                        mime_type: mime,
                        created_at: msg.date().to_string(),
                    });
            }
        }
    }

    let duplicates: Vec<DuplicateGroup> = file_groups
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|((name, size), files)| DuplicateGroup {
            name,
            size,
            files,
            verified: false,
            match_basis: "name_and_size_only",
        })
        .collect();

    HttpResponse::Ok().json(duplicates)
}

// A Telegram folder is only provably empty if the iterator reaches clean
// EOF without seeing *any* message. A non-media message is still content;
// a transport error is an unknown state, never evidence for deletion.
fn confirmed_empty_folder<T, E>(first: Result<Option<T>, E>) -> Result<bool, E> {
    first.map(|message| message.is_none())
}

#[cfg(test)]
mod empty_folder_safety_tests {
    #[test]
    fn only_clean_telegram_eof_proves_folder_is_empty() {
        use super::confirmed_empty_folder;
        assert_eq!(confirmed_empty_folder::<u8, &str>(Ok(None)), Ok(true));
        assert_eq!(confirmed_empty_folder::<u8, &str>(Ok(Some(17))), Ok(false));
        assert_eq!(
            confirmed_empty_folder::<u8, &str>(Err("Telegram timeout")),
            Err("Telegram timeout"),
        );
    }

    #[test]
    fn empty_folder_route_must_not_treat_errors_or_non_media_as_empty() {
        let source = include_str!("api_routes.rs");
        let route = source
            .rsplit_once("async fn api_empty_folders(")
            .unwrap()
            .1
            .split_once("async fn api_get_file_thumbnail(")
            .unwrap()
            .0;
        assert!(
            !route.contains(".next().await.ok().flatten()"),
            "Telegram iterator errors must never classify a folder as empty"
        );
        assert!(
            route.contains("EMPTY_FOLDERS_FAILED"),
            "empty-folder checks must fail explicitly when Telegram enumeration fails"
        );
        assert!(
            !route.contains("if msg.media().is_some()"),
            "a non-media message must not prove that a folder is empty"
        );
    }
}

#[get("/api/v1/folders/empty")]
async fn api_empty_folders(
    req: HttpRequest,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let mut folders_to_check = Vec::new();
    let mut dialogs = client.iter_dialogs();
    loop {
        let dialog = match dialogs.next().await {
            Ok(Some(dialog)) => dialog,
            Ok(None) => break,
            Err(error) => {
                return json_error(
                    "EMPTY_FOLDERS_FAILED",
                    &format!("Could not enumerate Telegram folders: {error}"),
                    502,
                );
            }
        };
        if let Peer::Channel(ref c) = dialog.peer {
            let name = c.raw.title.clone();
            let is_tera_folder = {
                let lower = name.to_lowercase();
                lower.contains("[tr]") || lower.contains("[td]")
            };
            if is_tera_folder {
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
                folders_to_check.push((c.raw.id, display_name, dialog.peer.clone()));
            }
        }
    }

    let mut empty_folders = Vec::new();

    for (fid, display_name, peer) in folders_to_check {
        let mut msgs = client.iter_messages(peer).limit(1);
        let is_empty = match confirmed_empty_folder(msgs.next().await) {
            Ok(is_empty) => is_empty,
            Err(error) => {
                return json_error(
                    "EMPTY_FOLDERS_FAILED",
                    &format!("Could not verify Telegram folder emptiness: {error}"),
                    502,
                );
            }
        };
        if is_empty {
            empty_folders.push(FolderMetadata {
                id: fid,
                name: display_name,
                parent_id: None,
                username: None,
                is_public: false,
                group_id: None,
                display_order: 0,
                role: None,
            });
        }
    }

    HttpResponse::Ok().json(empty_folders)
}

#[get("/api/v1/files/{message_id}/thumbnail")]
async fn api_get_file_thumbnail(
    req: HttpRequest,
    path: web::Path<i32>,
    query: web::Query<FolderQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let message_id = path.into_inner();
    let folder_id = query.folder_id;

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let peer = match resolve_peer(&client, folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("PEER_ERROR", &e, 400),
    };

    let messages = match client.get_messages_by_id(&peer, &[message_id]).await {
        Ok(msgs) => msgs,
        Err(e) => return json_error("GET_MESSAGE_ERROR", &e.to_string(), 500),
    };

    if let Some(m) = messages.into_iter().flatten().next() {
        if let Some(media) = m.media() {
            let (is_image, ext) = match &media {
                Media::Photo(_) => (true, "jpg"),
                Media::Document(d) => {
                    let mime = d.mime_type().unwrap_or("");
                    if mime.starts_with("image/") || mime.starts_with("video/") {
                        if !d.thumbs().is_empty() {
                            (true, "jpg")
                        } else {
                            (false, "")
                        }
                    } else {
                        (false, "")
                    }
                }
                _ => (false, ""),
            };

            if is_image {
                let temp_path = std::env::temp_dir().join(format!(
                    "thumb_{}_{}",
                    message_id,
                    rand::random::<u32>()
                ));
                let temp_path_str = temp_path.to_string_lossy().to_string();

                let thumbs = match &media {
                    Media::Photo(p) => p.thumbs(),
                    Media::Document(d) => d.thumbs(),
                    _ => vec![],
                };

                let download_success = if let Some(thumb) = thumbs
                    .iter()
                    .filter(|t| t.size() > 0)
                    .max_by_key(|t| t.size())
                {
                    client.download_media(thumb, &temp_path_str).await.is_ok()
                } else {
                    client.download_media(&media, &temp_path_str).await.is_ok()
                };

                if download_success {
                    if let Ok(bytes) = tokio::fs::read(&temp_path).await {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        let mime = match ext {
                            "png" => "image/png",
                            "gif" => "image/gif",
                            _ => "image/jpeg",
                        };
                        return HttpResponse::Ok().content_type(mime).body(bytes);
                    }
                }
                let _ = tokio::fs::remove_file(&temp_path).await;
            }
        }
    }

    json_error("NOT_FOUND", "Thumbnail not found", 404)
}

#[derive(Serialize)]
struct MediaInfoResponse {
    duration_secs: Option<f64>,
    width: Option<i32>,
    height: Option<i32>,
    audio_title: Option<String>,
    audio_performer: Option<String>,
}

#[get("/api/v1/files/{message_id}/media-info")]
async fn api_media_info(
    req: HttpRequest,
    path: web::Path<i32>,
    query: web::Query<FolderQuery>,
    tg_state: web::Data<Arc<TelegramState>>,
    api_state: web::Data<ApiState>,
) -> impl Responder {
    if let Err(e) = check_auth(&req, &api_state) {
        return *e;
    }
    let message_id = path.into_inner();
    let folder_id = query.folder_id;

    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return json_error("NOT_CONNECTED", "Telegram client is not connected", 503),
    };

    let peer = match resolve_peer(&client, folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => return json_error("PEER_ERROR", &e, 400),
    };

    let messages = match client.get_messages_by_id(&peer, &[message_id]).await {
        Ok(msgs) => msgs,
        Err(e) => return json_error("GET_MESSAGE_ERROR", &e.to_string(), 500),
    };

    let msg = match messages.into_iter().flatten().next() {
        Some(m) => m,
        None => return json_error("NOT_FOUND", "File message not found", 404),
    };

    let media = match msg.media() {
        Some(m) => m,
        None => return json_error("NO_MEDIA", "Message has no media", 400),
    };

    let mut info = MediaInfoResponse {
        duration_secs: None,
        width: None,
        height: None,
        audio_title: None,
        audio_performer: None,
    };

    if let Media::Document(d) = media {
        if let Some(tl::enums::Document::Document(doc)) = &d.raw.document {
            for attr in &doc.attributes {
                match attr {
                    tl::enums::DocumentAttribute::Video(v) => {
                        info.duration_secs = Some(v.duration);
                        info.width = Some(v.w);
                        info.height = Some(v.h);
                    }
                    tl::enums::DocumentAttribute::Audio(a) => {
                        info.duration_secs = Some(a.duration as f64);
                        info.audio_title = a.title.clone();
                        info.audio_performer = a.performer.clone();
                    }
                    _ => {}
                }
            }
        }
    }

    HttpResponse::Ok().json(info)
}

/// Register all API routes on the Actix App
pub fn configure_api(cfg: &mut web::ServiceConfig) {
    cfg.service(api_health)
        .service(api_list_files)
        .service(api_get_file)
        .service(api_download_file)
        .service(api_bulk_files)
        .service(api_search_files)
        .service(api_delete_file)
        .service(api_copy_file)
        .service(api_update_file)
        .service(api_upload_file)
        .service(api_list_folders)
        .service(api_create_folder)
        .service(api_rename_folder)
        .service(api_delete_folder)
        .service(api_storage_stats)
        .service(api_storage_duplicates)
        .service(api_empty_folders)
        .service(api_get_file_thumbnail)
        .service(api_media_info);
}
