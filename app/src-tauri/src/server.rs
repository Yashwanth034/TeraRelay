#[path = "logical_stream.rs"]
mod logical_stream;

use crate::commands::utils::resolve_peer;
use crate::commands::TelegramState;
use crate::transcode::TranscodeManager;
use actix_cors::Cors;
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use grammers_client::types::Media;

use std::net::TcpListener;
use std::sync::Arc;

/// Holds the per-session streaming token for Actix validation
pub struct StreamTokenData {
    pub token: String,
}

#[derive(serde::Deserialize)]
struct StreamQuery {
    token: Option<String>,
}

#[get("/subtitle/{file_key}/{stream_index}.vtt")]
async fn serve_subtitle(
    path: web::Path<(String, i32)>,
    query: web::Query<StreamQuery>,
    manager: web::Data<Arc<TranscodeManager>>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    match &query.token {
        Some(token) if token == &token_data.token => {}
        _ => return HttpResponse::Forbidden().body("Invalid or missing stream token"),
    }

    let (file_key, stream_index) = path.into_inner();
    if stream_index < 0
        || file_key
            .chars()
            .any(|c| !c.is_alphanumeric() && c != '_' && c != '-')
    {
        return HttpResponse::BadRequest().body("Invalid subtitle path");
    }

    let root = manager.cache_root.join("subtitles");
    let file_path = root.join(&file_key).join(format!("{stream_index}.vtt"));
    let safe_path = match file_path.canonicalize() {
        Ok(path) => path,
        Err(_) => return HttpResponse::NotFound().body("Subtitle not found"),
    };
    let safe_root = root.canonicalize().unwrap_or(root);
    if !safe_path.starts_with(&safe_root) {
        return HttpResponse::Forbidden().body("Access denied");
    }

    match std::fs::read(safe_path) {
        Ok(bytes) => HttpResponse::Ok()
            .insert_header(("Content-Type", "text/vtt; charset=utf-8"))
            .insert_header(("Cache-Control", "private, max-age=3600"))
            .body(bytes),
        Err(_) => HttpResponse::NotFound().body("Subtitle not found"),
    }
}

pub fn parse_range_header(header_val: &str, total_size: u64) -> Option<(u64, u64)> {
    if total_size == 0 {
        return None;
    }

    let raw = header_val.trim();
    let value = raw.strip_prefix("bytes=")?;
    if value.contains(',') {
        // Multiple ranges are not supported by this lightweight local server.
        return None;
    }

    let (start_text, end_text) = value.split_once('-')?;
    let start_text = start_text.trim();
    let end_text = end_text.trim();

    if start_text.is_empty() {
        // RFC 7233 suffix-byte-range-spec: "bytes=-N" means the last N bytes.
        let suffix_len = end_text.parse::<u64>().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let suffix_len = suffix_len.min(total_size);
        return Some((total_size - suffix_len, total_size - 1));
    }

    let start = start_text.parse::<u64>().ok()?;
    if start >= total_size {
        return None;
    }

    let end = if end_text.is_empty() {
        total_size - 1
    } else {
        end_text.parse::<u64>().ok()?.min(total_size - 1)
    };

    (start <= end).then_some((start, end))
}

#[cfg(test)]
mod range_tests {
    use super::parse_range_header;

    #[test]
    fn parses_closed_range() {
        assert_eq!(parse_range_header("bytes=100-199", 1_000), Some((100, 199)));
    }

    #[test]
    fn parses_open_ended_range() {
        assert_eq!(parse_range_header("bytes=900-", 1_000), Some((900, 999)));
    }

    #[test]
    fn parses_suffix_range() {
        assert_eq!(parse_range_header("bytes=-100", 1_000), Some((900, 999)));
        assert_eq!(parse_range_header("bytes=-5000", 1_000), Some((0, 999)));
    }

    #[test]
    fn clamps_end_to_file_size() {
        assert_eq!(
            parse_range_header("bytes=950-5000", 1_000),
            Some((950, 999))
        );
    }

    #[test]
    fn rejects_invalid_or_unsatisfiable_ranges() {
        assert_eq!(parse_range_header("bytes=1000-", 1_000), None);
        assert_eq!(parse_range_header("bytes=200-100", 1_000), None);
        assert_eq!(parse_range_header("bytes=-0", 1_000), None);
        assert_eq!(parse_range_header("bytes=0-1,4-5", 1_000), None);
        assert_eq!(parse_range_header("items=0-1", 1_000), None);
        assert_eq!(parse_range_header("bytes=0-1", 0), None);
    }

