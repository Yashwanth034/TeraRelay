use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{Emitter, Manager, State};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteImportSourceKind {
    WebMedia,
    DirectFile,
    Torrent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportQuality {
    pub format_id: String,
    pub label: String,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub hdr: bool,
    pub ext: Option<String>,
    pub estimated_size: Option<u64>,
    pub recommended: bool,
    pub has_audio: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportItem {
    pub id: String,
    pub label: String,
    pub path: Option<String>,
    pub size: Option<u64>,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportAnalysis {
    pub source_kind: RemoteImportSourceKind,
    pub title: String,
    pub source_label: String,
    pub duration_seconds: Option<f64>,
    pub thumbnail: Option<String>,
    pub qualities: Vec<RemoteImportQuality>,
    pub items: Vec<RemoteImportItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteImportEngineStatus {
    pub name: String,
    pub available: bool,
    pub version: Option<String>,
    pub required: bool,
    pub purpose: String,
}

const DIRECT_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "avi", "m4v", "mp3", "m4a", "aac", "flac", "wav", "ogg", "opus",
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "heic", "pdf", "zip", "7z", "rar", "tar", "gz",
    "tgz", "iso", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub",
];

const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "avi", "m4v", "ts", "m2ts", "mp3", "m4a", "aac", "flac", "wav",
    "ogg", "opus",
];

const REMOTE_IMPORT_DISK_HEADROOM: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct HttpResourceMetadata {
    direct: bool,
    size: Option<u64>,
    filename: Option<String>,
}

pub fn classify_source(raw: &str) -> Result<RemoteImportSourceKind, String> {
    let source = raw.trim();
    if source.is_empty() {
        return Err("Paste a URL, magnet link, or torrent source".to_string());
    }
    if source.starts_with("magnet:?") {
        if !source.contains("xt=urn:btih:") && !source.contains("xt=urn:btmh:") {
            return Err("Magnet link is missing a BitTorrent info hash".to_string());
        }
        return Ok(RemoteImportSourceKind::Torrent);
    }

    let url = reqwest::Url::parse(source).map_err(|_| {
        "Remote Import accepts only HTTP(S) URLs, magnet links, or local .torrent files".to_string()
    })?;
    if url.scheme() == "file" {
        let extension = std::path::Path::new(url.path())
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return if extension == "torrent" {
            Ok(RemoteImportSourceKind::Torrent)
        } else {
            Err(
                "Remote Import accepts local files only when they are .torrent metadata"
                    .to_string(),
            )
        };
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(
            "Remote Import accepts only HTTP(S) URLs, magnet links, or local .torrent files"
                .to_string(),
        );
    }
    let extension = std::path::Path::new(url.path())
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "torrent" {
        return Ok(RemoteImportSourceKind::Torrent);
    }
    if DIRECT_EXTENSIONS.contains(&extension.as_str()) {
        return Ok(RemoteImportSourceKind::DirectFile);
    }
    Ok(RemoteImportSourceKind::WebMedia)
}

fn http_headers_indicate_direct_resource(
    content_type: Option<&str>,
    content_disposition: Option<&str>,
) -> bool {
    if content_disposition
        .map(|value| value.to_ascii_lowercase().contains("attachment"))
        .unwrap_or(false)
    {
        return true;
    }
    let Some(content_type) = content_type else {
        return false;
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.starts_with("video/") || mime.starts_with("audio/") || mime.starts_with("image/") {
        return true;
    }
    if !mime.starts_with("application/") {
        return false;
    }
    !mime.contains("json")
        && !mime.contains("xml")
        && !mime.contains("javascript")
        && mime != "application/x-www-form-urlencoded"
}

fn content_disposition_filename(raw: &str) -> Option<String> {
    raw.split(';').map(str::trim).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("filename") {
            return None;
        }
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            None
        } else {
            Some(sanitize_import_filename(value))
        }
    })
}

fn content_range_total(raw: &str) -> Option<u64> {
    raw.rsplit_once('/')
        .and_then(|(_, total)| total.parse::<u64>().ok())
}

async fn probe_http_resource(
    source: &str,
    proxy: Option<&str>,
) -> Result<Option<HttpResourceMetadata>, String> {
    let url = match reqwest::Url::parse(source) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        _ => return Ok(None),
    };
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(proxy) = proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| "Remote Import proxy configuration is invalid".to_string())?,
        );
    }
    let client = builder.build().map_err(|error| error.to_string())?;
    let head = client.head(url.clone()).send().await.ok();
    let response = match head {
        Some(response) if response.status().is_success() => Some(response),
        Some(response)
            if matches!(
                response.status(),
                reqwest::StatusCode::METHOD_NOT_ALLOWED | reqwest::StatusCode::NOT_IMPLEMENTED
            ) =>
        {
            client
                .get(url.clone())
                .header(reqwest::header::RANGE, "bytes=0-0")
                .send()
                .await
                .ok()
        }
        _ => None,
    };
    let Some(response) = response else {
        return Ok(None);
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let content_disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok());
    let size = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_total)
        .or_else(|| response.content_length());
    let filename = content_disposition
        .and_then(content_disposition_filename)
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|value| !value.is_empty())
                .map(sanitize_import_filename)
        });
    Ok(Some(HttpResourceMetadata {
        direct: http_headers_indicate_direct_resource(content_type, content_disposition),
        size,
        filename,
    }))
}

async fn resolve_source_kind(
    source: &str,
    proxy: Option<&str>,
) -> Result<(RemoteImportSourceKind, Option<HttpResourceMetadata>), String> {
    let classified = classify_source(source)?;
    if classified == RemoteImportSourceKind::Torrent {
        return Ok((classified, None));
    }
    let metadata = probe_http_resource(source, proxy).await?;
    if classified == RemoteImportSourceKind::WebMedia
        && metadata.as_ref().is_some_and(|value| value.direct)
    {
        return Ok((RemoteImportSourceKind::DirectFile, metadata));
    }
    Ok((classified, metadata))
}

fn known_selection_total(selections: &[RemoteImportSelection]) -> Option<u64> {
    if selections.is_empty() {
        return None;
    }
    selections.iter().try_fold(0u64, |total, selection| {
        total.checked_add(selection.estimated_size?)
    })
}

fn disk_capacity_error(known_total: u64, existing_bytes: u64, available: u64) -> Option<String> {
    let remaining = known_total.saturating_sub(existing_bytes);
    if remaining == 0 {
        return None;
    }
    let required = remaining
        .checked_add(REMOTE_IMPORT_DISK_HEADROOM)
        .unwrap_or(u64::MAX);
    if available < required {
        Some(format!(
            "Insufficient disk space for Remote Import: {} bytes remain to acquire and {} bytes are available",
            remaining, available
        ))
    } else {
        None
    }
}

fn available_space_for_path(path: &Path) -> Option<u64> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| resolved.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
}

