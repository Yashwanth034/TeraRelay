use crate::commands::utils::resolve_peer;
use crate::commands::TelegramState;
use crate::vpn_optimizer::NetworkConfig;
use grammers_client::types::Media;
use serde::Serialize;
use std::io::{Cursor, Read};
use std::sync::Arc;
use tauri::State;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveEntry {
    pub filename: String,
    pub size: u64,
    pub compressed_size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractedFile {
    pub temp_path: String,
    pub filename: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ArchiveType {
    Zip,
    Rar,
    SevenZ,
}

fn detect_archive_type(filename: &str) -> ArchiveType {
    let lower = filename.to_lowercase();
    if lower.ends_with(".rar") {
        ArchiveType::Rar
    } else if lower.ends_with(".7z") {
        ArchiveType::SevenZ
    } else {
        ArchiveType::Zip
    }
}

fn rar_preview_unavailable() -> String {
    concat!(
        "RAR in-app preview is temporarily disabled until safe extraction is available. ",
        "You can still download the RAR file and open it with a trusted external archiver."
    )
    .to_string()
}

fn generate_unique_temp_prefix(label: &str) -> String {
    format!(
        "archive_{}_{}_{}",
        label,
        std::process::id(),
        rand::random::<u64>()
    )
}

/// Download a zip, rar, or 7z file from Telegram and return its directory listing.
#[tauri::command]
pub async fn cmd_list_archive_contents(
    message_id: i32,
    folder_id: Option<i64>,
    state: State<'_, TelegramState>,
    net_config: State<'_, Arc<NetworkConfig>>,
) -> Result<Vec<ArchiveEntry>, String> {
    let (client, media, filename, max_bytes) =
        prepare_archive_operation(message_id, folder_id, &state, &net_config).await?;
    let archive_type = detect_archive_type(&filename);

    match archive_type {
        ArchiveType::Zip => list_zip_contents(&client, &media, max_bytes, &filename).await,
        ArchiveType::Rar => Err(rar_preview_unavailable()),
        ArchiveType::SevenZ => list_sevenz_contents(&client, &media, max_bytes, &filename).await,
    }
}

/// Extract a single file from an archive and return its temp path for
/// subsequent upload.
#[tauri::command]
pub async fn cmd_extract_archive_entry(
    message_id: i32,
    folder_id: Option<i64>,
    entry_index: usize,
    state: State<'_, TelegramState>,
    net_config: State<'_, Arc<NetworkConfig>>,
) -> Result<ExtractedFile, String> {
    let (client, media, filename, max_bytes) =
        prepare_archive_operation(message_id, folder_id, &state, &net_config).await?;
    let archive_type = detect_archive_type(&filename);

    match archive_type {
        ArchiveType::Zip => extract_zip_entry(&client, &media, max_bytes, entry_index).await,
        ArchiveType::Rar => Err(rar_preview_unavailable()),
        ArchiveType::SevenZ => extract_sevenz_entry(&client, &media, max_bytes, entry_index).await,
    }
}

// ── Shared preparation ──────────────────────────────────────────────────

async fn prepare_archive_operation(
    message_id: i32,
    folder_id: Option<i64>,
    state: &TelegramState,
    net_config: &Arc<NetworkConfig>,
) -> Result<(grammers_client::Client, Media, String, u64), String> {
    let client_opt = { state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return Err("Telegram client is not connected".to_string()),
    };

    let peer = resolve_peer(&client, folder_id, &state.peer_cache)
        .await
        .map_err(|e| format!("Failed to resolve peer: {}", e))?;

    let messages = client
        .get_messages_by_id(&peer, &[message_id])
        .await
        .map_err(|e| format!("Failed to fetch message: {}", e))?;

    let msg = messages
        .into_iter()
        .flatten()
        .next()
        .ok_or("File not found")?;

    let media = msg.media().ok_or("Message has no media")?;

    let filename = match &media {
        Media::Document(d) => d.name().to_string(),
        _ => "unknown".to_string(),
    };

    let file_size = match &media {
        Media::Document(d) => d.size() as u64,
        _ => 0,
    };

    let max_bytes = net_config.archive_max_bytes();
    if max_bytes > 0 && file_size > max_bytes {
        return Err(format!(
            "Archive file ({} MiB) exceeds the {} MiB archive size limit",
            file_size / (1024 * 1024),
            max_bytes / (1024 * 1024),
        ));
    }

    Ok((client, media, filename, max_bytes))
}

// ── ZIP helpers ─────────────────────────────────────────────────────────

async fn download_to_memory(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut download_iter = client.iter_download(media);
    let mut total_bytes: u64 = 0;

    while let Some(chunk) = archive_download_chunk(download_iter.next().await, label)? {
        total_bytes += chunk.len() as u64;
        if max_bytes > 0 && total_bytes > max_bytes {
            return Err(format!(
                "{} download exceeded the {} MiB limit",
                label,
                max_bytes / (1024 * 1024),
            ));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn list_zip_contents(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    filename: &str,
) -> Result<Vec<ArchiveEntry>, String> {
    let data = download_to_memory(client, media, max_bytes, "ZIP").await?;
    let cursor = Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Failed to parse ZIP file: {}", e))?;

    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read ZIP entry at index {}: {}", i, e))?;
        entries.push(ArchiveEntry {
            filename: file
                .name()
                .map_err(|error| format!("Invalid ZIP entry name: {error}"))?
                .into_owned(),
            size: file.size(),
            compressed_size: file.compressed_size(),
            is_dir: file.is_dir(),
        });
    }
    check_non_empty(&entries, filename, "ZIP")?;
    Ok(entries)
}

async fn extract_zip_entry(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    entry_index: usize,
) -> Result<ExtractedFile, String> {
    let data = download_to_memory(client, media, max_bytes, "ZIP").await?;

    let (buf, safe_name, entry_size, temp_path) = {
        let cursor = Cursor::new(data);
        let mut archive =
            zip::ZipArchive::new(cursor).map_err(|e| format!("Failed to parse ZIP file: {}", e))?;
        let mut file = archive
            .by_index(entry_index)
            .map_err(|e| format!("Failed to read ZIP entry at index {}: {}", entry_index, e))?;
        if file.is_dir() {
            return Err("Cannot extract a directory entry".to_string());
        }
        let entry_name = file
            .name()
            .map_err(|error| format!("Invalid ZIP entry name: {error}"))?
            .into_owned();
        let entry_size = file.size();
        let safe_name = sanitise_entry_name(&entry_name, entry_index);
        let temp_path = std::env::temp_dir().join(format!(
            "{}_{}",
            generate_unique_temp_prefix("extract"),
            safe_name
        ));
        // A small compressed ZIP entry can claim an enormous uncompressed size.
        // Never reserve that claimed capacity or decode past the preview budget.
        let buf = read_bounded_archive_entry(&mut file, entry_size)?;
        Ok::<_, String>((buf, safe_name, entry_size, temp_path))
    }?;

    tokio::fs::write(&temp_path, &buf)
        .await
        .map_err(|e| format!("Failed to write extracted file: {}", e))?;
    if let Err(error) = super::fs::register_temporary_output(&temp_path) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error);
    }

    Ok(ExtractedFile {
        temp_path: temp_path.to_string_lossy().to_string(),
        filename: safe_name,
        size: entry_size,
    })
}

// ── RAR helpers ─────────────────────────────────────────────────────────

async fn download_to_temp_file(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    label: &str,
    extension: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let unique_id = generate_unique_temp_prefix("viewer");
    let archive_path = std::env::temp_dir().join(format!("{}.{}", unique_id, extension));
    let extract_dir = std::env::temp_dir().join(format!("{}_extract", unique_id));

    tokio::fs::create_dir_all(&extract_dir)
        .await
        .map_err(|e| format!("Failed to create temp extract directory: {}", e))?;

    {
        let mut file = tokio::fs::File::create(&archive_path)
            .await
            .map_err(|e| format!("Failed to create temp file: {}", e))?;
        let mut download_iter = client.iter_download(media);
        let mut total_bytes: u64 = 0;

        loop {
            let next = match archive_download_chunk(download_iter.next().await, label) {
                Ok(next) => next,
                Err(error) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&archive_path).await;
                    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
                    return Err(error);
                }
            };
            let Some(chunk) = next else { break };
            total_bytes += chunk.len() as u64;
            if max_bytes > 0 && total_bytes > max_bytes {
                let _ = tokio::fs::remove_file(&archive_path).await;
                let _ = tokio::fs::remove_dir_all(&extract_dir).await;
                return Err(format!(
                    "{} download exceeded the {} MiB limit",
                    label,
                    max_bytes / (1024 * 1024),
                ));
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Failed to write temp file: {}", e))?;
        }
        file.flush()
            .await
            .map_err(|e| format!("Failed to flush temp file: {}", e))?;
    }

    Ok((archive_path, extract_dir))
}

// RAR in-app previews intentionally fail closed until a safe, licensed,
// single-member decoder is available. Stored RAR downloads remain supported.

// ── 7z helpers ──────────────────────────────────────────────────────────

async fn list_sevenz_contents(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    filename: &str,
) -> Result<Vec<ArchiveEntry>, String> {
    let (archive_path, extract_dir) =
        download_to_temp_file(client, media, max_bytes, "7z", "7z").await?;
    let path = archive_path.clone();

    let entries_result: Result<Vec<ArchiveEntry>, String> =
        tokio::task::spawn_blocking(move || {
            let archive = sevenz_rust2::Archive::open(&path)
                .map_err(|e| format!("Failed to open 7z file: {}", e))?;
            let entries = archive
                .files
                .iter()
                .map(|e| ArchiveEntry {
                    filename: e.name().to_string(),
                    size: e.size,
                    compressed_size: e.compressed_size,
                    is_dir: e.is_directory,
                })
                .collect::<Vec<_>>();
            drop(archive);
            Ok(entries)
        })
        .await
        .map_err(|e| format!("7z listing task panicked: {:?}", e))?;

    let _ = tokio::fs::remove_file(&archive_path).await;
    // 7z listing doesn't extract, so extract_dir is empty — clean it up.
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;

    let entries = entries_result?;
    check_non_empty(&entries, filename, "7z")?;
    Ok(entries)
}

async fn extract_sevenz_entry(
    client: &grammers_client::Client,
    media: &Media,
    max_bytes: u64,
    entry_index: usize,
) -> Result<ExtractedFile, String> {
    let (archive_path, extract_dir) =
        download_to_temp_file(client, media, max_bytes, "7z", "7z").await?;
    let path = archive_path.clone();

    let extraction_result: Result<ExtractedFile, String> = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .map_err(|e| format!("Failed to open temp 7z file: {}", e))?;
        let len = file
            .metadata()
            .map_err(|e| format!("Failed to read 7z file metadata: {}", e))?
            .len();
        let mut reader = sevenz_rust2::SevenZReader::new(file, len, [].as_slice().into())
            .map_err(|e| format!("Failed to create 7z reader: {}", e))?;

        let mut found: Option<(String, u64, Vec<u8>)> = None;
        let mut idx: usize = 0;

        reader
            .for_each_entries(|entry, entry_reader| {
                let current = idx;
                idx += 1;
                if current != entry_index {
                    return Ok(true); // continue
                }
                if entry.is_directory {
                    return Err(sevenz_rust2::Error::other(
                        "Cannot extract a directory entry",
                    ));
                }
                let safe_name = sanitise_entry_name(entry.name(), entry_index);
                let buf = read_bounded_archive_entry(entry_reader, entry.size)
                    .map_err(sevenz_rust2::Error::other)?;
                found = Some((safe_name, entry.size, buf));
                Ok(false) // stop iteration
            })
            .map_err(|e| format!("7z extraction error: {}", e))?;

        let (safe_name, size, buf) =
            found.ok_or_else(|| format!("Entry index {} not found in 7z archive", entry_index))?;

        let temp_path = std::env::temp_dir().join(format!(
            "{}_{}",
            generate_unique_temp_prefix("extract"),
            safe_name
        ));
        std::fs::write(&temp_path, &buf)
            .map_err(|e| format!("Failed to write extracted 7z entry: {}", e))?;
        if let Err(error) = super::fs::register_temporary_output(&temp_path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }

        Ok(ExtractedFile {
            temp_path: temp_path.to_string_lossy().to_string(),
            filename: safe_name,
            size,
        })
    })
    .await
    .map_err(|e| format!("7z extraction task panicked: {:?}", e))?;

    let _ = tokio::fs::remove_file(&archive_path).await;
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;

    extraction_result
}

