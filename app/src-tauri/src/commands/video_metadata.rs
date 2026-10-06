use crate::commands::streaming::StreamConfig;
use crate::commands::utils::resolve_peer;
use crate::mp4_utils;
use crate::transcode::TranscodeManager;
use crate::TelegramState;
use grammers_client::types::Media;
use std::sync::Arc;
use tauri::State;

#[derive(serde::Serialize)]
pub struct VideoMetadata {
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    pub has_audio: bool,
    pub track_count: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(serde::Serialize, Clone)]
pub struct MediaTrackInfo {
    pub index: i32,
    pub kind: String,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
}

#[derive(serde::Serialize)]
pub struct MediaTrackProbe {
    pub audio_tracks: Vec<MediaTrackInfo>,
    pub subtitle_tracks: Vec<MediaTrackInfo>,
    pub duration_secs: Option<f64>,
    pub start_time_secs: Option<f64>,
}

#[derive(serde::Deserialize)]
pub struct BatchMetadataRequest {
    pub message_id: i32,
    pub file_name: String,
}

#[derive(serde::Serialize)]
pub struct BatchMetadataEntry {
    pub message_id: i32,
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[tauri::command]
pub async fn cmd_get_video_metadata(
    message_id: i32,
    folder_id: Option<i64>,
    state: State<'_, TelegramState>,
) -> Result<VideoMetadata, String> {
    let client = { state.client.lock().await.clone() };
    let client = client.ok_or_else(|| "Not connected to Telegram".to_string())?;

    let buffer = download_moov_chunk(&client, message_id, folder_id, &state).await?;
    let meta = parse_mp4_metadata(&buffer)?;
    let (width, height) = mp4_utils::scan_video_tkhd_dimensions(&buffer);

    Ok(VideoMetadata {
        duration_secs: meta.duration_secs,
        video_codec: meta.video_codec,
        has_audio: meta.has_audio,
        track_count: meta.track_count,
        width,
        height,
    })
}

#[tauri::command]
pub async fn cmd_probe_media_tracks(
    message_id: i32,
    folder_id: Option<i64>,
    config: State<'_, StreamConfig>,
) -> Result<MediaTrackProbe, String> {
    let folder_segment = folder_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "home".to_string());
    let stream_url = format!(
        "http://localhost:{}/stream/{}/{}?token={}",
        config.port,
        folder_segment,
        message_id,
        urlencoding::encode(&config.token)
    );

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration,start_time:stream=index,codec_type,codec_name,channels:stream_tags=language,title")
            .arg("-of")
            .arg("json")
            .arg(&stream_url)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "FFprobe timed out while reading media tracks".to_string())?
    .map_err(|e| format!("FFprobe is unavailable: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("FFprobe failed: {}", stderr.trim()));
    }

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid FFprobe JSON: {e}"))?;

    Ok(parse_media_track_probe(parsed))
}

fn finite_probe_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