    #[test]
    fn mp4_http_media_preserves_private_range_cache_until_decoder_fix() {
        use super::logical_stream::media_stream_cache_control;

        assert_eq!(
            media_stream_cache_control("video/mp4"),
            "private, max-age=120"
        );
        assert_eq!(
            media_stream_cache_control("VIDEO/MP4"),
            "private, max-age=120"
        );
        assert_eq!(
            media_stream_cache_control("video/mp4; codecs=avc1"),
            "private, max-age=120"
        );
        for other in [
            "video/x-matroska",
            "video/webm",
            "image/jpeg",
            "application/pdf",
            "application/octet-stream",
        ] {
            assert_eq!(
                media_stream_cache_control(other),
                "private, max-age=120",
                "unrelated media cannot lose its cache policy"
            );
        }
    }

    #[test]
    fn invalid_signed_telegram_sizes_fail_before_http_streaming() {
        assert_eq!(super::checked_stream_document_size(0_i64).unwrap(), 0);
        assert_eq!(super::checked_stream_document_size(8192_i64).unwrap(), 8192);
        assert_eq!(
            super::checked_stream_document_size(i64::MAX).unwrap(),
            i64::MAX as u64
        );
        assert!(super::checked_stream_document_size(-1_i64).is_err());
        assert!(super::checked_stream_document_size(i64::MIN).is_err());

        let source = include_str!("server.rs");
        let shared = source
            .rsplit_once("pub fn build_media_response(")
            .unwrap()
            .1
            .split_once("struct LogicalStreamMedia")
            .unwrap()
            .0;
        let parts = source
            .rsplit_once("async fn resolve_stream_media(")
            .unwrap()
            .1
            .split_once("fn build_logical_stream_response(")
            .unwrap()
            .0;
        let video = source
            .rsplit_once("fn build_logical_stream_response(")
            .unwrap()
            .1
            .split_once("logical_stream::range_response(")
            .unwrap()
            .0;
        for section in [shared, parts, video] {
            assert!(
                !section.contains("size() as u64")
                    && section.contains("checked_stream_document_size("),
                "all streaming routes must reject negative Telegram sizes before casting"
            );
        }
    }

    #[test]
    fn cdn_seek_chunk_index_must_reject_i32_overflow() {
        const CHUNK_BYTES: u64 = 65_536;
        assert_eq!(super::checked_cdn_seek_chunk_index(0).unwrap(), 0);
        assert_eq!(
            super::checked_cdn_seek_chunk_index(CHUNK_BYTES * 8).unwrap(),
            8
        );
        assert_eq!(
            super::checked_cdn_seek_chunk_index(i32::MAX as u64 * CHUNK_BYTES).unwrap(),
            i32::MAX
        );
        assert!(
            super::checked_cdn_seek_chunk_index((i32::MAX as u64 + 1) * CHUNK_BYTES).is_err(),
            "large Telegram seek offsets cannot wrap into an earlier download chunk"
        );
        assert!(super::checked_cdn_seek_chunk_index(u64::MAX).is_err());
        let source = include_str!("server.rs");
        let route = source
            .rsplit_once("pub fn build_media_response(")
            .unwrap()
            .1
            .split_once("struct LogicalStreamMedia")
            .unwrap()
            .0;
        assert!(
            route.contains("checked_cdn_seek_chunk_index(cdn_aligned_start)")
                && !route.contains("(cdn_aligned_start / CHUNK_SIZE as u64) as i32"),
            "direct HTTP range streaming must use checked Telegram chunk indices"
        );
    }

    #[test]
    fn single_document_http_stream_uses_error_propagating_range_guard() {
        // Integration contract: the regular document/photo HTTP response
        // must use the same byte-bounded, fallible stream path as logical
        // multi-part media, not treat an incomplete Telegram read as EOF.
        let source = include_str!("server.rs");
        let direct = source
            .rsplit_once("pub fn build_media_response(")
            .unwrap()
            .1
            .split_once("struct LogicalStreamMedia")
            .unwrap()
            .0;
        assert!(
            direct.contains("logical_stream::bounded_chunks("),
            "single-document stream must propagate download errors and short bodies"
        );
    }
}

#[cfg(test)]
mod disposition_tests {
    use super::attachment_content_disposition;
    use actix_web::http::header::HeaderValue;