async fn ensure_known_disk_capacity(
    path: &Path,
    known_total: Option<u64>,
    existing_bytes: u64,
) -> Result<(), String> {
    let Some(known_total) = known_total else {
        return Ok(());
    };
    let path = path.to_path_buf();
    let available = tokio::task::spawn_blocking(move || available_space_for_path(&path))
        .await
        .map_err(|error| format!("Remote Import disk-space check failed: {error}"))?
        .ok_or_else(|| {
            "Remote Import could not determine available disk space for staging".to_string()
        })?;
    if let Some(error) = disk_capacity_error(known_total, existing_bytes, available) {
        return Err(error);
    }
    Ok(())
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn file_size(format: &Value) -> Option<u64> {
    format
        .get("filesize")
        .and_then(Value::as_u64)
        .or_else(|| format.get("filesize_approx").and_then(Value::as_u64))
}

fn is_hdr(format: &Value) -> bool {
    let dynamic = format
        .get("dynamic_range")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if !dynamic.is_empty() && dynamic != "sdr" && dynamic != "none" {
        return true;
    }
    format
        .get("format_note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
        .contains("hdr")
}

fn quality_label(height: u32, fps: Option<f64>, hdr: bool) -> String {
    let mut label = if height >= 4320 {
        "8K".to_string()
    } else if height >= 2160 {
        "4K".to_string()
    } else {
        format!("{height}p")
    };
    if fps.unwrap_or(0.0) > 30.5 {
        label.push_str(&format!("{}", fps.unwrap_or(0.0).round() as u32));
    }
    if hdr {
        label.push_str(" HDR");
    }
    label
}

/// Convert yt-dlp's often-long format table into one useful choice per visible
/// quality. The exact chosen format_id is kept so acquisition never has to
/// reconstruct a format expression from the display label.
pub fn normalize_yt_dlp_qualities(metadata: &Value) -> Result<Vec<RemoteImportQuality>, String> {
    let formats = metadata
        .get("formats")
        .and_then(Value::as_array)
        .ok_or_else(|| "This source did not expose selectable video qualities".to_string())?;

    let mut by_visible_quality: BTreeMap<(u32, u32, bool), RemoteImportQuality> = BTreeMap::new();
    for format in formats {
        let Some(format_id) = format.get("format_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(height) = format
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
        else {
            continue;
        };
        if height == 0 || format.get("vcodec").and_then(Value::as_str) == Some("none") {
            continue;
        }
        let fps = number(format.get("fps"));
        let fps_key = fps.unwrap_or(0.0).round().max(0.0) as u32;
        let hdr = is_hdr(format);
        let acodec = format
            .get("acodec")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let candidate = RemoteImportQuality {
            format_id: format_id.to_string(),
            label: quality_label(height, fps, hdr),
            height: Some(height),
            fps,
            hdr,
            ext: format
                .get("ext")
                .and_then(Value::as_str)
                .map(str::to_string),
            estimated_size: file_size(format),
            recommended: false,
            has_audio: acodec != "none",
        };
        let key = (height, fps_key, hdr);
        match by_visible_quality.get(&key) {
            None => {
                by_visible_quality.insert(key, candidate);
            }
            Some(existing) => {
                // Prefer a muxed format for simple/efficient imports. Otherwise
                // prefer a known smaller representation, keeping the first
                // stable extractor order when size is unknown.
                let replace = (!existing.has_audio && candidate.has_audio)
                    || (existing.has_audio == candidate.has_audio
                        && match (existing.estimated_size, candidate.estimated_size) {
                            (Some(old), Some(new)) => new < old,
                            (None, Some(_)) => true,
                            _ => false,
                        });
                if replace {
                    by_visible_quality.insert(key, candidate);
                }
            }
        }
    }

    let mut qualities: Vec<_> = by_visible_quality.into_values().collect();
    if qualities.is_empty() {
        return Err("This source did not expose a downloadable video format".to_string());
    }
    qualities.sort_by(|a, b| {
        a.height
            .cmp(&b.height)
            .then_with(|| {
                a.fps
                    .partial_cmp(&b.fps)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.hdr.cmp(&b.hdr))
    });

    let recommended_index = qualities
        .iter()
        .enumerate()
        .filter(|(_, quality)| quality.height == Some(1080))
        .max_by_key(|(_, quality)| (quality.has_audio, quality.fps.unwrap_or(0.0).round() as u32))
        .map(|(index, _)| index)
        .or_else(|| {
            qualities
                .iter()
                .enumerate()
                .filter(|(_, quality)| quality.height.unwrap_or(0) < 1080)
                .max_by_key(|(_, quality)| quality.height.unwrap_or(0))
                .map(|(index, _)| index)
        })
        .unwrap_or(0);
    qualities[recommended_index].recommended = true;
    Ok(qualities)
}

fn executable_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

async fn command_version(path: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Some("available".to_string())
    } else {
        Some(
            value
                .lines()
                .next()
                .unwrap_or("available")
                .chars()
                .take(120)
                .collect(),
        )
    }
}

fn managed_engine_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|path| path.join("remote-import").join("managed"))
        .map_err(|error| format!("Could not locate TeraRelay app data: {error}"))
}

fn bundled_engine_path(app: &tauri::AppHandle, name: &str) -> Option<PathBuf> {
    let executable = executable_name(name);
    let resource_dir = app.path().resource_dir().ok()?;
    [
        resource_dir.join("remote-import").join(&executable),
        resource_dir.join(&executable),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

async fn ensure_managed_yt_dlp(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let managed_dir = managed_engine_dir(app)?;
    tokio::fs::create_dir_all(&managed_dir)
        .await
        .map_err(|error| {
            format!("Could not create Remote Import managed engine directory: {error}")
        })?;
    let destination = managed_dir.join(executable_name("yt-dlp"));
    if !destination.is_file() {
        let bundled =
            bundled_engine_path(app, "yt-dlp").ok_or_else(|| missing_engine_message("yt-dlp"))?;
        tokio::fs::copy(&bundled, &destination)
            .await
            .map_err(|error| format!("Could not prepare managed yt-dlp: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&destination)
                .map_err(|error| error.to_string())?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&destination, permissions)
                .map_err(|error| format!("Could not make managed yt-dlp executable: {error}"))?;
        }
    }
    Ok(destination)
}

async fn locate_engine(
    app_handle: Option<&tauri::AppHandle>,
    name: &str,
) -> Option<(PathBuf, String)> {
    let executable = executable_name(name);
    if let Some(app) = app_handle {
        if name == "yt-dlp" {
            if let Ok(managed_dir) = managed_engine_dir(app) {
                let managed = managed_dir.join(&executable);
                if managed.is_file() {
                    if let Some(version) = command_version(&managed).await {
                        return Some((managed, version));
                    }
                }
            }
        }
        if let Some(candidate) = bundled_engine_path(app, name) {
            if let Some(version) = command_version(&candidate).await {
                return Some((candidate, version));
            }
        }
    }

    let path = PathBuf::from(&executable);
    command_version(&path).await.map(|version| (path, version))
}

pub fn missing_engine_message(name: &str) -> String {
    format!("Remote Import engine '{name}' is unavailable. Install or bundle the engine and retry.")
}

pub fn sanitize_engine_error(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(1200));
    for token in raw.split_whitespace() {
        let safe = if token.starts_with("http://")
            || token.starts_with("https://")
            || token.starts_with("magnet:")
        {
            "<redacted-source>"
        } else {
            token
        };
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(safe);
        if cleaned.chars().count() >= 1180 {
            cleaned = cleaned.chars().take(1180).collect();
            cleaned.push_str(" …");
            break;
        }
    }
    if cleaned.is_empty() {
        "Remote import engine failed without an error message".to_string()
    } else {
        cleaned
    }
}

pub fn normalize_engine_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("drm") {
        return "This source appears to use DRM-protected media, which is not supported by Remote Import.".to_string();
    }
    if lower.contains("not available in your country")
        || lower.contains("not available in your region")
        || lower.contains("geo-restricted")
        || lower.contains("geographic restriction")
    {
        return "This source is not available in your region.".to_string();
    }
    if lower.contains("private video")
        || lower.contains("private source")
        || lower.contains("is private")
    {
        return "This source is private and requires explicitly authorized access before Remote Import can use it.".to_string();
    }
    if lower.contains("confirm your age")
        || lower.contains("age-restricted")
        || lower.contains("age restricted")
    {
        return "This source is age-restricted and requires explicitly authorized sign-in."
            .to_string();
    }
    if lower.contains("nsig extraction failed")
        || lower.contains("signature extraction failed")
        || lower.contains("update yt-dlp")
        || lower.contains("extractor needs an update")
    {
        return "The media extractor needs an update for this source. Update Remote Import engines and retry.".to_string();
    }
    if lower.contains("login required")
        || lower.contains("sign in to")
        || lower.contains("authentication required")
    {
        return "This source requires explicitly authorized sign-in before Remote Import can access it.".to_string();
    }
    sanitize_engine_error(raw)
}

fn media_path_requires_probe(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| MEDIA_EXTENSIONS.contains(&value.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn ffprobe_output_has_media_stream(value: &Value) -> bool {
    value
        .get("streams")
        .and_then(Value::as_array)
        .map(|streams| {
            streams.iter().any(|stream| {
                matches!(
                    stream.get("codec_type").and_then(Value::as_str),
                    Some("audio") | Some("video")
                )
            })
        })
        .unwrap_or(false)
}

async fn verify_processed_media_file(
    app_handle: &tauri::AppHandle,
    path: &Path,
) -> Result<(), String> {
    if !media_path_requires_probe(path) {
        return Ok(());
    }
    let (ffprobe, _) = locate_engine(Some(app_handle), "ffprobe")
        .await
        .ok_or_else(|| missing_engine_message("ffprobe"))?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(ffprobe)
            .args(["-v", "error", "-show_streams", "-of", "json", "--"])
            .arg(path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Media verification timed out".to_string())?
    .map_err(|error| format!("Could not start media verification: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Processed media failed verification: {}",
            normalize_engine_error(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    let metadata = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|_| "Media verification returned invalid metadata".to_string())?;
    if !ffprobe_output_has_media_stream(&metadata) {
        return Err("Processed media contains no playable audio or video stream".to_string());
    }
    Ok(())
}

pub async fn run_metadata_engine(
    executable: &Path,
    source: &str,
    deno_path: Option<&Path>,
) -> Result<Value, String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--dump-single-json")
        .arg("--skip-download")
        .arg("--ignore-config")
        .arg("--no-warnings");
    if let Some(deno) = deno_path {
        command
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno.to_string_lossy()))
            .arg("--remote-components")
            .arg("ejs:github");
    }
    // The source is deliberately one direct argv entry. Never concatenate this
    // into a shell command: URLs may contain &, $, quotes, or other syntax.
    command.arg("--").arg(source).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), command.output())
        .await
        .map_err(|_| "Remote media analysis timed out".to_string())?
        .map_err(|error| format!("Could not start media extractor: {error}"))?;
    if !output.status.success() {
        return Err(normalize_engine_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|_| "Media extractor returned invalid metadata".to_string())
}

fn entry_item(entry: &Value, index: usize) -> RemoteImportItem {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("item-{index}"));
    let label = entry
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| entry.get("filename").and_then(Value::as_str))
        .unwrap_or(&id)
        .to_string();
    RemoteImportItem {
        id,
        label,
        path: entry
            .get("webpage_url")
            .and_then(Value::as_str)
            .or_else(|| entry.get("url").and_then(Value::as_str))
            .map(str::to_string),
        size: entry
            .get("filesize")
            .and_then(Value::as_u64)
            .or_else(|| entry.get("filesize_approx").and_then(Value::as_u64)),
        kind: "media_item".to_string(),
    }
}