fn parse_media_track_probe(parsed: serde_json::Value) -> MediaTrackProbe {
    let mut audio_tracks = Vec::new();
    let mut subtitle_tracks = Vec::new();

    for stream in parsed
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(index) = stream.get("index").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let kind = stream
            .get("codec_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if kind != "audio" && kind != "subtitle" {
            continue;
        }

        let tags = stream.get("tags").and_then(serde_json::Value::as_object);
        let track = MediaTrackInfo {
            index: index as i32,
            kind: kind.clone(),
            codec: stream
                .get("codec_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            language: tags
                .and_then(|value| value.get("language"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            title: tags
                .and_then(|value| value.get("title"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            channels: stream
                .get("channels")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as u32),
        };

        if kind == "audio" {
            audio_tracks.push(track);
        } else {
            subtitle_tracks.push(track);
        }
    }

    MediaTrackProbe {
        audio_tracks,
        subtitle_tracks,
        duration_secs: finite_probe_number(&parsed["format"]["duration"]).filter(|n| *n > 0.0),
        start_time_secs: finite_probe_number(&parsed["format"]["start_time"]),
    }
}

#[tauri::command]
pub async fn cmd_prepare_subtitle_track(
    message_id: i32,
    folder_id: Option<i64>,
    stream_index: i32,
    config: State<'_, StreamConfig>,
    manager: State<'_, Arc<TranscodeManager>>,
) -> Result<String, String> {
    if stream_index < 0 {
        return Err("Invalid subtitle stream index".to_string());
    }

    let folder_value = folder_id.unwrap_or(0);
    let folder_segment = folder_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "home".to_string());
    let file_key = format!("{}_{}", folder_value, message_id);
    let output_dir = manager.cache_root.join("subtitles").join(&file_key);
    let output_path = output_dir.join(format!("{stream_index}.vtt"));

    if output_path.exists()
        && std::fs::metadata(&output_path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    {
        return Ok(format!("/subtitle/{file_key}/{stream_index}.vtt"));
    }

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create subtitle cache: {e}"))?;

    let stream_url = format!(
        "http://localhost:{}/stream/{}/{}?token={}",
        config.port,
        folder_segment,
        message_id,
        urlencoding::encode(&config.token)
    );

    let ffmpeg_path = manager
        .ffmpeg_path
        .lock()
        .await
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        tokio::process::Command::new(ffmpeg_path)
            .arg("-y")
            .arg("-v")
            .arg("error")
            .arg("-i")
            .arg(&stream_url)
            .arg("-map")
            .arg(format!("0:{stream_index}"))
            .arg("-vn")
            .arg("-an")
            .arg("-c:s")
            .arg("webvtt")
            .arg(&output_path)
            .output(),
    )
    .await
    .map_err(|_| "Subtitle extraction timed out".to_string())?
    .map_err(|e| format!("Failed to launch FFmpeg for subtitles: {e}"))?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&output_path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Subtitle conversion failed: {}", stderr.trim()));
    }

    if !output_path.exists()
        || std::fs::metadata(&output_path)
            .map(|meta| meta.len() == 0)
            .unwrap_or(true)
    {
        let _ = std::fs::remove_file(&output_path);
        return Err("Subtitle conversion produced no output".to_string());
    }

    Ok(format!("/subtitle/{file_key}/{stream_index}.vtt"))
}

#[tauri::command]
pub async fn cmd_get_video_metadata_batch(
    requests: Vec<BatchMetadataRequest>,
    folder_id: Option<i64>,
    state: State<'_, TelegramState>,
) -> Result<Vec<BatchMetadataEntry>, String> {
    let client = { state.client.lock().await.clone() };
    let client = client.ok_or_else(|| "Not connected to Telegram".to_string())?;
    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;

    let mut results: Vec<BatchMetadataEntry> = Vec::with_capacity(requests.len());

    for req in &requests {
        if !req.file_name.to_lowercase().ends_with(".mp4") {
            continue;
        }
        match download_and_process(&client, &peer, req).await {
            Ok(e) => results.push(e),
            Err(_) => results.push(BatchMetadataEntry {
                message_id: req.message_id,
                duration_secs: None,
                width: None,
                height: None,
            }),
        }
    }

    Ok(results)
}

// ── Internal helpers ─────────────────────────────────────────────────

struct ParsedMetadata {
    duration_secs: Option<f64>,
    video_codec: Option<String>,
    has_audio: bool,
    track_count: usize,
}

/// Download the first 2 MB of a file and parse metadata + scan tkhd.
async fn download_and_process(
    client: &grammers_client::Client,
    peer: &grammers_client::types::Peer,
    req: &BatchMetadataRequest,
) -> Result<BatchMetadataEntry, String> {
    let messages = client
        .get_messages_by_id(peer, &[req.message_id])
        .await
        .map_err(|e| e.to_string())?;
    let msg = messages
        .into_iter()
        .flatten()
        .next()
        .ok_or_else(|| format!("Message {} not found", req.message_id))?;
    let media = msg.media().ok_or_else(|| "No media".to_string())?;

    let size = match &media {
        Media::Document(d) => d.size() as u64,
        _ => return Err("Not a document".to_string()),
    };

    let buffer = download_bytes(client, &media, size).await?;
    let meta = parse_mp4_metadata(&buffer)?;
    let (width, height) = mp4_utils::scan_video_tkhd_dimensions(&buffer);

    Ok(BatchMetadataEntry {
        message_id: req.message_id,
        duration_secs: meta.duration_secs,
        width,
        height,
    })
}

async fn download_moov_chunk(
    client: &grammers_client::Client,
    message_id: i32,
    folder_id: Option<i64>,
    state: &TelegramState,
) -> Result<Vec<u8>, String> {
    let peer = resolve_peer(client, folder_id, &state.peer_cache).await?;
    let messages = client
        .get_messages_by_id(&peer, &[message_id])
        .await
        .map_err(|e| e.to_string())?;
    let msg = messages
        .into_iter()
        .flatten()
        .next()
        .ok_or_else(|| format!("Message {message_id} not found"))?;
    let media = msg.media().ok_or_else(|| "No media".to_string())?;
    let size = match &media {
        Media::Document(d) => d.size() as u64,
        _ => return Err("Not a document".to_string()),
    };
    download_bytes(client, &media, size).await
}

/// Download at most the first 2 MB from a Telegram document.
async fn download_bytes(
    client: &grammers_client::Client,
    media: &Media,
    file_size: u64,
) -> Result<Vec<u8>, String> {
    let max_bytes = std::cmp::min(2 * 1024 * 1024, file_size) as usize;
    let mut buffer: Vec<u8> = Vec::with_capacity(max_bytes);
    let mut download_iter = client.iter_download(media);
    download_iter = download_iter.chunk_size(65536);

    while buffer.len() < max_bytes {
        match download_iter.next().await {
            Ok(Some(chunk)) => {
                let remaining = max_bytes.saturating_sub(buffer.len());
                let take = std::cmp::min(chunk.len(), remaining);
                buffer.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("Download error: {e}")),
        }
    }
    if buffer.is_empty() {
        return Err("Downloaded zero bytes".to_string());
    }
    Ok(buffer)
}

fn parse_mp4_metadata(buffer: &[u8]) -> Result<ParsedMetadata, String> {
    let mut cursor = std::io::Cursor::new(buffer);
    let context = mp4parse::read_mp4(&mut cursor).map_err(|e| format!("MP4 parse error: {e}"))?;

    let video_track = context
        .tracks
        .iter()
        .find(|t| t.track_type == mp4parse::TrackType::Video);

    let has_audio = context
        .tracks
        .iter()
        .any(|t| t.track_type == mp4parse::TrackType::Audio);

    let duration_secs = video_track.and_then(|t| {
        let d = t.duration.as_ref()?;
        let ts = t.timescale.as_ref()?;
        Some((d.0 as f64) / (ts.0 as f64))
    });

    Ok(ParsedMetadata {
        duration_secs,
        video_codec: None,
        has_audio,
        track_count: context.tracks.len(),
    })
}

#[cfg(test)]
mod track_probe_tests {
    use super::*;
    #[test]
    fn track_probe_keeps_complete_source_duration_and_selected_track_metadata() {
        let probe = parse_media_track_probe(serde_json::json!({
            "format":{"duration":"7200.064","start_time":"-0.064"},
            "streams":[{"index":2,"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":"tel"}},
                       {"index":3,"codec_type":"subtitle","codec_name":"subrip"}]
        }));
        assert_eq!(probe.duration_secs, Some(7200.064));
        assert_eq!(probe.start_time_secs, Some(-0.064));
        assert_eq!(probe.audio_tracks[0].index, 2);
        assert_eq!(probe.audio_tracks[0].language.as_deref(), Some("tel"));
        assert_eq!(probe.subtitle_tracks[0].index, 3);
    }
    #[test]
    fn unknown_or_nonfinite_duration_does_not_create_a_false_movie_length() {
        for duration in ["N/A", "NaN", "inf", "-1", "0"] {
            let probe =
                parse_media_track_probe(serde_json::json!({"format":{"duration":duration}}));
            assert_eq!(probe.duration_secs, None);
        }
    }
}