    #[test]
    fn ordinary_download_name_retains_compatible_ascii_fallback() {
        let header = attachment_content_disposition("report (final).pdf");
        assert_eq!(
            header,
            "attachment; filename=\"report (final).pdf\"; filename*=UTF-8''report%20%28final%29.pdf"
        );
        HeaderValue::from_str(&header).unwrap();
    }

    #[test]
    fn malicious_and_utf8_filenames_cannot_inject_response_headers() {
        let filename = "report\"; filename=evil\r\nX-Forged:yes/\\doc.txt";
        let header = attachment_content_disposition(filename);
        HeaderValue::from_str(&header).unwrap();
        assert!(header.starts_with("attachment; filename=\""));
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
        assert!(!header.contains("filename=evil"));
        assert!(!header.contains("X-Forged:"));
        assert!(!header.contains("/\\"));
        assert!(header.contains("filename*=UTF-8''"));
        assert!(header.contains("%22%3B"));
        assert!(header.contains("%3D"));
        assert_eq!(
            attachment_content_disposition(""),
            "attachment; filename=\"download\"; filename*=UTF-8''download"
        );

        let unicode = attachment_content_disposition("日本語.txt");
        assert!(unicode.contains("filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E.txt"));
        HeaderValue::from_str(&unicode).unwrap();
    }
}

/// Format a filename as a safe ASCII fallback and an RFC 5987 UTF-8 value.
/// Never interpolate untrusted filenames directly into HTTP header syntax.
fn attachment_content_disposition(filename: &str) -> String {
    use std::fmt::Write;

    // Reject path separators and control characters as download-name content.
    let clean: String = filename
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let clean = if clean.trim().is_empty() {
        "download"
    } else {
        clean.as_str()
    };

    let fallback: String = clean
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '_' | '-' | '(' | ')') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    let mut encoded = String::with_capacity(clean.len());
    for byte in clean.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// Extra headers to inject into streaming responses (e.g. Cache-Control, Content-Disposition).
pub struct StreamingExtras {
    pub extra_headers: Vec<(&'static str, String)>,
    pub log_label: &'static str,
}

// Telegram document sizes are signed. A negative value is invalid
// metadata, not a multi-exabyte HTTP Content-Length or video part size.
fn checked_stream_document_size<T: TryInto<u64>>(size: T) -> Result<u64, &'static str> {
    size.try_into()
        .map_err(|_| "Invalid Telegram document size")
}

// Grammers indexes 64 KiB chunks with a signed 32-bit offset. Never wrap
// a large HTTP byte-range seek into an unrelated download chunk.
fn checked_cdn_seek_chunk_index(offset: u64) -> Result<i32, &'static str> {
    i32::try_from(offset / 65_536)
        .map_err(|_| "Requested media offset exceeds Telegram download limits")
}