pub fn analysis_from_yt_dlp(
    source_kind: RemoteImportSourceKind,
    metadata: &Value,
) -> Result<RemoteImportAnalysis, String> {
    let title = metadata
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| metadata.get("fulltitle").and_then(Value::as_str))
        .unwrap_or("Remote media")
        .to_string();
    let source_label = metadata
        .get("extractor_key")
        .and_then(Value::as_str)
        .or_else(|| metadata.get("extractor").and_then(Value::as_str))
        .unwrap_or("Web")
        .to_string();
    let entries = metadata.get("entries").and_then(Value::as_array);
    let items = entries
        .map(|values| {
            values
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_null())
                .map(|(index, entry)| entry_item(entry, index))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let qualities = if items.is_empty() && metadata.get("formats").is_some() {
        normalize_yt_dlp_qualities(metadata)?
    } else {
        Vec::new()
    };
    Ok(RemoteImportAnalysis {
        source_kind,
        title,
        source_label,
        duration_seconds: number(metadata.get("duration")),
        thumbnail: metadata
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(str::to_string),
        qualities,
        items,
    })
}

fn collect_gallery_objects<'a>(value: &'a Value, output: &mut Vec<&'a Value>) {
    match value {
        Value::Object(_) => output.push(value),
        Value::Array(values) => {
            for value in values {
                collect_gallery_objects(value, output);
            }
        }
        _ => {}
    }
}

async fn analyze_with_gallery(
    executable: &Path,
    source: &str,
) -> Result<RemoteImportAnalysis, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        tokio::process::Command::new(executable)
            .arg("--dump-json")
            .arg("--")
            .arg(source)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Gallery analysis timed out".to_string())?
    .map_err(|error| format!("Could not start gallery extractor: {error}"))?;
    if !output.status.success() {
        return Err(normalize_engine_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut raw_objects = Vec::new();
    let parsed_lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    for value in &parsed_lines {
        collect_gallery_objects(value, &mut raw_objects);
    }
    let mut items = Vec::new();
    for (index, object) in raw_objects.into_iter().enumerate() {
        if object.get("url").and_then(Value::as_str).is_none()
            && object.get("filename").and_then(Value::as_str).is_none()
        {
            continue;
        }
        let mut item = entry_item(object, index);
        item.id = format!("gallery:{}", index + 1);
        item.kind = "gallery_item".to_string();
        if !items
            .iter()
            .any(|existing: &RemoteImportItem| existing.id == item.id)
        {
            items.push(item);
        }
    }
    if items.is_empty() {
        return Err("Gallery extractor found no downloadable items".to_string());
    }
    Ok(RemoteImportAnalysis {
        source_kind: RemoteImportSourceKind::WebMedia,
        title: items
            .first()
            .map(|item| item.label.clone())
            .unwrap_or_else(|| "Gallery".into()),
        source_label: "gallery-dl".to_string(),
        duration_seconds: None,
        thumbnail: None,
        qualities: Vec::new(),
        items,
    })
}

fn direct_file_analysis(
    source: &str,
    metadata: Option<&HttpResourceMetadata>,
) -> Result<RemoteImportAnalysis, String> {
    let url = reqwest::Url::parse(source).map_err(|_| "Invalid direct file URL".to_string())?;
    let name = metadata
        .and_then(|value| value.filename.clone())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|name| !name.is_empty())
                .map(sanitize_import_filename)
        })
        .unwrap_or_else(|| "remote-file".to_string());
    Ok(RemoteImportAnalysis {
        source_kind: RemoteImportSourceKind::DirectFile,
        title: name.clone(),
        source_label: "Direct URL".to_string(),
        duration_seconds: None,
        thumbnail: None,
        qualities: Vec::new(),
        items: vec![RemoteImportItem {
            id: "direct".to_string(),
            label: name,
            path: None,
            size: metadata.and_then(|value| value.size),
            kind: "file".to_string(),
        }],
    })
}