// ── Shared utilities ────────────────────────────────────────────────────

// Do not interpret a Telegram network/transport error as a clean EOF.
// Treating Err as None previously returned incomplete archive bytes as if
// the remote download had completed successfully.
fn archive_download_chunk<T, E: std::fmt::Display>(
    result: Result<Option<T>, E>,
    label: &str,
) -> Result<Option<T>, String> {
    result.map_err(|error| format!("{label} download failed: {error}"))
}

// ZIP and 7z previews load one decompressed entry into memory. Keep the
// decoded output bounded even when the configured archive-download limit is
// unlimited (or the entry's advertised uncompressed size is malicious).
const MAX_ARCHIVE_ENTRY_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;

fn read_bounded_archive_entry<R: Read + ?Sized>(
    reader: &mut R,
    declared_size: u64,
) -> Result<Vec<u8>, String> {
    read_bounded_archive_entry_with_limit(reader, declared_size, MAX_ARCHIVE_ENTRY_PREVIEW_BYTES)
}

fn read_bounded_archive_entry_with_limit<R: Read + ?Sized>(
    reader: &mut R,
    declared_size: u64,
    limit: u64,
) -> Result<Vec<u8>, String> {
    if declared_size > limit {
        return Err(format!(
            "Archive entry exceeds the {} MiB preview limit",
            limit / (1024 * 1024)
        ));
    }
    let read_limit = declared_size
        .checked_add(1)
        .ok_or_else(|| "Archive entry size overflow".to_string())?;
    let mut bytes = Vec::new();
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read archive entry bytes: {e}"))?;
    if bytes.len() as u64 != declared_size {
        return Err(
            "Archive entry's decompressed length does not match its declared size".to_string(),
        );
    }
    Ok(bytes)
}