/// Build a streaming HTTP response for a Telegram media file with optional byte-range support.
/// This is the single shared implementation used by the streaming server, REST API, and share routes.
pub fn build_media_response(
    client: &grammers_client::Client,
    media: &Media,
    req: &actix_web::HttpRequest,
    mime: &str,
    filename: Option<&str>,
    extras: StreamingExtras,
) -> HttpResponse {
    let size = match media {
        Media::Document(d) => match checked_stream_document_size(d.size()) {
            Ok(size) => Some(size),
            Err(message) => return HttpResponse::BadGateway().body(message),
        },
        // Telegram photos don't expose one stable document size through this
        // media abstraction. Stream them chunked instead of advertising
        // Content-Length: 0, which makes browsers treat the response as empty.
        Media::Photo(_) => None,
        _ => None,
    };

    // Parse a single HTTP byte range. Invalid/unsatisfiable ranges must return
    // 416 rather than silently falling back to a full 200 response; PDF.js and
    // media elements rely on this distinction while seeking.
    let mut start_byte = 0u64;
    let mut end_byte = size.map(|value| value.saturating_sub(1)).unwrap_or(0);
    let mut is_range = false;

    if let (Some(total_size), Some(range_header)) =
        (size, req.headers().get(actix_web::http::header::RANGE))
    {
        let range_str = match range_header.to_str() {
            Ok(value) => value,
            Err(_) => {
                return HttpResponse::RangeNotSatisfiable()
                    .insert_header(("Content-Range", format!("bytes */{}", total_size)))
                    .insert_header(("Accept-Ranges", "bytes"))
                    .finish();
            }
        };

        match parse_range_header(range_str, total_size) {
            Some((start, end)) => {
                start_byte = start;
                end_byte = end;
                is_range = true;
            }
            None => {
                return HttpResponse::RangeNotSatisfiable()
                    .insert_header(("Content-Range", format!("bytes */{}", total_size)))
                    .insert_header(("Accept-Ranges", "bytes"))
                    .finish();
            }
        }
    }

    let content_length = size.map(|total_size| {
        if is_range {
            end_byte - start_byte + 1
        } else {
            total_size
        }
    });

    // Chunk alignment for Telegram's upload.getFile offset requirement.
    //
    // CRITICAL: Without the `precise` flag (which grammers-client does not
    // expose), Telegram may route the request through a CDN that rounds the
    // offset down to a CDN chunk boundary (commonly 512 KB = 524288 bytes).
    // If our requested offset is not aligned to this boundary, the CDN
    // silently returns data starting from the rounded-down position.
    //
    // Example: requesting offset 111935488 (213.48 × 512 KB) gets rounded
    // to 111673344 (213 × 512 KB), introducing a 262 KB shift. This
    // misalignment accumulates across successive Range requests and
    // eventually corrupts the MP4 box parsing (triggering the "ORrI" error).
    //
    // Fix: always align to 512 KB boundaries, then slice off the leading
    // bytes to serve the exact byte range the client requested.
    let mut download_iter = client.iter_download(media);
    let mut bytes_to_skip: usize = 0;

    if start_byte > 0 {
        /// MTProto chunk size (must be divisible by grammers' MIN_CHUNK_SIZE).
        /// 65536 is safe — it is the default and widely tested.
        const CHUNK_SIZE: i32 = 65536;
        /// Telegram CDN alignment boundary. 512 KB is the largest observed
        /// CDN chunk size; aligning to this boundary prevents ANY rounding.
        const CDN_ALIGNMENT: u64 = 524288; // 512 KB

        // 1) Round the requested start down to a CDN-safe boundary.
        let cdn_aligned_start = (start_byte / CDN_ALIGNMENT) * CDN_ALIGNMENT;

        // 2) Compute how many 64 KB chunks to skip to reach that boundary.
        let chunk_index = match checked_cdn_seek_chunk_index(cdn_aligned_start) {
            Ok(index) => index,
            Err(message) => return HttpResponse::BadGateway().body(message),
        };

        // Always set chunk size for predictable download behaviour.
        download_iter = download_iter.chunk_size(CHUNK_SIZE);
        if chunk_index > 0 {
            download_iter = download_iter.skip_chunks(chunk_index);
        }

        // 3) Leading bytes between the CDN-aligned offset and the client's
        //    actual requested start must be discarded.
        bytes_to_skip = (start_byte - cdn_aligned_start) as usize;

        // Safety: cdn_aligned_start ≤ start_byte by construction.
        debug_assert!(
            cdn_aligned_start <= start_byte,
            "CDN alignment invariant violated: aligned {} > requested {}",
            cdn_aligned_start,
            start_byte
        );

        log::debug!(
            "Range alignment: requested={}, cdn_aligned={}, chunk_index={}, bytes_to_skip={}",
            start_byte,
            cdn_aligned_start,
            chunk_index,
            bytes_to_skip,
        );
    }

    // Convert Grammers' fallible download iterator into the same bounded
    // byte stream used by logical multi-part media. That shared path
    // propagates Telegram errors and incomplete bodies instead of treating
    // them as a clean EOF after advertising Content-Length.
    let label = extras.log_label;
    let source = futures::stream::unfold(Some(download_iter), move |download| async move {
        let mut download = download?;
        match download.next().await {
            Ok(Some(data)) => Some((Ok(data), Some(download))),
            Ok(None) => None,
            Err(error) => {
                log::error!("{} stream error: {}", label, error);
                Some((Err(error), None))
            }
        }
    });
    let stream = logical_stream::bounded_chunks(source, bytes_to_skip as u64, content_length);

    let mut resp = if is_range {
        let total_size = size.expect("range responses require a known size");
        let mut r = HttpResponse::PartialContent();
        r.insert_header((
            "Content-Range",
            format!("bytes {}-{}/{}", start_byte, end_byte, total_size),
        ));
        if let Some(content_length) = content_length {
            r.insert_header(("Content-Length", content_length.to_string()));
        }
        r
    } else {
        let mut r = HttpResponse::Ok();
        if let Some(content_length) = content_length {
            r.insert_header(("Content-Length", content_length.to_string()));
        }
        r
    };

    resp.insert_header(("Content-Type", mime.to_owned()));
    resp.insert_header(("Accept-Ranges", "bytes"));

    if let Some(fname) = filename {
        resp.insert_header(("Content-Disposition", attachment_content_disposition(fname)));
    }

    for (key, val) in &extras.extra_headers {
        resp.insert_header((*key, val.clone()));
    }
    // Keep caller-specific download/share headers intact. The authenticated
    // /stream path applies its bounded private cache policy separately.
    resp.streaming(stream)
}