#[tauri::command]
pub async fn cmd_remote_import_update_engines(
    app_handle: tauri::AppHandle,
) -> Result<Vec<RemoteImportEngineStatus>, String> {
    let yt_dlp = ensure_managed_yt_dlp(&app_handle).await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        tokio::process::Command::new(&yt_dlp)
            .args(["--update-to", "stable"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "yt-dlp update timed out".to_string())?
    .map_err(|error| format!("Could not start managed yt-dlp update: {error}"))?;
    if !output.status.success() {
        let detail = sanitize_engine_error(&String::from_utf8_lossy(&output.stderr));
        return Err(format!("Managed yt-dlp update failed: {detail}"));
    }
    let version = command_version(&yt_dlp)
        .await
        .ok_or_else(|| "Managed yt-dlp updated but could not be executed".to_string())?;
    Ok(vec![RemoteImportEngineStatus {
        name: "yt-dlp".to_string(),
        available: true,
        version: Some(version),
        required: true,
        purpose: "web/social media extraction".to_string(),
    }])
}

#[cfg(not(target_os = "ios"))]
fn torrent_engine_status() -> RemoteImportEngineStatus {
    RemoteImportEngineStatus {
        name: "librqbit".to_string(),
        available: true,
        version: Some(librqbit::version().to_string()),
        required: false,
        purpose: "magnet and torrent acquisition".to_string(),
    }
}

#[cfg(target_os = "ios")]
fn torrent_engine_status() -> RemoteImportEngineStatus {
    RemoteImportEngineStatus {
        name: "librqbit".to_string(),
        available: false,
        version: None,
        required: false,
        purpose: "magnet and torrent acquisition (unavailable on iOS)".to_string(),
    }
}

#[tauri::command]
pub async fn cmd_remote_import_engine_status(
    app_handle: tauri::AppHandle,
) -> Result<Vec<RemoteImportEngineStatus>, String> {
    let definitions = [
        ("yt-dlp", true, "web/social media extraction"),
        ("deno", false, "YouTube JavaScript challenge support"),
        ("ffmpeg", true, "video/audio merge and remux"),
        ("ffprobe", true, "media verification"),
        ("aria2c", false, "accelerated resumable HTTP downloads"),
        ("gallery-dl", false, "gallery/carousel fallback"),
    ];
    let mut statuses = Vec::with_capacity(definitions.len() + 1);
    for (name, required, purpose) in definitions {
        let found = locate_engine(Some(&app_handle), name).await;
        statuses.push(RemoteImportEngineStatus {
            name: name.to_string(),
            available: found.is_some(),
            version: found.map(|(_, version)| version),
            required,
            purpose: purpose.to_string(),
        });
    }
    statuses.push(torrent_engine_status());
    Ok(statuses)
}

#[tauri::command]
pub async fn cmd_remote_import_analyze(
    app_handle: tauri::AppHandle,
    source: String,
    net_config: State<'_, Arc<crate::vpn_optimizer::NetworkConfig>>,
) -> Result<RemoteImportAnalysis, String> {
    let source = source.trim().to_string();
    let proxy = net_config.effective_proxy_url();
    let (source_kind, http_metadata) = resolve_source_kind(&source, proxy.as_deref()).await?;
    match source_kind {
        RemoteImportSourceKind::DirectFile => direct_file_analysis(&source, http_metadata.as_ref()),
        RemoteImportSourceKind::Torrent => {
            let root = app_handle
                .path()
                .app_cache_dir()
                .map_err(|error| format!("Could not locate TeraRelay cache: {error}"))?
                .join("remote-import-analysis")
                .join(format!("torrent-{}", rand::random::<u64>()));
            tokio::fs::create_dir_all(&root)
                .await
                .map_err(|error| error.to_string())?;
            let result = analyze_torrent_source(&source, root.clone()).await;
            let _ = tokio::fs::remove_dir_all(&root).await;
            result
        }
        RemoteImportSourceKind::WebMedia => {
            let (yt_dlp, _) = locate_engine(Some(&app_handle), "yt-dlp")
                .await
                .ok_or_else(|| missing_engine_message("yt-dlp"))?;
            let deno = locate_engine(Some(&app_handle), "deno")
                .await
                .map(|(path, _)| path);
            match run_metadata_engine(&yt_dlp, &source, deno.as_deref()).await {
                Ok(metadata) => analysis_from_yt_dlp(RemoteImportSourceKind::WebMedia, &metadata),
                Err(yt_error) => {
                    if let Some((gallery, _)) = locate_engine(Some(&app_handle), "gallery-dl").await
                    {
                        match analyze_with_gallery(&gallery, &source).await {
                            Ok(analysis) => Ok(analysis),
                            Err(_) => Err(yt_error),
                        }
                    } else {
                        Err(yt_error)
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportSelection {
    pub id: String,
    pub label: Option<String>,
    pub source: Option<String>,
    pub format_id: Option<String>,
    pub has_audio: Option<bool>,
    pub height: Option<u32>,
    pub estimated_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportAcquireRequest {
    pub job_id: String,
    pub source: String,
    #[serde(default)]
    pub selections: Vec<RemoteImportSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportOutput {
    pub selection_id: String,
    pub staging_path: String,
    pub display_name: String,
    pub size: u64,
    pub label: Option<String>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteImportAcquireResult {
    pub job_id: String,
    pub outputs: Vec<RemoteImportOutput>,
}

#[derive(Debug, Clone, Serialize)]
struct RemoteImportProgressPayload {
    job_id: String,
    phase: String,
    downloaded_bytes: u64,
    total_bytes: u64,
    speed_bytes_per_sec: u64,
    selection_id: Option<String>,
}

static REMOTE_IMPORT_CANCELLATIONS: OnceLock<Mutex<HashMap<String, watch::Sender<bool>>>> =
    OnceLock::new();

fn cancellation_map() -> &'static Mutex<HashMap<String, watch::Sender<bool>>> {
    REMOTE_IMPORT_CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn validate_job_id(job_id: &str) -> Result<(), String> {
    if job_id.is_empty()
        || job_id.len() > 128
        || matches!(job_id, "." | "..")
        || !job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Remote Import job ID is invalid".to_string());
    }
    Ok(())
}

pub fn sanitize_import_filename(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let basename = normalized
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("remote-file");
    if matches!(basename, "." | "..") || basename.trim().is_empty() {
        return "remote-file".to_string();
    }
    let mut safe = String::with_capacity(basename.len().min(180));
    for ch in basename.chars().take(180) {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            safe.push('_');
        } else {
            safe.push(ch);
        }
    }
    let safe = safe.trim().trim_matches('.').trim().to_string();
    if safe.is_empty() || matches!(safe.as_str(), "." | "..") {
        "remote-file".to_string()
    } else {
        safe
    }
}

fn safe_segment(raw: &str) -> String {
    let mut safe = raw
        .chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    while safe.contains("__") {
        safe = safe.replace("__", "_");
    }
    let safe = safe.trim_matches('_').to_string();
    if safe.is_empty() {
        "item".to_string()
    } else {
        safe
    }
}

fn job_staging_dir(app_handle: &tauri::AppHandle, job_id: &str) -> Result<PathBuf, String> {
    validate_job_id(job_id)?;
    let cache = app_handle
        .path()
        .app_cache_dir()
        .map_err(|error| format!("Could not locate TeraRelay cache: {error}"))?;
    Ok(cache.join("remote-import").join(job_id))
}

pub fn register_import_cancellation(job_id: &str) -> Result<watch::Receiver<bool>, String> {
    validate_job_id(job_id)?;
    let mut map = cancellation_map()
        .lock()
        .map_err(|_| "Remote Import cancellation state is unavailable".to_string())?;
    if map.contains_key(job_id) {
        return Err("This Remote Import job is already running".to_string());
    }
    let (tx, rx) = watch::channel(false);
    map.insert(job_id.to_string(), tx);
    Ok(rx)
}

pub fn cancel_registered_import(job_id: &str) -> bool {
    let Ok(map) = cancellation_map().lock() else {
        return false;
    };
    map.get(job_id)
        .is_some_and(|sender| sender.send(true).is_ok())
}

pub fn unregister_import_cancellation(job_id: &str) {
    if let Ok(mut map) = cancellation_map().lock() {
        map.remove(job_id);
    }
}

fn import_is_active(job_id: &str) -> bool {
    cancellation_map()
        .lock()
        .map(|map| map.contains_key(job_id))
        .unwrap_or(false)
}

pub fn aria2_direct_args(source: &str, directory: &Path, filename: &str) -> Vec<String> {
    vec![
        "--continue=true".to_string(),
        "--max-connection-per-server=8".to_string(),
        "--split=8".to_string(),
        "--min-split-size=1M".to_string(),
        "--file-allocation=none".to_string(),
        "--auto-file-renaming=false".to_string(),
        "--allow-overwrite=true".to_string(),
        format!("--dir={}", directory.to_string_lossy()),
        format!("--out={filename}"),
        source.to_string(),
    ]
}

fn canonical_owned_path(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("Remote Import staging directory is unavailable: {error}"))?;
    let candidate = candidate
        .canonicalize()
        .map_err(|error| format!("Remote Import output is unavailable: {error}"))?;
    if !candidate.starts_with(&root) {
        return Err("Remote Import output escaped its owned staging directory".to_string());
    }
    Ok(candidate)
}

pub fn validate_acquired_file(
    job_dir: &Path,
    path: &Path,
    display_name: &str,
    selection_id: &str,
    label: Option<String>,
    height: Option<u32>,
) -> Result<RemoteImportOutput, String> {
    let path = canonical_owned_path(job_dir, path)?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("Could not inspect Remote Import output: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err("Remote Import produced an empty or invalid file".to_string());
    }
    Ok(RemoteImportOutput {
        selection_id: selection_id.to_string(),
        staging_path: path.to_string_lossy().to_string(),
        display_name: sanitize_import_filename(display_name),
        size: metadata.len(),
        label,
        height,
    })
}

fn directory_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            match entry.metadata() {
                Ok(metadata) if metadata.is_file() => metadata.len(),
                Ok(metadata) if metadata.is_dir() => directory_size(&path),
                _ => 0,
            }
        })
        .sum()
}

fn collect_regular_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(metadata) if metadata.is_file() => {
                let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("");
                if extension != "aria2" && !path.to_string_lossy().ends_with(".part") {
                    files.push(path);
                }
            }
            Ok(metadata) if metadata.is_dir() => files.extend(collect_regular_files(&path)),
            _ => {}
        }
    }
    files
}

fn emit_import_progress(
    app_handle: &tauri::AppHandle,
    job_id: &str,
    phase: &str,
    downloaded_bytes: u64,
    total_bytes: u64,
    speed: u64,
    selection_id: Option<&str>,
) {
    let _ = app_handle.emit(
        "remote-import-progress",
        RemoteImportProgressPayload {
            job_id: job_id.to_string(),
            phase: phase.to_string(),
            downloaded_bytes,
            total_bytes,
            speed_bytes_per_sec: speed,
            selection_id: selection_id.map(str::to_string),
        },
    );
}

async fn run_owned_command(
    mut command: tokio::process::Command,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    selection_id: &str,
    output_dir: &Path,
    total_hint: u64,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<std::process::Output, String> {
    command.kill_on_drop(true);
    let mut future = Box::pin(command.output());
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut last_bytes = directory_size(output_dir);
    let mut last_time = std::time::Instant::now();
    loop {
        tokio::select! {
            result = &mut future => {
                return result.map_err(|error| format!("Could not start Remote Import engine: {error}"));
            }
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    return Err("Remote import cancelled".to_string());
                }
            }
            _ = interval.tick() => {
                let current = directory_size(output_dir);
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_time).as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (current.saturating_sub(last_bytes) as f64 / elapsed) as u64
                } else { 0 };
                emit_import_progress(app_handle, job_id, "downloading", current, total_hint, speed, Some(selection_id));
                last_bytes = current;
                last_time = now;
            }
        }
    }
}

fn direct_filename(source: &str) -> String {
    reqwest::Url::parse(source)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_string))
        })
        .filter(|name| !name.is_empty())
        .map(|name| sanitize_import_filename(&name))
        .unwrap_or_else(|| "remote-file".to_string())
}

async fn acquire_direct_reqwest(
    source: &str,
    target: &Path,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    selection_id: &str,
    proxy: Option<&str>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(proxy) = proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| "Remote Import proxy configuration is invalid".to_string())?,
        );
    }
    let client = builder.build().map_err(|error| error.to_string())?;
    let existing = tokio::fs::metadata(target)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut request = client.get(source);
    if existing > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let append = existing > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !response.status().is_success() {
        return Err(format!(
            "Direct download returned HTTP {}",
            response.status()
        ));
    }
    let start = if append { existing } else { 0 };
    let total = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_total)
        .or_else(|| {
            response
                .content_length()
                .map(|remaining| start.saturating_add(remaining))
        })
        .unwrap_or(0);
    ensure_known_disk_capacity(
        target.parent().unwrap_or_else(|| Path::new(".")),
        (total > 0).then_some(total),
        start,
    )
    .await?;
    let mut file = if append {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(target)
            .await
    } else {
        tokio::fs::File::create(target).await
    }
    .map_err(|error| format!("Could not create Remote Import staging file: {error}"))?;
    let mut downloaded = start;
    let mut last_bytes = start;
    let mut last_time = std::time::Instant::now();
    let mut stream = response.bytes_stream();
    loop {
        tokio::select! {
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    return Err("Remote import cancelled".to_string());
                }
            }
            chunk = futures::StreamExt::next(&mut stream) => {
                let Some(chunk) = chunk else { break; };
                let bytes = chunk.map_err(|error| error.to_string())?;
                tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await.map_err(|error| error.to_string())?;
                downloaded = downloaded.saturating_add(bytes.len() as u64);
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_time).as_secs_f64();
                if elapsed >= 0.25 {
                    let speed = (downloaded.saturating_sub(last_bytes) as f64 / elapsed) as u64;
                    emit_import_progress(app_handle, job_id, "downloading", downloaded, total, speed, Some(selection_id));
                    last_bytes = downloaded;
                    last_time = now;
                }
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|error| error.to_string())?;
    file.sync_all().await.map_err(|error| error.to_string())?;
    emit_import_progress(
        app_handle,
        job_id,
        "downloading",
        downloaded,
        total.max(downloaded),
        0,
        Some(selection_id),
    );
    Ok(())
}