fn sanitise_entry_name(entry_name: &str, entry_index: usize) -> String {
    std::path::Path::new(entry_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("extracted_{}", entry_index))
}

fn check_non_empty(entries: &[ArchiveEntry], filename: &str, label: &str) -> Result<(), String> {
    if entries.is_empty() {
        return Err(format!(
            "The file \"{}\" does not appear to be a valid {} archive (no entries found)",
            filename, label,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod bounded_archive_tests {
    use super::{
        archive_download_chunk, detect_archive_type, rar_preview_unavailable,
        read_bounded_archive_entry, read_bounded_archive_entry_with_limit, sanitise_entry_name,
        ArchiveType,
    };
    use std::io::{Cursor, Read, Write};

    #[test]
    fn zip_nine_deflate_archive_lists_and_extracts_one_bounded_entry() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("nested/hello.txt", options).unwrap();
        writer.write_all(b"TeraRelay ZIP compatibility").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        let entry_name = entry.name().unwrap().into_owned();
        assert_eq!(sanitise_entry_name(&entry_name, 0), "hello.txt");
        let size = entry.size();
        let extracted = read_bounded_archive_entry(&mut entry, size).unwrap();
        assert_eq!(extracted, b"TeraRelay ZIP compatibility");
    }

    #[test]
    fn unsafe_rar_preview_fails_closed_without_disabling_zip_or_sevenz() {
        assert_eq!(detect_archive_type("anything.RAR"), ArchiveType::Rar);
        assert_eq!(detect_archive_type("anything.zip"), ArchiveType::Zip);
        assert_eq!(detect_archive_type("anything.7z"), ArchiveType::SevenZ);
        let message = rar_preview_unavailable();
        assert!(message.contains("temporarily disabled"));
        assert!(message.contains("download the RAR"));
    }

    #[test]
    fn download_transport_failure_is_not_silently_treated_as_end_of_file() {
        let failure = archive_download_chunk::<Vec<u8>, std::io::Error>(
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection lost",
            )),
            "RAR",
        )
        .unwrap_err();
        assert!(failure.contains("RAR download failed"));
        assert!(failure.contains("connection lost"));

        assert_eq!(
            archive_download_chunk::<Vec<u8>, std::io::Error>(Ok(None), "ZIP").unwrap(),
            None
        );
        assert_eq!(
            archive_download_chunk::<Vec<u8>, std::io::Error>(Ok(Some(b"ok".to_vec())), "ZIP")
                .unwrap(),
            Some(b"ok".to_vec())
        );
    }

    #[test]
    fn accepts_exact_bounded_data_and_empty_entries() {
        assert_eq!(
            read_bounded_archive_entry_with_limit(&mut Cursor::new(b"hello"), 5, 8).unwrap(),
            b"hello"
        );
        assert_eq!(
            read_bounded_archive_entry_with_limit(&mut Cursor::new(b""), 0, 8).unwrap(),
            b""
        );
    }

    #[test]
    fn rejects_oversized_claim_before_reading_any_bytes() {
        struct MustNotRead;
        impl Read for MustNotRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("oversized archive entry must be rejected before decompression")
            }
        }
        assert!(
            read_bounded_archive_entry_with_limit(&mut MustNotRead, 9, 8)
                .unwrap_err()
                .contains("preview limit")
        );
    }

    #[test]
    fn rejects_overlong_or_truncated_decompressed_entries() {
        assert!(
            read_bounded_archive_entry_with_limit(&mut Cursor::new(b"123456789"), 8, 8)
                .unwrap_err()
                .contains("decompressed length")
        );
        assert!(
            read_bounded_archive_entry_with_limit(&mut Cursor::new(b"123"), 8, 8)
                .unwrap_err()
                .contains("decompressed length")
        );
    }
}
