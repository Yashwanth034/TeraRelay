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

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct MediaTrackInfo {
    pub index: i32,
    pub kind: String,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
}

#[derive(serde::Serialize)]
pub struct MediaTrackProbe {
    pub audio_tracks: Vec<MediaTrackInfo>,
    pub subtitle_tracks: Vec<MediaTrackInfo>,
    pub duration_secs: Option<f64>,
    pub start_time_secs: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RichMediaMetadata {
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub pixel_format: Option<String>,
    pub dynamic_range: Option<String>,
    pub container: Option<String>,
    pub audio_tracks: Vec<MediaTrackInfo>,
    pub subtitle_tracks: Vec<MediaTrackInfo>,
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
            .arg("format=duration,start_time:stream=index,codec_type,codec_name,channels,channel_layout:stream_tags=language,title")
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

fn qa_rich_media_metadata(file_name: &str) -> RichMediaMetadata {
    let lower = file_name.to_ascii_lowercase();
    let is_dune = lower.contains("dune");
    let is_4k = lower.contains("2160p") || lower.contains("4k");
    let is_hdr = lower.contains("hdr");
    let duration_secs = if is_dune { 9_960.0 } else { 10_140.0 };

    RichMediaMetadata {
        duration_secs: Some(duration_secs),
        width: Some(if is_4k { 3840 } else { 1920 }),
        height: Some(if is_4k { 2160 } else { 1080 }),
        video_codec: Some(if is_4k {
            "hevc".to_string()
        } else {
            "h264".to_string()
        }),
        video_profile: None,
        pixel_format: Some(if is_4k {
            "yuv420p10le".to_string()
        } else {
            "yuv420p".to_string()
        }),
        dynamic_range: is_hdr.then(|| "HDR10".to_string()),
        container: Some("MKV".to_string()),
        audio_tracks: vec![MediaTrackInfo {
            index: 1,
            kind: "audio".to_string(),
            codec: "aac".to_string(),
            language: Some("eng".to_string()),
            title: None,
            channels: Some(if is_4k { 6 } else { 2 }),
            channel_layout: Some(if is_4k {
                "5.1".to_string()
            } else {
                "stereo".to_string()
            }),
        }],
        subtitle_tracks: if is_4k {
            (0..3)
                .map(|offset| MediaTrackInfo {
                    index: 2 + offset,
                    kind: "subtitle".to_string(),
                    codec: "subrip".to_string(),
                    language: Some("eng".to_string()),
                    title: None,
                    channels: None,
                    channel_layout: None,
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

fn container_from_file_name(file_name: &str) -> Option<String> {
    let extension = std::path::Path::new(file_name)
        .extension()?
        .to_str()?
        .to_ascii_uppercase();
    (!extension.is_empty()).then_some(extension)
}

fn parse_rich_media_metadata(parsed: serde_json::Value, file_name: &str) -> RichMediaMetadata {
    let tracks = parse_media_track_probe(parsed.clone());
    let video = parsed
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .and_then(|streams| {
            streams.iter().find(|stream| {
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
            })
        });

    let width = video
        .and_then(|stream| stream.get("width"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as u32);
    let height = video
        .and_then(|stream| stream.get("height"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as u32);
    let video_codec = video
        .and_then(|stream| stream.get("codec_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let video_profile = video
        .and_then(|stream| stream.get("profile"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let pixel_format = video
        .and_then(|stream| stream.get("pix_fmt"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let transfer = video
        .and_then(|stream| stream.get("color_transfer"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let primaries = video
        .and_then(|stream| stream.get("color_primaries"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let dynamic_range = match transfer {
        "smpte2084" if primaries == "bt2020" => Some("HDR10".to_string()),
        "smpte2084" => Some("HDR (PQ)".to_string()),
        "arib-std-b67" => Some("HLG".to_string()),
        _ => None,
    };

    let container = container_from_file_name(file_name).or_else(|| {
        parsed
            .get("format")
            .and_then(|format| format.get("format_name"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.split(',').next())
            .map(|value| value.to_ascii_uppercase())
    });

    RichMediaMetadata {
        duration_secs: tracks.duration_secs,
        width,
        height,
        video_codec,
        video_profile,
        pixel_format,
        dynamic_range,
        container,
        audio_tracks: tracks.audio_tracks,
        subtitle_tracks: tracks.subtitle_tracks,
    }
}

#[tauri::command]
pub async fn cmd_get_rich_media_metadata(
    message_id: i32,
    folder_id: Option<i64>,
    file_name: String,
    config: State<'_, StreamConfig>,
) -> Result<RichMediaMetadata, String> {
    if crate::commands::qa_feature_a::enabled() {
        return Ok(qa_rich_media_metadata(&file_name));
    }

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
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=format_name,duration,start_time:stream=index,codec_type,codec_name,profile,width,height,pix_fmt,color_transfer,color_primaries,color_space,channels,channel_layout:stream_tags=language,title")
            .arg("-of")
            .arg("json")
            .arg(&stream_url)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "FFprobe timed out while reading media details".to_string())?
    .map_err(|e| format!("FFprobe is unavailable: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("FFprobe failed: {}", stderr.trim()));
    }

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid FFprobe JSON: {e}"))?;

    Ok(parse_rich_media_metadata(parsed, &file_name))
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
            channel_layout: stream
                .get("channel_layout")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
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

    #[test]
    fn rich_probe_extracts_video_hdr_audio_and_subtitle_details() {
        let metadata = parse_rich_media_metadata(
            serde_json::json!({
                "format":{"duration":"10140.0","format_name":"matroska,webm"},
                "streams":[
                    {
                        "index":0,
                        "codec_type":"video",
                        "codec_name":"hevc",
                        "profile":"Main 10",
                        "width":3840,
                        "height":2160,
                        "pix_fmt":"yuv420p10le",
                        "color_transfer":"smpte2084",
                        "color_primaries":"bt2020"
                    },
                    {
                        "index":1,
                        "codec_type":"audio",
                        "codec_name":"aac",
                        "channels":6,
                        "channel_layout":"5.1",
                        "tags":{"language":"eng"}
                    },
                    {
                        "index":2,
                        "codec_type":"subtitle",
                        "codec_name":"subrip",
                        "tags":{"language":"eng"}
                    }
                ]
            }),
            "Interstellar.2160p.HDR.mkv",
        );

        assert_eq!(metadata.duration_secs, Some(10_140.0));
        assert_eq!(metadata.width, Some(3840));
        assert_eq!(metadata.height, Some(2160));
        assert_eq!(metadata.video_codec.as_deref(), Some("hevc"));
        assert_eq!(metadata.dynamic_range.as_deref(), Some("HDR10"));
        assert_eq!(metadata.container.as_deref(), Some("MKV"));
        assert_eq!(metadata.audio_tracks.len(), 1);
        assert_eq!(
            metadata.audio_tracks[0].channel_layout.as_deref(),
            Some("5.1")
        );
        assert_eq!(metadata.subtitle_tracks.len(), 1);
    }
}