async fn acquire_direct(
    source: &str,
    job_dir: &Path,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    proxy: Option<&str>,
    http_metadata: Option<&HttpResourceMetadata>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<RemoteImportOutput, String> {
    let selection_id = "direct";
    let output_dir = job_dir.join("direct");
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| error.to_string())?;
    let name = http_metadata
        .and_then(|value| value.filename.clone())
        .unwrap_or_else(|| direct_filename(source));
    let target = output_dir.join(&name);
    let existing = tokio::fs::metadata(&target)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    ensure_known_disk_capacity(
        &output_dir,
        http_metadata.and_then(|value| value.size),
        existing,
    )
    .await?;
    let aria2 = locate_engine(Some(app_handle), "aria2c")
        .await
        .map(|(path, _)| path);
    if let Some(aria2) = aria2 {
        let mut command = tokio::process::Command::new(aria2);
        command.args(aria2_direct_args(source, &output_dir, &name));
        if let Some(proxy) = proxy {
            command.env("ALL_PROXY", proxy);
        }
        let output = run_owned_command(
            command,
            app_handle,
            job_id,
            selection_id,
            &output_dir,
            http_metadata.and_then(|value| value.size).unwrap_or(0),
            cancel_rx.clone(),
        )
        .await?;
        if output.status.success() && target.is_file() {
            return validate_acquired_file(job_dir, &target, &name, selection_id, None, None);
        }
        let partial = tokio::fs::metadata(&target)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if partial > 0 {
            return Err(format!(
                "aria2 paused with {partial} bytes saved; retry this import to resume"
            ));
        }
    }
    acquire_direct_reqwest(
        source,
        &target,
        app_handle,
        job_id,
        selection_id,
        proxy,
        cancel_rx,
    )
    .await?;
    validate_acquired_file(job_dir, &target, &name, selection_id, None, None)
}

fn yt_dlp_format(selection: &RemoteImportSelection) -> String {
    match selection.format_id.as_deref() {
        Some(format_id) if selection.has_audio == Some(false) => {
            format!("{format_id}+bestaudio/best")
        }
        Some(format_id) => format_id.to_string(),
        None => "bv*+ba/b".to_string(),
    }
}

async fn acquire_with_yt_dlp(
    selection: &RemoteImportSelection,
    original_source: &str,
    job_dir: &Path,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    proxy: Option<&str>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<Vec<RemoteImportOutput>, String> {
    let (yt_dlp, _) = locate_engine(Some(app_handle), "yt-dlp")
        .await
        .ok_or_else(|| missing_engine_message("yt-dlp"))?;
    let output_dir = job_dir.join(format!("web-{}", safe_segment(&selection.id)));
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| error.to_string())?;
    ensure_known_disk_capacity(
        &output_dir,
        selection.estimated_size,
        directory_size(&output_dir),
    )
    .await?;
    let target_source = selection.source.as_deref().unwrap_or(original_source);
    let mut command = tokio::process::Command::new(yt_dlp);
    command
        .arg("--ignore-config")
        .arg("--continue")
        .arg("--no-playlist")
        .arg("--format")
        .arg(yt_dlp_format(selection))
        .arg("--paths")
        .arg(&output_dir)
        .arg("--output")
        .arg("%(title).180B [%(id)s].%(ext)s")
        .arg("--print")
        .arg("after_move:filepath")
        .arg("--no-warnings");
    if let Some((ffmpeg, _)) = locate_engine(Some(app_handle), "ffmpeg").await {
        command.arg("--ffmpeg-location").arg(ffmpeg);
    }
    if let Some((deno, _)) = locate_engine(Some(app_handle), "deno").await {
        command
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno.to_string_lossy()))
            .arg("--remote-components")
            .arg("ejs:github");
    }
    if locate_engine(Some(app_handle), "aria2c").await.is_some() {
        command
            .arg("--downloader")
            .arg("aria2c")
            .arg("--downloader-args")
            .arg("aria2c:-x8 -s8 -k1M --continue=true --file-allocation=none");
    }
    if let Some(proxy) = proxy {
        command.env("ALL_PROXY", proxy);
    }
    command.arg("--").arg(target_source);
    let output = run_owned_command(
        command,
        app_handle,
        job_id,
        &selection.id,
        &output_dir,
        selection.estimated_size.unwrap_or(0),
        cancel_rx,
    )
    .await?;
    if !output.status.success() {
        return Err(normalize_engine_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    let printed_path = printed
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| path.is_file());
    let mut files = printed_path.into_iter().collect::<Vec<_>>();
    if files.is_empty() {
        files = collect_regular_files(&output_dir);
    }
    if files.is_empty() {
        return Err("Media extractor completed but produced no file".to_string());
    }
    let mut outputs = Vec::new();
    for file in files {
        verify_processed_media_file(app_handle, &file).await?;
        let name = file
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("remote-media");
        outputs.push(validate_acquired_file(
            job_dir,
            &file,
            name,
            &selection.id,
            selection.label.clone(),
            selection.height,
        )?);
    }
    Ok(outputs)
}

fn gallery_range(selection_id: &str) -> Result<usize, String> {
    selection_id
        .strip_prefix("gallery:")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| "Gallery selection is invalid".to_string())
}

async fn acquire_with_gallery(
    selection: &RemoteImportSelection,
    source: &str,
    job_dir: &Path,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    proxy: Option<&str>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<Vec<RemoteImportOutput>, String> {
    let index = gallery_range(&selection.id)?;
    let (gallery, _) = locate_engine(Some(app_handle), "gallery-dl")
        .await
        .ok_or_else(|| missing_engine_message("gallery-dl"))?;
    let output_dir = job_dir.join(format!("gallery-{index}"));
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| error.to_string())?;
    ensure_known_disk_capacity(
        &output_dir,
        selection.estimated_size,
        directory_size(&output_dir),
    )
    .await?;
    let mut command = tokio::process::Command::new(gallery);
    command
        .arg("--ignore-config")
        .arg("--range")
        .arg(index.to_string())
        .arg("--directory")
        .arg(&output_dir);
    if let Some(proxy) = proxy {
        command.env("ALL_PROXY", proxy);
    }
    command.arg("--").arg(source);
    let output = run_owned_command(
        command,
        app_handle,
        job_id,
        &selection.id,
        &output_dir,
        selection.estimated_size.unwrap_or(0),
        cancel_rx,
    )
    .await?;
    if !output.status.success() {
        return Err(normalize_engine_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    let files = collect_regular_files(&output_dir);
    if files.is_empty() {
        return Err("Gallery extractor completed but produced no file".to_string());
    }
    files
        .into_iter()
        .map(|file| {
            let name = file
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("gallery-item");
            validate_acquired_file(
                job_dir,
                &file,
                name,
                &selection.id,
                selection.label.clone(),
                None,
            )
        })
        .collect()
}

struct ImportCancellationGuard(String);
impl Drop for ImportCancellationGuard {
    fn drop(&mut self) {
        unregister_import_cancellation(&self.0);
    }
}

#[tauri::command]
pub async fn cmd_remote_import_acquire(
    req: RemoteImportAcquireRequest,
    app_handle: tauri::AppHandle,
    net_config: State<'_, Arc<crate::vpn_optimizer::NetworkConfig>>,
) -> Result<RemoteImportAcquireResult, String> {
    validate_job_id(&req.job_id)?;
    let source = req.source.trim().to_string();
    let proxy = net_config.effective_proxy_url();
    let (source_kind, http_metadata) = resolve_source_kind(&source, proxy.as_deref()).await?;
    let job_dir = job_staging_dir(&app_handle, &req.job_id)?;
    tokio::fs::create_dir_all(&job_dir)
        .await
        .map_err(|error| error.to_string())?;
    ensure_known_disk_capacity(
        &job_dir,
        known_selection_total(&req.selections),
        directory_size(&job_dir),
    )
    .await?;
    let cancel_rx = register_import_cancellation(&req.job_id)?;
    let _guard = ImportCancellationGuard(req.job_id.clone());
    let outputs = match source_kind {
        RemoteImportSourceKind::DirectFile => vec![
            acquire_direct(
                &source,
                &job_dir,
                &app_handle,
                &req.job_id,
                proxy.as_deref(),
                http_metadata.as_ref(),
                cancel_rx,
            )
            .await?,
        ],
        RemoteImportSourceKind::WebMedia => {
            let selections = if req.selections.is_empty() {
                vec![RemoteImportSelection {
                    id: "best".to_string(),
                    label: Some("Best".to_string()),
                    source: None,
                    format_id: None,
                    has_audio: None,
                    height: None,
                    estimated_size: None,
                }]
            } else {
                req.selections
            };
            let mut outputs = Vec::new();
            for selection in selections {
                if *cancel_rx.borrow() {
                    return Err("Remote import cancelled".to_string());
                }
                let mut acquired = if selection.id.starts_with("gallery:") {
                    acquire_with_gallery(
                        &selection,
                        &source,
                        &job_dir,
                        &app_handle,
                        &req.job_id,
                        proxy.as_deref(),
                        cancel_rx.clone(),
                    )
                    .await?
                } else {
                    acquire_with_yt_dlp(
                        &selection,
                        &source,
                        &job_dir,
                        &app_handle,
                        &req.job_id,
                        proxy.as_deref(),
                        cancel_rx.clone(),
                    )
                    .await?
                };
                outputs.append(&mut acquired);
            }
            outputs
        }
        RemoteImportSourceKind::Torrent => {
            acquire_torrent(
                &source,
                &req.selections,
                &job_dir,
                &app_handle,
                &req.job_id,
                cancel_rx,
            )
            .await?
        }
    };
    emit_import_progress(
        &app_handle,
        &req.job_id,
        "ready_to_upload",
        outputs.iter().map(|o| o.size).sum(),
        outputs.iter().map(|o| o.size).sum(),
        0,
        None,
    );
    Ok(RemoteImportAcquireResult {
        job_id: req.job_id,
        outputs,
    })
}

#[cfg(target_os = "ios")]
async fn acquire_torrent(
    _source: &str,
    _selections: &[RemoteImportSelection],
    _job_dir: &Path,
    _app_handle: &tauri::AppHandle,
    _job_id: &str,
    _cancel_rx: watch::Receiver<bool>,
) -> Result<Vec<RemoteImportOutput>, String> {
    Err("Torrent Remote Import is unavailable on iOS".to_string())
}

#[tauri::command]
pub async fn cmd_remote_import_cancel(job_id: String) -> Result<bool, String> {
    validate_job_id(&job_id)?;
    Ok(cancel_registered_import(&job_id))
}

#[tauri::command]
pub async fn cmd_remote_import_cleanup(
    app_handle: tauri::AppHandle,
    job_id: String,
) -> Result<bool, String> {
    validate_job_id(&job_id)?;
    if import_is_active(&job_id) {
        return Err("Cannot clean a Remote Import while it is still running".to_string());
    }
    let job_dir = job_staging_dir(&app_handle, &job_id)?;
    if !job_dir.exists() {
        return Ok(false);
    }
    tokio::fs::remove_dir_all(&job_dir)
        .await
        .map_err(|error| format!("Could not clean Remote Import staging: {error}"))?;
    Ok(true)
}

pub fn safe_torrent_relative_path(components: &[String]) -> Option<PathBuf> {
    if components.is_empty() {
        return None;
    }
    let mut path = PathBuf::new();
    for component in components {
        if component.is_empty()
            || component == "."
            || component == ".."
            || Path::new(component).is_absolute()
            || component.contains('/')
            || component.contains('\\')
        {
            return None;
        }
        let safe = sanitize_import_filename(component);
        if safe == "remote-file" && component != "remote-file" {
            return None;
        }
        path.push(safe);
    }
    Some(path)
}

fn torrent_padding_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        value == ".pad" || value.starts_with("_____padding_file_")
    })
}