struct LogicalStreamMedia {
    parts: Vec<Media>,
    mime: String,
}

async fn resolve_stream_media(
    client: &grammers_client::Client,
    peer: &grammers_client::types::Peer,
    db: &crate::db::DbConnection,
    cache: &logical_stream::PartCache,
    folder_id: Option<i64>,
    message_id: i32,
) -> Result<LogicalStreamMedia, String> {
    let manifest = if let Some(backing_id) = folder_id {
        let conn = db.lock().map_err(|_| "DB poisoned".to_string())?;
        match crate::commands::logical_channels::logical_channel_id_for_backing(&conn, backing_id)?
        {
            Some(logical_id) => crate::commands::logical_files::cached_manifest_for_first_message(
                &conn,
                &logical_id,
                i64::from(message_id),
            )?,
            None => None,
        }
    } else {
        None
    };
    // Peer kind and ID keep channels and each Saved Messages account distinct.
    let peer_key = match peer {
        grammers_client::types::Peer::Channel(channel) => (true, channel.raw.id, message_id),
        grammers_client::types::Peer::User(user) => (false, user.raw.id(), message_id),
        _ => return Err("Unsupported streaming peer".to_string()),
    };
    let cached = if manifest.is_none() {
        cache.get(peer_key)
    } else {
        None
    };
    let ids: Vec<i32> = if let Some(manifest) = &manifest {
        manifest
            .chunks
            .iter()
            .map(|chunk| {
                i32::try_from(chunk.message_id).map_err(|_| "Invalid stored chunk ID".to_string())
            })
            .collect::<Result<_, _>>()?
    } else if let Some(cached) = &cached {
        cached.ids.clone()
    } else {
        crate::commands::fs::resolve_parts(client, peer, message_id, true).await?
    };
    let messages = crate::commands::fs::get_messages_in_batches(client, peer, &ids)
        .await
        .map_err(|e| {
            cache.invalidate(peer_key);
            e.to_string()
        })?;
    let mut messages: std::collections::HashMap<_, _> = messages
        .into_iter()
        .flatten()
        .map(|message| (message.id(), message))
        .collect();
    let mut parts = Vec::with_capacity(ids.len());
    let mut original_name = manifest.as_ref().map(|m| m.original_name.clone());
    let mut total_size = 0u64;
    let mut metadata = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        let message = messages.remove(id).ok_or_else(|| {
            cache.invalidate(peer_key);
            format!("Stored video part {} is missing", id)
        })?;
        let media = message.media().ok_or_else(|| {
            cache.invalidate(peer_key);
            format!("Video part {} has no media", id)
        })?;
        if let Media::Document(document) = &media {
            let size = checked_stream_document_size(document.size())
                .map_err(|error| format!("Video part {id}: {error}"))?;
            let stored_name = if message.text().is_empty() {
                document.name()
            } else {
                message.text()
            };
            metadata.push((size, stored_name.to_string()));
            total_size = total_size.checked_add(size).ok_or("Video size overflow")?;
            if let Some(manifest) = &manifest {
                if manifest.chunks[index].size != size {
                    return Err(format!(
                        "Stored video part {} size does not match its manifest",
                        id
                    ));
                }
            }
            if original_name.is_none() {
                let name = if message.text().is_empty() {
                    document.name()
                } else {
                    message.text()
                };
                original_name = Some(
                    crate::commands::fs::parse_part_name(name)
                        .map(|(base, _, _, _)| base)
                        .unwrap_or(name)
                        .to_string(),
                );
            }
        } else if ids.len() > 1 || manifest.is_some() {
            return Err("Multipart video contains a non-document part".to_string());
        }
        parts.push(media);
    }
    if parts.is_empty() {
        return Err("Video message not found".to_string());
    }
    if let Some(manifest) = &manifest {
        if total_size != manifest.total_size {
            return Err("Stored video size does not match its manifest".to_string());
        }
    }
    if let Some(cached) = cached {
        if cached.metadata != metadata {
            cache.invalidate(peer_key);
            // One fresh resolution handles renamed/replaced parts without
            // leaving the browser stuck with stale part IDs.
            return Box::pin(resolve_stream_media(
                client, peer, db, cache, folder_id, message_id,
            ))
            .await;
        }
    } else if manifest.is_none() && ids.len() > 1 {
        cache.insert(peer_key, ids, metadata);
    }
    // Split documents can have an opaque Telegram MIME type. Recover the
    // original container from the manifest/name, without exposing part suffixes.
    let mime = manifest
        .as_ref()
        .and_then(|m| m.mime_type.clone())
        .filter(|m| m != "application/octet-stream")
        .or_else(|| {
            original_name
                .as_ref()
                .and_then(|name| mime_guess::from_path(name).first_raw().map(str::to_string))
        })
        .unwrap_or_else(|| mime_type_from_media(&parts[0]));
    Ok(LogicalStreamMedia { parts, mime })
}

fn build_logical_stream_response(
    client: &grammers_client::Client,
    media: LogicalStreamMedia,
    req: &actix_web::HttpRequest,
) -> HttpResponse {
    if media.parts.len() == 1 && !matches!(media.parts[0], Media::Document(_)) {
        return build_media_response(
            client,
            &media.parts[0],
            req,
            &media.mime,
            None,
            StreamingExtras {
                extra_headers: vec![(
                    "Cache-Control",
                    logical_stream::media_stream_cache_control(&media.mime).to_string(),
                )],
                log_label: "Stream",
            },
        );
    }
    let sizes: Vec<u64> = match media
        .parts
        .iter()
        .map(|part| match part {
            Media::Document(document) => checked_stream_document_size(document.size()),
            _ => Ok(0),
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(sizes) => sizes,
        Err(message) => return HttpResponse::BadGateway().body(message),
    };
    let client = client.clone();
    logical_stream::range_response(&sizes, &media.mime, req, move |ranges| {
        async_stream::stream! {
            use futures::StreamExt;
            const CHUNK_SIZE: u64 = 524_288;
            for range in ranges {
                let chunk_index = match i32::try_from(range.start / CHUNK_SIZE) {
                    Ok(index) => index,
                    Err(_) => {
                        yield Err(actix_web::error::ErrorBadGateway("Video offset is too large"));
                        return;
                    }
                };
                let download = client.iter_download(&media.parts[range.index])
                    .chunk_size(CHUNK_SIZE as i32).skip_chunks(chunk_index);
                let source = futures::stream::unfold(Some(download), |download| async move {
                    let mut download = download?;
                    match download.next().await {
                        Ok(Some(bytes)) => Some((Ok(bytes), Some(download))),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), None)),
                    }
                });
                let bounded = logical_stream::bounded_chunks(source, range.start % CHUNK_SIZE, Some(range.length));
                futures::pin_mut!(bounded);
                while let Some(chunk) = bounded.next().await {
                    let failed = chunk.is_err();
                    yield chunk;
                    if failed { return; }
                }
            }
        }
    })
}