pub fn torrent_selection_indices(
    selections: &[RemoteImportSelection],
) -> Result<Vec<usize>, String> {
    let mut indices = Vec::new();
    for selection in selections {
        let index = selection
            .id
            .strip_prefix("torrent:")
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| "Torrent file selection is invalid".to_string())?;
        indices.push(index);
    }
    indices.sort_unstable();
    indices.dedup();
    Ok(indices)
}

#[cfg(not(target_os = "ios"))]
pub fn torrent_download_options(
    selected: Vec<usize>,
    output_folder: &Path,
) -> librqbit::AddTorrentOptions {
    librqbit::AddTorrentOptions {
        only_files: if selected.is_empty() {
            None
        } else {
            Some(selected)
        },
        overwrite: true,
        output_folder: Some(output_folder.to_string_lossy().into_owned()),
        ..Default::default()
    }
}

#[cfg(not(target_os = "ios"))]
fn torrent_items_from_info(
    info: &librqbit::ValidatedTorrentMetaV1Info<librqbit::ByteBufOwned>,
) -> Vec<RemoteImportItem> {
    info.iter_file_details()
        .enumerate()
        .filter_map(|(index, details)| {
            let relative = safe_torrent_relative_path(&details.filename.to_vec())?;
            if torrent_padding_path(&relative) {
                return None;
            }
            Some(RemoteImportItem {
                id: format!("torrent:{index}"),
                label: relative.to_string_lossy().to_string(),
                path: None,
                size: Some(details.len),
                kind: "torrent_file".to_string(),
            })
        })
        .collect()
}

#[cfg(not(target_os = "ios"))]
async fn torrent_session(
    root: PathBuf,
    persistence: bool,
) -> Result<Arc<librqbit::Session>, String> {
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| format!("Could not create torrent staging directory: {error}"))?;
    let mut options = librqbit::SessionOptions::default();
    options.fastresume = persistence;
    if persistence {
        let state_dir = root.join(".rqbit-state");
        tokio::fs::create_dir_all(&state_dir)
            .await
            .map_err(|error| format!("Could not create torrent resume directory: {error}"))?;
        options.persistence = Some(librqbit::SessionPersistenceConfig::Json {
            folder: Some(state_dir),
        });
    }
    librqbit::Session::new_with_opts(root, options)
        .await
        .map_err(|error| format!("Could not initialize BitTorrent engine: {error}"))
}

#[cfg(not(target_os = "ios"))]
async fn torrent_add_source(source: &str) -> Result<librqbit::AddTorrent<'static>, String> {
    if let Ok(url) = reqwest::Url::parse(source) {
        if url.scheme() == "file" {
            let path = url.to_file_path().map_err(|_| {
                "Local .torrent URI could not be converted to a file path".to_string()
            })?;
            if path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| value.eq_ignore_ascii_case("torrent"))
                != Some(true)
            {
                return Err(
                    "Remote Import accepts local files only when they are .torrent metadata"
                        .to_string(),
                );
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| format!("Could not read local .torrent file: {error}"))?;
            return Ok(librqbit::AddTorrent::from_bytes(bytes));
        }
    }
    Ok(librqbit::AddTorrent::from_url(source.to_string()))
}

#[cfg(not(target_os = "ios"))]
async fn list_torrent_add(
    session: &Arc<librqbit::Session>,
    add: librqbit::AddTorrent<'_>,
) -> Result<librqbit::ListOnlyResponse, String> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session.add_torrent(
            add,
            Some(librqbit::AddTorrentOptions {
                list_only: true,
                ..Default::default()
            }),
        ),
    )
    .await
    .map_err(|_| "Torrent metadata discovery timed out".to_string())?
    .map_err(|error| format!("Could not read torrent metadata: {error}"))?;
    match response {
        librqbit::AddTorrentResponse::ListOnly(response) => Ok(response),
        _ => Err("BitTorrent engine did not return list-only metadata".to_string()),
    }
}

#[cfg(not(target_os = "ios"))]
async fn analyze_torrent_source(
    source: &str,
    root: PathBuf,
) -> Result<RemoteImportAnalysis, String> {
    let session = torrent_session(root, false).await?;
    let add = torrent_add_source(source).await?;
    let listed = list_torrent_add(&session, add).await;
    session.stop().await;
    let listed = listed?;
    let items = torrent_items_from_info(&listed.info);
    if items.is_empty() {
        return Err("Torrent contains no safe downloadable files".to_string());
    }
    Ok(RemoteImportAnalysis {
        source_kind: RemoteImportSourceKind::Torrent,
        title: listed
            .info
            .name()
            .map(|name| name.into_owned())
            .unwrap_or_else(|| "BitTorrent".to_string()),
        source_label: "BitTorrent".to_string(),
        duration_seconds: None,
        thumbnail: None,
        qualities: Vec::new(),
        items,
    })
}

#[cfg(target_os = "ios")]
async fn analyze_torrent_source(
    _source: &str,
    _root: PathBuf,
) -> Result<RemoteImportAnalysis, String> {
    Err("Torrent Remote Import is unavailable on iOS".to_string())
}

#[cfg(all(test, not(target_os = "ios")))]
async fn torrent_items_from_bytes_for_test(
    bytes: Vec<u8>,
    root: &Path,
) -> Result<Vec<RemoteImportItem>, String> {
    let mut options = librqbit::SessionOptions::default();
    options.dht = None;
    options.disable_trackers = true;
    options.disable_local_service_discovery = true;
    let session = librqbit::Session::new_with_opts(root.to_path_buf(), options)
        .await
        .map_err(|error| error.to_string())?;
    let listed = list_torrent_add(&session, librqbit::AddTorrent::from_bytes(bytes)).await?;
    let items = torrent_items_from_info(&listed.info);
    session.stop().await;
    Ok(items)
}

#[cfg(not(target_os = "ios"))]
fn torrent_file_layout(
    handle: &Arc<librqbit::ManagedTorrent>,
) -> Result<Vec<(usize, PathBuf, u64)>, String> {
    handle
        .with_metadata(|metadata| {
            metadata
                .info
                .iter_file_details()
                .enumerate()
                .filter_map(|(index, details)| {
                    let relative = safe_torrent_relative_path(&details.filename.to_vec())?;
                    if torrent_padding_path(&relative) {
                        return None;
                    }
                    Some((index, relative, details.len))
                })
                .collect::<Vec<_>>()
        })
        .map_err(|error| format!("Torrent metadata is unavailable: {error}"))
}