#[get("/stream/{folder_id}/{message_id}")]
async fn stream_media(
    req: actix_web::HttpRequest,
    path: web::Path<(String, i32)>,
    query: web::Query<StreamQuery>,
    data: web::Data<Arc<TelegramState>>,
    db: web::Data<crate::db::DbConnection>,
    parts_cache: web::Data<logical_stream::PartCache>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    let (folder_id_str, message_id) = path.into_inner();

    // Validate session token
    match &query.token {
        Some(t) if t == &token_data.token => {
            log::debug!(
                "Stream request: Token validated successfully for msg {}",
                message_id
            );
        }
        _ => {
            log::error!(
                "Stream request failed: Invalid or missing stream token for msg {}",
                message_id
            );
            return HttpResponse::Forbidden().body("Invalid or missing stream token");
        }
    }

    // Parse folder ID
    let folder_id = if folder_id_str == "me" || folder_id_str == "home" || folder_id_str == "null" {
        log::debug!("Stream request: Using root folder for msg {}", message_id);
        None
    } else {
        match folder_id_str.parse::<i64>() {
            Ok(id) => {
                log::debug!(
                    "Stream request: Parsed folder ID {} for msg {}",
                    id,
                    message_id
                );
                Some(id)
            }
            Err(_) => {
                log::error!(
                    "Stream request failed: Invalid folder ID format '{}' for msg {}",
                    folder_id_str,
                    message_id
                );
                return HttpResponse::BadRequest().body("Invalid folder ID");
            }
        }
    };

    let client_opt = { data.client.lock().await.clone() };

    if let Some(client) = client_opt {
        log::debug!(
            "Stream request: Client acquired, resolving peer for msg {}...",
            message_id
        );
        match resolve_peer(&client, folder_id, &data.peer_cache).await {
            Ok(peer) => {
                log::debug!(
                    "Stream request: Peer resolved, fetching message {}...",
                    message_id
                );
                match resolve_stream_media(
                    &client,
                    &peer,
                    db.get_ref(),
                    parts_cache.get_ref(),
                    folder_id,
                    message_id,
                )
                .await
                {
                    Ok(media) => build_logical_stream_response(&client, media, &req),
                    Err(error) => {
                        log::error!("Failed to resolve video {}: {}", message_id, error);
                        HttpResponse::BadGateway().body(error)
                    }
                }
            }
            Err(e) => {
                log::error!(
                    "Stream request failed: Peer resolution error for msg {}: {}",
                    message_id,
                    e
                );
                HttpResponse::BadRequest().body(format!("Peer resolution failed: {}", e))
            }
        }
    } else {
        log::error!(
            "Stream request failed: Telegram client not connected for msg {}",
            message_id
        );
        HttpResponse::ServiceUnavailable().body("Telegram client not connected")
    }
}

fn mime_type_from_media(media: &Media) -> String {
    match media {
        Media::Document(d) => d
            .mime_type()
            .unwrap_or("application/octet-stream")
            .to_string(),
        Media::Photo(_) => "image/jpeg".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// Reserve the exact IPv4 endpoint before any clients receive its configuration.
/// Keep the preferred port when available; another listener must never receive
/// this session's token merely because it occupied our usual port.
pub fn bind_stream_listener(port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            log::warn!(
                "Streaming port {} is occupied; reserving another IPv4 loopback port",
                port
            );
            TcpListener::bind(("127.0.0.1", 0))
        }
        Err(error) => Err(error),
    }
}

pub async fn start_server(
    state: Arc<TelegramState>,
    listener: TcpListener,
    token: String,
    db_pool: crate::db::DbConnection,
    transcode_manager: Arc<TranscodeManager>,
    fmp4_remux: crate::fmp4_remux::Fmp4RemuxState,
) -> std::io::Result<actix_web::dev::Server> {
    let port = listener.local_addr()?.port();
    let state_data = web::Data::new(state);
    let token_data = web::Data::new(StreamTokenData { token });
    let db_data = web::Data::new(db_pool);
    let parts_cache = web::Data::new(logical_stream::PartCache::default());
    let transcode_data = web::Data::new(transcode_manager);
    let fmp4_data = web::Data::new(fmp4_remux);

    log::info!("Starting Streaming Server on port {}", port);

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_origin_fn(|origin, _req_head| {
                let origin_bytes = origin.as_bytes();
                origin_bytes.starts_with(b"tauri://")
                    || origin_bytes.starts_with(b"http://tauri.localhost")
                    || origin_bytes.starts_with(b"https://tauri.localhost")
                    || origin_bytes.starts_with(b"http://localhost")
                    || origin_bytes.starts_with(b"http://127.0.0.1")
                    || origin_bytes.starts_with(b"https://asset.localhost")
                    || origin_bytes.starts_with(b"http://asset.localhost")
                    || origin_bytes == b"null"
            })
            .allow_any_method()
            .allow_any_header();

        App::new()
            .wrap(cors)
            .app_data(state_data.clone())
            .app_data(token_data.clone())
            .app_data(db_data.clone())
            .app_data(parts_cache.clone())
            .app_data(transcode_data.clone())
            .app_data(fmp4_data.clone())
            .service(stream_media)
            .service(serve_subtitle)
            .configure(crate::share_routes::configure_share_routes)
            .configure(crate::transcode::configure_hls_routes)
            .configure(crate::fmp4_remux::configure_fmp4_routes)
    })
    .listen(listener)?
    .run();

    log::info!("Streaming Server started successfully on port {}", port);

    Ok(server)
}