#[cfg(not(target_os = "ios"))]
async fn acquire_torrent(
    source: &str,
    selections: &[RemoteImportSelection],
    job_dir: &Path,
    app_handle: &tauri::AppHandle,
    job_id: &str,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<Vec<RemoteImportOutput>, String> {
    let torrent_root = job_dir.join("torrent");
    let data_root = torrent_root.join("data");
    tokio::fs::create_dir_all(&data_root)
        .await
        .map_err(|error| error.to_string())?;
    let session = torrent_session(torrent_root.clone(), true).await?;
    let selected = torrent_selection_indices(selections)?;
    let options = torrent_download_options(selected.clone(), &data_root);
    let add = torrent_add_source(source).await?;
    let mut add_future = Box::pin(session.add_torrent(add, Some(options)));
    let response = tokio::select! {
        result = &mut add_future => result.map_err(|error| format!("Could not start torrent: {error}"))?,
        changed = cancel_rx.changed() => {
            session.stop().await;
            if changed.is_err() || *cancel_rx.borrow() { return Err("Remote import cancelled".to_string()); }
            return Err("Torrent cancellation channel closed".to_string());
        }
    };
    let handle = response.into_handle().ok_or_else(|| {
        "Torrent engine returned metadata without starting the download".to_string()
    })?;
    handle
        .wait_until_initialized()
        .await
        .map_err(|error| format!("Torrent initialization failed: {error}"))?;
    let layout = torrent_file_layout(&handle)?;
    let available: std::collections::HashSet<usize> =
        layout.iter().map(|(index, _, _)| *index).collect();
    if selected.iter().any(|index| !available.contains(index)) {
        session.stop().await;
        return Err("Selected torrent file no longer exists in this torrent".to_string());
    }

    let mut completion = Box::pin(handle.wait_until_completed());
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut last_bytes = 0u64;
    let mut last_time = std::time::Instant::now();
    loop {
        tokio::select! {
            result = &mut completion => { result.map_err(|error| format!("Torrent download failed: {error}"))?; break; }
            changed = cancel_rx.changed() => {
                session.stop().await;
                if changed.is_err() || *cancel_rx.borrow() { return Err("Remote import cancelled".to_string()); }
            }
            _ = interval.tick() => {
                let stats = handle.stats();
                if let Some(error) = stats.error.as_deref() {
                    session.stop().await;
                    return Err(format!("Torrent download failed: {}", normalize_engine_error(error)));
                }
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_time).as_secs_f64();
                let speed = if elapsed > 0.0 { (stats.progress_bytes.saturating_sub(last_bytes) as f64 / elapsed) as u64 } else { 0 };
                emit_import_progress(app_handle, job_id, "downloading", stats.progress_bytes, stats.total_bytes, speed, None);
                last_bytes = stats.progress_bytes;
                last_time = now;
            }
        }
    }
    let selected_set: std::collections::HashSet<usize> = if selected.is_empty() {
        available
    } else {
        selected.into_iter().collect()
    };
    let mut outputs = Vec::new();
    for (index, relative, _) in layout {
        if !selected_set.contains(&index) {
            continue;
        }
        let path = data_root.join(&relative);
        let display_name = relative
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("torrent-file");
        outputs.push(validate_acquired_file(
            job_dir,
            &path,
            display_name,
            &format!("torrent:{index}"),
            None,
            None,
        )?);
    }
    session.stop().await;
    if outputs.is_empty() {
        return Err("Torrent completed but no selected files were found".to_string());
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_magnet_direct_file_and_web_sources() {
        assert_eq!(
            classify_source("magnet:?xt=urn:btih:abc").unwrap(),
            RemoteImportSourceKind::Torrent
        );
        assert_eq!(
            classify_source("https://cdn.example.com/movie.mp4").unwrap(),
            RemoteImportSourceKind::DirectFile
        );
        assert_eq!(
            classify_source("https://www.youtube.com/watch?v=abc").unwrap(),
            RemoteImportSourceKind::WebMedia
        );
        assert_eq!(
            classify_source("file:///tmp/legal-fixture.torrent").unwrap(),
            RemoteImportSourceKind::Torrent
        );
        assert!(classify_source("javascript:alert(1)").is_err());
        assert!(classify_source("file:///etc/passwd").is_err());
        assert!(classify_source("").is_err());
    }

    #[test]
    fn extensionless_http_headers_distinguish_files_from_web_pages() {
        assert!(http_headers_indicate_direct_resource(
            Some("application/octet-stream"),
            None,
        ));
        assert!(http_headers_indicate_direct_resource(
            Some("text/plain; charset=utf-8"),
            Some("attachment; filename=fixture.txt"),
        ));
        assert!(http_headers_indicate_direct_resource(
            Some("video/mp4"),
            None,
        ));
        assert!(!http_headers_indicate_direct_resource(
            Some("text/html; charset=utf-8"),
            None,
        ));
        assert!(!http_headers_indicate_direct_resource(
            Some("application/json"),
            None,
        ));
    }

    #[test]
    fn normalizes_video_formats_and_recommends_one_1080p_choice() {
        let value = json!({
            "formats": [
                {"format_id":"18","height":360,"fps":30,"vcodec":"avc1","acodec":"mp4a","ext":"mp4","filesize":20_000_000},
                {"format_id":"22","height":720,"fps":30,"vcodec":"avc1","acodec":"mp4a","ext":"mp4","filesize":70_000_000},
                {"format_id":"137","height":1080,"fps":30,"vcodec":"avc1","acodec":"none","ext":"mp4","filesize":140_000_000},
                {"format_id":"399","height":1080,"fps":30,"vcodec":"av01","acodec":"none","ext":"mp4","filesize":120_000_000},
                {"format_id":"401","height":2160,"fps":60,"dynamic_range":"HDR10","vcodec":"av01","acodec":"none","ext":"mp4","filesize":620_000_000},
                {"format_id":"702","height":4320,"fps":30,"vcodec":"av01","acodec":"none","ext":"webm","filesize":1_800_000_000}
            ]
        });
        let qualities = normalize_yt_dlp_qualities(&value).unwrap();
        assert!(qualities.iter().any(|q| q.label == "720p"));
        assert!(qualities.iter().any(|q| q.label == "1080p"));
        assert!(qualities.iter().any(|q| q.label == "4K60 HDR"));
        assert!(qualities.iter().any(|q| q.label == "8K"));
        assert_eq!(qualities.iter().filter(|q| q.recommended).count(), 1);
        assert_eq!(
            qualities.iter().find(|q| q.recommended).unwrap().height,
            Some(1080)
        );
        assert!(qualities.iter().all(|q| !q.format_id.is_empty()));
        assert_eq!(
            qualities.iter().filter(|q| q.height == Some(1080)).count(),
            1,
            "same visible quality must be deduplicated"
        );
    }

    #[test]
    fn recommended_falls_back_to_best_below_1080_when_1080_missing() {
        let value = json!({"formats":[
            {"format_id":"a","height":480,"fps":30,"vcodec":"h264","acodec":"aac","ext":"mp4"},
            {"format_id":"b","height":720,"fps":30,"vcodec":"h264","acodec":"aac","ext":"mp4"}
        ]});
        let qualities = normalize_yt_dlp_qualities(&value).unwrap();
        let recommended = qualities.iter().find(|q| q.recommended).unwrap();
        assert_eq!(recommended.height, Some(720));
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yt_dlp_metadata_becomes_analysis_without_losing_playlist_entries() {
        let metadata = json!({
            "title": "Example playlist",
            "extractor_key": "YoutubeTab",
            "entries": [
                {"id":"one","title":"Episode One","duration":61.0,"webpage_url":"https://example.test/one"},
                {"id":"two","title":"Episode Two","duration":72.0,"webpage_url":"https://example.test/two"}
            ]
        });
        let analysis = analysis_from_yt_dlp(RemoteImportSourceKind::WebMedia, &metadata).unwrap();
        assert_eq!(analysis.title, "Example playlist");
        assert_eq!(analysis.source_label, "YoutubeTab");
        assert_eq!(analysis.items.len(), 2);
        assert_eq!(analysis.items[0].id, "one");
        assert_eq!(analysis.items[1].label, "Episode Two");
    }

    #[test]
    fn engine_errors_redact_urls_and_are_bounded() {
        let raw = format!(
            "ERROR: request failed https://example.test/video?token=super-secret {}",
            "x".repeat(4000)
        );
        let cleaned = sanitize_engine_error(&raw);
        assert!(!cleaned.contains("super-secret"));
        assert!(!cleaned.contains("https://example.test"));
        assert!(cleaned.len() <= 1200);
        assert!(cleaned.contains("ERROR"));
    }

    #[test]
    fn missing_engine_message_is_actionable() {
        let message = missing_engine_message("yt-dlp");
        assert!(message.contains("yt-dlp"));
        assert!(message.contains("Remote Import"));
        assert!(message.contains("engine"));
    }

    #[test]
    fn engine_failures_are_normalized_into_actionable_categories() {
        let login = normalize_engine_error(
            "ERROR: Sign in to confirm your age https://example.test/watch?token=secret",
        );
        assert!(login.to_ascii_lowercase().contains("age"));
        assert!(!login.contains("secret"));

        let geo = normalize_engine_error("ERROR: This video is not available in your country");
        assert!(geo.to_ascii_lowercase().contains("region"));

        let private =
            normalize_engine_error("ERROR: Private video. Sign in if you've been granted access");
        assert!(private.to_ascii_lowercase().contains("private"));

        let drm = normalize_engine_error("ERROR: This content is DRM protected");
        assert!(drm.to_ascii_lowercase().contains("drm"));
        assert!(drm.to_ascii_lowercase().contains("not supported"));

        let update = normalize_engine_error(
            "ERROR: nsig extraction failed; please report this issue and update yt-dlp",
        );
        assert!(update.to_ascii_lowercase().contains("update"));
    }

    #[test]
    fn ffprobe_validation_requires_a_real_audio_or_video_stream() {
        assert!(media_path_requires_probe(Path::new("movie.mp4")));
        assert!(media_path_requires_probe(Path::new("song.flac")));
        assert!(!media_path_requires_probe(Path::new("cover.jpg")));
        assert!(!media_path_requires_probe(Path::new("notes.txt")));
        assert!(ffprobe_output_has_media_stream(&json!({
            "streams": [{"codec_type":"video"}]
        })));
        assert!(ffprobe_output_has_media_stream(&json!({
            "streams": [{"codec_type":"audio"}]
        })));
        assert!(!ffprobe_output_has_media_stream(&json!({"streams": []})));
        assert!(!ffprobe_output_has_media_stream(&json!({
            "streams": [{"codec_type":"subtitle"}]
        })));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_receives_source_as_literal_argument_without_shell_expansion() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("terarelay-engine-argv-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-yt-dlp");
        let args_file = root.join("args.txt");
        let marker = root.join("SHOULD_NOT_EXIST");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{{\"title\":\"Safe\",\"extractor_key\":\"Test\",\"formats\":[{{\"format_id\":\"x\",\"height\":720,\"fps\":30,\"vcodec\":\"h264\",\"acodec\":\"aac\",\"ext\":\"mp4\"}}]}}'\n",
            args_file.display()
        );
        std::fs::write(&script, body).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let source = format!("https://example.test/watch?v=$(touch {})", marker.display());
        let output = run_metadata_engine(&script, &source, None).await.unwrap();
        assert_eq!(
            output.get("title").and_then(serde_json::Value::as_str),
            Some("Safe")
        );
        let args = std::fs::read_to_string(&args_file).unwrap();
        assert!(args.lines().any(|arg| arg == source));
        assert!(!marker.exists(), "source text was interpreted by a shell");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod acquisition_tests {
    use super::*;

    #[test]
    fn job_ids_and_filenames_cannot_escape_owned_staging() {
        assert!(validate_job_id("import-AbC_123").is_ok());
        for bad in ["", "../escape", "a/b", "a\\b", ".", "..", "job id", "x;rm"] {
            assert!(
                validate_job_id(bad).is_err(),
                "accepted unsafe job id: {bad}"
            );
        }
        assert_eq!(sanitize_import_filename("../../movie?.mp4"), "movie_.mp4");
        assert_eq!(sanitize_import_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_import_filename(".."), "remote-file");
        assert_eq!(sanitize_import_filename("a/b\\c.zip"), "c.zip");
    }

    #[test]
    fn aria2_direct_download_args_are_resumable_and_keep_url_literal() {
        let source = "https://example.test/file.mp4?x=1&y=$(touch nope)";
        let args = aria2_direct_args(source, Path::new("/safe/job"), "movie.mp4");
        assert!(args.iter().any(|arg| arg == "--continue=true"));
        assert!(args
            .iter()
            .any(|arg| arg == "--max-connection-per-server=8"));
        assert!(args.iter().any(|arg| arg == "--split=8"));
        assert_eq!(args.last().map(String::as_str), Some(source));
    }

    #[test]
    fn known_selection_size_and_disk_preflight_are_overflow_safe_and_resume_aware() {
        let known = vec![
            RemoteImportSelection {
                id: "a".into(),
                label: None,
                source: None,
                format_id: None,
                has_audio: None,
                height: None,
                estimated_size: Some(80),
            },
            RemoteImportSelection {
                id: "b".into(),
                label: None,
                source: None,
                format_id: None,
                has_audio: None,
                height: None,
                estimated_size: Some(20),
            },
        ];
        assert_eq!(known_selection_total(&known), Some(100));
        let mut unknown = known.clone();
        unknown[1].estimated_size = None;
        assert_eq!(known_selection_total(&unknown), None);
        let mut overflow = known.clone();
        overflow[0].estimated_size = Some(u64::MAX);
        overflow[1].estimated_size = Some(1);
        assert_eq!(known_selection_total(&overflow), None);

        assert!(disk_capacity_error(100, 40, 100 + REMOTE_IMPORT_DISK_HEADROOM).is_none());
        let error = disk_capacity_error(100, 40, 59 + REMOTE_IMPORT_DISK_HEADROOM).unwrap();
        assert!(error.to_ascii_lowercase().contains("disk space"));
        assert!(disk_capacity_error(100, 100, REMOTE_IMPORT_DISK_HEADROOM).is_none());
    }

    #[tokio::test]
    async fn cancellation_is_isolated_per_job() {
        let a = format!("qa-a-{}", rand::random::<u64>());
        let b = format!("qa-b-{}", rand::random::<u64>());
        let mut rx_a = register_import_cancellation(&a).unwrap();
        let rx_b = register_import_cancellation(&b).unwrap();
        assert!(cancel_registered_import(&a));
        rx_a.changed().await.unwrap();
        assert!(*rx_a.borrow());
        assert!(!*rx_b.borrow());
        unregister_import_cancellation(&a);
        unregister_import_cancellation(&b);
    }

    #[test]
    fn sparse_output_above_two_gb_is_valid_when_owned_and_nonempty() {
        let root =
            std::env::temp_dir().join(format!("terarelay-large-import-{}", rand::random::<u64>()));
        let job = root.join("job-safe");
        std::fs::create_dir_all(&job).unwrap();
        let output = job.join("large.bin");
        let file = std::fs::File::create(&output).unwrap();
        file.set_len(2_147_483_649).unwrap();
        drop(file);
        let validated =
            validate_acquired_file(&job, &output, "large.bin", "direct", None, None).unwrap();
        assert_eq!(validated.size, 2_147_483_649);
        assert_eq!(validated.display_name, "large.bin");
        let outside = root.join("outside.bin");
        std::fs::write(&outside, b"outside").unwrap();
        assert!(
            validate_acquired_file(&job, &outside, "outside.bin", "direct", None, None).is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(all(test, not(target_os = "ios")))]
mod torrent_tests {
    use super::*;

    #[test]
    fn torrent_paths_are_relative_and_sanitized() {
        assert_eq!(
            safe_torrent_relative_path(&["Movies".into(), "clip?.mkv".into()]).unwrap(),
            PathBuf::from("Movies").join("clip_.mkv")
        );
        assert!(safe_torrent_relative_path(&["..".into(), "escape.bin".into()]).is_none());
        assert!(safe_torrent_relative_path(&["/etc".into(), "passwd".into()]).is_none());
        assert!(safe_torrent_relative_path(&[]).is_none());
    }

    #[test]
    fn selected_torrent_indices_are_exact_deduplicated_and_sorted() {
        let selections = vec![
            RemoteImportSelection {
                id: "torrent:5".into(),
                label: None,
                source: None,
                format_id: None,
                has_audio: None,
                height: None,
                estimated_size: None,
            },
            RemoteImportSelection {
                id: "torrent:2".into(),
                label: None,
                source: None,
                format_id: None,
                has_audio: None,
                height: None,
                estimated_size: None,
            },
            RemoteImportSelection {
                id: "torrent:5".into(),
                label: None,
                source: None,
                format_id: None,
                has_audio: None,
                height: None,
                estimated_size: None,
            },
        ];
        assert_eq!(torrent_selection_indices(&selections).unwrap(), vec![2, 5]);
        let bad = vec![RemoteImportSelection {
            id: "torrent:../../bad".into(),
            label: None,
            source: None,
            format_id: None,
            has_audio: None,
            height: None,
            estimated_size: None,
        }];
        assert!(torrent_selection_indices(&bad).is_err());
    }

    #[test]
    fn torrent_download_options_select_only_requested_files_inside_owned_output() {
        let output = Path::new("/safe/job/torrent-data");
        let options = torrent_download_options(vec![2, 5], output);
        assert_eq!(options.only_files, Some(vec![2, 5]));
        assert!(
            options.overwrite,
            "resume requires overwrite for existing pieces"
        );
        assert_eq!(
            options.output_folder.as_deref(),
            Some("/safe/job/torrent-data")
        );
        assert!(!options.list_only);
    }

    #[tokio::test]
    async fn librqbit_list_only_reads_a_tiny_local_fixture_without_network() {
        let bytes = b"d4:infod6:lengthi4e4:name8:test.bin12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let root =
            std::env::temp_dir().join(format!("terarelay-rqbit-list-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let items = torrent_items_from_bytes_for_test(bytes.to_vec(), &root)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "torrent:0");
        assert_eq!(items[0].label, "test.bin");
        assert_eq!(items[0].size, Some(4));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn local_torrent_file_uri_is_read_through_the_real_source_adapter() {
        let bytes = b"d4:infod6:lengthi4e4:name8:test.bin12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let root = std::env::temp_dir().join(format!(
            "terarelay-local-torrent-source-{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let torrent_path = root.join("fixture.torrent");
        std::fs::write(&torrent_path, bytes).unwrap();
        let source = reqwest::Url::from_file_path(&torrent_path)
            .unwrap()
            .to_string();
        assert_eq!(
            classify_source(&source).unwrap(),
            RemoteImportSourceKind::Torrent
        );

        let session_root = root.join("session");
        let session = torrent_session(session_root, false).await.unwrap();
        let add = torrent_add_source(&source).await.unwrap();
        let listed = list_torrent_add(&session, add).await.unwrap();
        let items = torrent_items_from_info(&listed.info);
        session.stop().await;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "test.bin");
        assert_eq!(items[0].size, Some(4));
        let _ = std::fs::remove_dir_all(root);
    }
}
