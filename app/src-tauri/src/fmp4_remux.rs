// ── fMP4 Remux Module ───────────────────────────────────────────────────
// Handles on-the-fly conversion of progressive (moov-at-end) MP4 files into
// fragmented MP4 (fMP4), copying video and converting audio to AAC. The
// output fMP4 can be parsed by mp4box and fed into the frontend's MediaSource
// Extensions pipeline, eliminating the need to fall back to native <video>.
//
// Cache layout:
//   $APPDATA/streaming/fmp4/{folder_id}_{message_id}/output.mp4

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::commands::streaming::StreamConfig;
use crate::server::StreamTokenData;
use crate::transcode::TranscodeManager;
use actix_web::{web, HttpRequest, HttpResponse, Responder};

// ── Constants ────────────────────────────────────────────────────────

/// Subdirectory under the streaming cache root for fMP4 outputs.
const FMP4_DIR: &str = "fmp4";
#[path = "fmp4_seek.rs"]
mod seek;
use seek::{probe_seek_anchor, SeekAnchor};

// ── Types ────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct Fmp4StreamInfo {
    pub url: String,
    pub output_file_key: String,
    /// "ready" if the fMP4 is available, "processing" if download/remux is in progress.
    pub status: String,
    pub window_start_secs: f64,
}

#[derive(serde::Serialize, Clone)]
pub struct Fmp4StatusResult {
    pub status: String,
    pub error: Option<String>,
    /// True once the immutable output supports HTTP ranges and seeking.
    pub complete: bool,
}

/// Shared state for tracking in-flight fMP4 remux jobs.
/// Managed as Tauri state so both commands and the frontend can query progress.
#[derive(Clone)]
pub struct Fmp4RemuxState {
    /// Maps file_key → job status: None = not started, Some(None) = in progress,
    /// Some(Some(err)) = failed with error. Absent + output exists = ready.
    jobs: Arc<Mutex<HashMap<String, Option<String>>>>,
    active_sources: Arc<Mutex<HashMap<String, ActiveSource>>>,
}

#[derive(Default)]
struct ActiveSource {
    generation: u64,
    key: Option<String>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Fmp4RemuxState {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            active_sources: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// ── FFmpeg Remux ─────────────────────────────────────────────────────

/// Run FFmpeg to remux a progressive MP4 into a fragmented MP4 (fMP4).
///
/// Copies video packets and converts the selected audio to browser-compatible AAC.
///
/// # Flags
/// - `frag_keyframe`  — start a new fragment at every video keyframe
/// - `empty_moov`     — initial moov is minimal; track metadata lives in moof boxes
/// - `default_base_moof` — ensures each moof has the necessary base offset
///
/// These flags produce an fMP4 that mp4box's `initializeSegmentation()` can
/// handle, enabling full MSE playback.
pub async fn run_fmp4_remux(
    ffmpeg_path: &Path,
    input_path: &Path,
    output_path: &Path,
    audio_stream_index: Option<i32>,
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    progress_callback: impl Fn(f32),
) -> Result<(), String> {
    run_fmp4_remux_at(
        ffmpeg_path,
        input_path,
        output_path,
        audio_stream_index,
        None,
        cancel_rx,
        progress_callback,
    )
    .await
}

async fn run_fmp4_remux_at(
    ffmpeg_path: &Path,
    input_path: &Path,
    output_path: &Path,
    audio_stream_index: Option<i32>,
    anchor: Option<SeekAnchor>,
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    progress_callback: impl Fn(f32),
) -> Result<(), String> {
    // Ensure output directory exists
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create fMP4 output dir: {}", e))?;
    }

    let mut cmd = tokio::process::Command::new(ffmpeg_path);
    cmd.arg("-y");
    let input_name = input_path.to_string_lossy();
    if input_name.starts_with("http://") || input_name.starts_with("https://") {
        cmd.arg("-rw_timeout").arg("20000000");
    }
    if let Some(anchor) = anchor {
        cmd.arg("-seek_timestamp")
            .arg("1")
            .arg("-ss")
            .arg(format!("{:.6}", anchor.source_timestamp));
    }
    cmd.arg("-i")
        .arg(input_path)
        // Explicit mapping keeps playback deterministic when the source has
        // multiple audio tracks. Subtitles are handled separately as WebVTT.
        .arg("-map")
        .arg("0:v:0");

    if let Some(stream_index) = audio_stream_index {
        cmd.arg("-map").arg(format!("0:{stream_index}?"));
    } else {
        cmd.arg("-map").arg("0:a:0?");
    }

    cmd.arg("-sn")
        .arg("-dn")
        .arg("-c:v")
        .arg("copy")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("192k")
        .arg("-ac")
        .arg("2")
        .arg("-movflags")
        .arg("frag_keyframe+empty_moov+default_base_moof")
        .arg("-frag_duration")
        .arg("2000000")
        .arg("-flush_packets")
        .arg("1")
        .arg("-f")
        .arg("mp4")
        .arg(output_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn FFmpeg for fMP4 remux: {}", e))?;

    let stderr = child
        .stderr
        .take()
        .ok_or("No stderr pipe for FFmpeg fMP4 remux")?;

    // Read stderr lines for progress (best-effort) and error collection
    let stderr_reader = tokio::io::BufReader::new(stderr);
    let mut lines = tokio::io::AsyncBufReadExt::lines(stderr_reader);
    let input_size = std::fs::metadata(input_path).map(|m| m.len()).unwrap_or(0);
    let mut stderr_tail = std::collections::VecDeque::new();

    let parse_result: Result<(), String> = loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = std::fs::remove_file(output_path);
                break Err("Cancelled".to_string());
            }
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        let mut safe_line = line.replace(input_name.as_ref(), "[video source]");
                        if let Some(token) = input_name.split("token=").nth(1).and_then(|value| value.split('&').next()) {
                            if !token.is_empty() { safe_line = safe_line.replace(token, "[redacted]"); }
                        }
                        stderr_tail.push_back(safe_line.chars().take(1024).collect::<String>());
                        if stderr_tail.len() > 8 { stderr_tail.pop_front(); }
                        // Parse time= for progress
                        if let Some(time_str) = line.split("time=").nth(1) {
                            let time_str = time_str.split_whitespace().next().unwrap_or("0");
                            if let Ok(secs) = parse_ffmpeg_time(time_str) {
                                // With -c copy, duration is the total input duration.
                                // Report progress based on time position.
                                if input_size > 0 && secs > 0.0 {
                                    // Coarse progress from stderr time markers
                                    progress_callback(0.5); // FFmpeg spends ~50% time reading input
                                }
                            }
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => {
                        log::warn!("fMP4 remux: stderr read error: {}", e);
                        break Ok(());
                    }
                }
            }
        }
    };

    // Check cancellation
    parse_result?;

    let status = child
        .wait()
        .await
        .map_err(|e| format!("FFmpeg fMP4 wait error: {}", e))?;

    if !status.success() {
        let _ = std::fs::remove_file(output_path);
        return Err(format!(
            "FFmpeg fMP4 remux exited with code {:?}:\n{}",
            status.code(),
            stderr_tail.into_iter().collect::<Vec<_>>().join("\n")
        ));
    }

    // Verify output
    if !output_path.exists() {
        return Err("FFmpeg fMP4 remux completed but no output file was produced".to_string());
    }

    let output_size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if output_size == 0 {
        let _ = std::fs::remove_file(output_path);
        return Err("FFmpeg fMP4 remux produced an empty output file".to_string());
    }

    log::info!(
        "fMP4 remux: output {:?} ({} bytes)",
        output_path,
        output_size
    );

    Ok(())
}

/// Parse an FFmpeg time string like "00:05:30.12" into seconds.
fn parse_ffmpeg_time(time: &str) -> Result<f64, ()> {
    let parts: Vec<&str> = time.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().map_err(|_| ())?;
        let m: f64 = parts[1].parse().map_err(|_| ())?;
        let s: f64 = parts[2].parse().map_err(|_| ())?;
        Ok(h * 3600.0 + m * 60.0 + s)
    } else {
        Err(())
    }
}

/// Publish a growing fMP4 only after moov and one complete moof/mdat pair.
/// Metadata alone is nonempty but cannot produce a decoded video frame.
fn has_playable_fragment(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    let total = metadata.len();
    let mut offset = 0u64;
    let mut moov = false;
    let mut moof = false;
    while offset.saturating_add(8) <= total {
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return false;
        }
        let short_size = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, header_size) = match short_size {
            0 => return false, // Size extends to EOF: not final while growing.
            1 => {
                let mut extended = [0u8; 8];
                if file.read_exact(&mut extended).is_err() {
                    return false;
                }
                (u64::from_be_bytes(extended), 16u64)
            }
            size => (u64::from(size), 8),
        };
        let Some(end) = offset.checked_add(size) else {
            return false;
        };
        if size < header_size || end > total {
            return false;
        }
        match &header[4..8] {
            b"moov" => moov = true,
            b"moof" => moof = true,
            b"mdat" if moov && moof && size > header_size => return true,
            _ => {}
        }
        offset = end;
    }
    false
}

fn growing_mp4(
    partial: std::path::PathBuf,
    finished: std::path::PathBuf,
    idle_timeout: std::time::Duration,
) -> impl futures::Stream<Item = Result<web::Bytes, actix_web::Error>> {
    growing_mp4_with_eof(partial, finished, idle_timeout, |_| {})
}

fn growing_mp4_with_eof(
    partial: std::path::PathBuf,
    finished: std::path::PathBuf,
    idle_timeout: std::time::Duration,
    mut at_eof: impl FnMut(bool),
) -> impl futures::Stream<Item = Result<web::Bytes, actix_web::Error>> {
    async_stream::stream! {
        use tokio::io::AsyncReadExt;
        let mut file = match tokio::fs::File::open(&partial).await {
            Ok(file) => file,
            Err(_) => match tokio::fs::File::open(&finished).await {
                Ok(file) => file,
                Err(error) => { yield Err(actix_web::error::ErrorBadGateway(error)); return; }
            }
        };
        let mut last_data = tokio::time::Instant::now();
        let mut completed = false;
        let mut buffer = vec![0u8; 65_536];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => {
                    at_eof(false);
                    if completed { return; }
                    if tokio::fs::try_exists(&finished).await.unwrap_or(false) {
                        // Completion can occur after the preceding read saw
                        // EOF. Drain the now-final descriptor before stopping.
                        completed = true;
                        continue;
                    }
                    at_eof(true);
                    if !tokio::fs::try_exists(&partial).await.unwrap_or(false) {
                        // Rename may occur between the two existence checks.
                        if tokio::fs::try_exists(&finished).await.unwrap_or(false) {
                            completed = true;
                            continue;
                        }
                        yield Err(actix_web::error::ErrorBadGateway("Video remux failed"));
                        return;
                    }
                    if last_data.elapsed() >= idle_timeout {
                        yield Err(actix_web::error::ErrorGatewayTimeout("Video stream stopped receiving data"));
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Ok(length) => {
                    last_data = tokio::time::Instant::now();
                    yield Ok(web::Bytes::copy_from_slice(&buffer[..length]));
                }
                Err(error) => { yield Err(actix_web::error::ErrorBadGateway(error)); return; }
            }
        }
    }
}

// ── Tauri Commands ───────────────────────────────────────────────────

/// Prepare a fragmented MP4 stream for a progressive MP4 file.
///
/// Returns immediately with `status: "ready"` if the fMP4 is cached, or
/// `status: "processing"` after spawning the remux in the background.
/// The frontend polls until a complete media fragment can be played.
impl Fmp4RemuxState {
    async fn reserve_source(&self, source: &str) -> u64 {
        let mut active = self.active_sources.lock().await;
        let entry = active.entry(source.to_string()).or_default();
        entry.generation += 1;
        entry.generation
    }

    async fn install_job(
        &self,
        source: &str,
        generation: u64,
        file_key: &str,
        cancel: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(), String> {
        let mut active = self.active_sources.lock().await;
        let entry = active.get_mut(source).ok_or("Video source changed")?;
        if entry.generation != generation {
            return Err("Video source changed".to_string());
        }
        if let Some(previous) = entry.cancel.take() {
            let _ = previous.send(());
        }
        entry.key = Some(file_key.to_string());
        entry.cancel = cancel;
        Ok(())
    }

    async fn cancel_key(&self, file_key: &str) -> bool {
        let mut active = self.active_sources.lock().await;
        let Some(entry) = active
            .values_mut()
            .find(|entry| entry.key.as_deref() == Some(file_key))
        else {
            return false;
        };
        if let Some(cancel) = entry.cancel.take() {
            let _ = cancel.send(());
        }
        entry.key = None;
        true
    }
}

fn cached_window_key(root: &Path, stem: &str) -> Option<String> {
    let prefix = format!("{stem}_job_");
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with(&prefix)
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && std::fs::metadata(entry.path().join("output.mp4")).is_ok_and(|m| m.len() > 0)
            {
                Some(name)
            } else {
                None
            }
        })
}

#[tauri::command]
pub async fn cmd_prepare_fmp4_stream(
    message_id: i32,
    folder_id: Option<i64>,
    audio_stream_index: Option<i32>,
    start_time_secs: Option<f64>,
    stream_config: tauri::State<'_, StreamConfig>,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
    remux_state: tauri::State<'_, Fmp4RemuxState>,
) -> Result<Fmp4StreamInfo, String> {
    if audio_stream_index.is_some_and(|index| index < 0) {
        return Err("Invalid audio stream index".to_string());
    }
    let requested = start_time_secs.unwrap_or(0.0);
    if !requested.is_finite() || requested < 0.0 || requested > u64::MAX as f64 / 1000.0 {
        return Err("Invalid video seek position".to_string());
    }
    let folder_id = folder_id.unwrap_or(0);
    let source_key = format!("{folder_id}_{message_id}");
    // Reserve before awaiting the probe, so an older seek cannot replace a newer one.
    let generation = remux_state.reserve_source(&source_key).await;
    let input_url = format!(
        "http://localhost:{}/stream/{}/{}?token={}",
        stream_config.port,
        if folder_id == 0 {
            "me".to_string()
        } else {
            folder_id.to_string()
        },
        message_id,
        urlencoding::encode(&stream_config.token),
    );
    let anchor = if requested > 0.0 {
        Some(probe_seek_anchor(Path::new("ffprobe"), &input_url, requested).await?)
    } else {
        None
    };
    let window_start = anchor.map(|anchor| anchor.window_start).unwrap_or(0.0);
    let audio_key = audio_stream_index
        .map(|index| index.to_string())
        .unwrap_or_else(|| "default".to_string());
    let stem = format!(
        "{}_{}_a{}_s{}_seek1_aac",
        folder_id,
        message_id,
        audio_key,
        (window_start * 1000.0).round() as u64
    );
    let root = manager.cache_root.join(FMP4_DIR);
    if let Some(file_key) = cached_window_key(&root, &stem) {
        remux_state
            .install_job(&source_key, generation, &file_key, None)
            .await?;
        return Ok(Fmp4StreamInfo {
            url: format!("/fmp4/{file_key}/output.mp4"),
            output_file_key: file_key,
            status: "ready".to_string(),
            window_start_secs: window_start,
        });
    }
    let ffmpeg_path = manager
        .ffmpeg_path
        .lock()
        .await
        .clone()
        .ok_or("FFmpeg is not available. Install FFmpeg to enable fMP4 streaming.")?;
    // Separate partial files prevent cancellation cleanup from removing a replacement job.
    let file_key = format!("{stem}_job_{:016x}", rand::random::<u64>());
    let output_dir = root.join(&file_key);
    let output_path = output_dir.join("output.mp4");
    let partial_path = output_dir.join("output.mp4.part");
    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    remux_state
        .install_job(&source_key, generation, &file_key, Some(cancel_tx))
        .await?;
    remux_state.jobs.lock().await.insert(file_key.clone(), None);
    let remux_state = remux_state.inner().clone();
    let job_key = file_key.clone();
    tokio::spawn(async move {
        let result: Result<(), String> = async {
            run_fmp4_remux_at(
                &ffmpeg_path,
                Path::new(&input_url),
                &partial_path,
                audio_stream_index,
                anchor,
                &mut cancel_rx,
                |_| {},
            )
            .await?;
            if !has_playable_fragment(&partial_path) {
                return Err("Remux output contains no complete video fragment".to_string());
            }
            std::fs::rename(&partial_path, &output_path)
                .map_err(|error| format!("Failed to finish video remux: {error}"))?;
            Ok(())
        }
        .await;
        let mut jobs = remux_state.jobs.lock().await;
        match result {
            Ok(()) => {
                jobs.remove(&job_key);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&partial_path);
                jobs.insert(job_key, Some(error));
            }
        }
    });
    Ok(Fmp4StreamInfo {
        url: format!("/fmp4/{file_key}/output.mp4"),
        output_file_key: file_key,
        status: "processing".to_string(),
        window_start_secs: window_start,
    })
}

/// Stop only the requested remux window; stale cleanup cannot cancel its replacement.
#[tauri::command]
pub async fn cmd_cancel_fmp4_stream(
    file_key: String,
    remux_state: tauri::State<'_, Fmp4RemuxState>,
) -> Result<bool, String> {
    Ok(remux_state.cancel_key(&file_key).await)
}

fn playable_output_status(output_dir: &Path, allow_partial: bool) -> Option<Fmp4StatusResult> {
    if std::fs::metadata(output_dir.join("output.mp4")).is_ok_and(|metadata| metadata.len() > 0) {
        return Some(Fmp4StatusResult {
            status: "ready".to_string(),
            error: None,
            complete: true,
        });
    }
    if allow_partial && has_playable_fragment(&output_dir.join("output.mp4.part")) {
        return Some(Fmp4StatusResult {
            status: "ready".to_string(),
            error: None,
            complete: false,
        });
    }
    None
}

/// Poll the status of an fMP4 remux job.
#[tauri::command]
pub async fn cmd_get_fmp4_status(
    file_key: String,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
    remux_state: tauri::State<'_, Fmp4RemuxState>,
) -> Result<Fmp4StatusResult, String> {
    let output_dir = manager.cache_root.join(FMP4_DIR).join(&file_key);
    if let Some(status) = playable_output_status(&output_dir, false) {
        return Ok(status);
    }
    let jobs = remux_state.jobs.lock().await;
    match jobs.get(&file_key) {
        Some(None) => Ok(
            playable_output_status(&output_dir, true).unwrap_or(Fmp4StatusResult {
                status: "processing".to_string(),
                error: None,
                complete: false,
            }),
        ),
        Some(Some(error)) => Ok(Fmp4StatusResult {
            status: "error".to_string(),
            error: Some(error.clone()),
            complete: false,
        }),
        None => Ok(Fmp4StatusResult {
            status: "not_found".to_string(),
            error: None,
            complete: false,
        }),
    }
}

// ── Actix Serving Route ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct Fmp4Query {
    token: Option<String>,
}

/// GET /fmp4/{file_key}/output.mp4
///
/// Serves a pre-remuxed fragmented MP4 file. Token validation matches the
/// existing streaming server pattern.
#[actix_web::get("/fmp4/{file_key}/output.mp4")]
async fn serve_fmp4(
    _req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<Fmp4Query>,
    manager: web::Data<std::sync::Arc<TranscodeManager>>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    let file_key = path.into_inner();

    // Validate token
    match &query.token {
        Some(t) if t == &token_data.token => {}
        _ => return HttpResponse::Forbidden().body("Invalid or missing stream token"),
    }

    // Sanitize file_key to prevent path traversal
    if file_key
        .chars()
        .any(|c| !c.is_alphanumeric() && c != '_' && c != '-')
    {
        return HttpResponse::BadRequest().body("Invalid file key");
    }

    // Build path and validate it stays within the cache root
    let fmp4_root = manager.cache_root.join(FMP4_DIR);
    let finished = fmp4_root.join(&file_key).join("output.mp4");
    let partial = fmp4_root.join(&file_key).join("output.mp4.part");
    let file_path = if finished.exists() {
        &finished
    } else {
        &partial
    };

    // Keep the existing canonical-path containment check for both files.
    let safe_path = match file_path
        .canonicalize()
        .or_else(|_| finished.canonicalize())
    {
        Ok(path) => path,
        Err(_) => return HttpResponse::NotFound().body("File not found"),
    };

    let safe_root = fmp4_root
        .canonicalize()
        .unwrap_or_else(|_| fmp4_root.clone());
    if !safe_path.starts_with(&safe_root) {
        log::error!(
            "fMP4 path traversal attempt: {:?} not under {:?}",
            safe_path,
            safe_root
        );
        return HttpResponse::Forbidden().body("Access denied");
    }

    if safe_path
        .file_name()
        .is_some_and(|name| name == "output.mp4.part")
    {
        if !has_playable_fragment(&safe_path) && !finished.exists() {
            return HttpResponse::ServiceUnavailable()
                .insert_header(("Retry-After", "1"))
                .body("Video fragment is not ready");
        }
        // The final length is unknown while FFmpeg writes. Send a growing,
        // chunked response, then use NamedFile ranges once atomically finished.
        return HttpResponse::Ok()
            .insert_header(("Content-Type", "video/mp4"))
            .insert_header(("Cache-Control", "no-store"))
            .insert_header(("Accept-Ranges", "none"))
            .streaming(growing_mp4(
                safe_path,
                finished,
                std::time::Duration::from_secs(120),
            ));
    }

    // Use NamedFile for automatic Range/Content-Range support and
    // streaming from disk (no full-file memory load).
    match actix_files::NamedFile::open_async(&safe_path).await {
        Ok(f) => f
            .set_content_type("video/mp4".parse().unwrap())
            .into_response(&_req),
        Err(e) => {
            log::error!("Failed to open fMP4 file {:?}: {}", safe_path, e);
            HttpResponse::InternalServerError().body("Failed to read file")
        }
    }
}

#[cfg(test)]
mod streaming_readiness_tests {
    use super::*;
    use futures::StreamExt;

    pub(super) struct Fixture(pub(super) std::path::PathBuf);
    impl Fixture {
        pub(super) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "terarelay-fmp4-test-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn header_only_or_partial_fragment_is_not_ready() {
        let fixture = Fixture::new();
        let path = fixture.0.join("output.mp4.part");
        let mut bytes = atom(b"ftyp", b"isom");
        bytes.extend(atom(b"moov", b"init"));
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            !has_playable_fragment(&path),
            "Header-only output is not a playable video"
        );
        bytes.extend(atom(b"moof", b"fragment"));
        let mut data = atom(b"mdat", b"video");
        bytes.extend_from_slice(&data[..data.len() - 1]);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            !has_playable_fragment(&path),
            "Partial media data must not be published"
        );
        bytes.push(data.pop().unwrap());
        std::fs::write(&path, bytes).unwrap();
        assert!(has_playable_fragment(&path));
    }

    #[tokio::test]
    async fn growing_stream_waits_for_more_bytes_and_finishes_after_atomic_rename() {
        let fixture = Fixture::new();
        let partial = fixture.0.join("output.mp4.part");
        let finished = fixture.0.join("output.mp4");
        std::fs::write(&partial, b"first").unwrap();
        let stream = growing_mp4(
            partial.clone(),
            finished.clone(),
            std::time::Duration::from_secs(2),
        );
        futures::pin_mut!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"first");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .unwrap()
            .write_all(b"second")
            .unwrap();
        std::fs::rename(&partial, &finished).unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"second");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn final_bytes_written_between_eof_and_completion_check_are_drained() {
        let fixture = Fixture::new();
        let partial = fixture.0.join("output.mp4.part");
        let finished = fixture.0.join("output.mp4");
        std::fs::write(&partial, b"first").unwrap();
        let mut completed = false;
        let stream = growing_mp4_with_eof(
            partial.clone(),
            finished.clone(),
            std::time::Duration::from_secs(2),
            move |before_partial| {
                if completed || before_partial {
                    return;
                }
                completed = true;
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&partial)
                    .unwrap()
                    .write_all(b"tail")
                    .unwrap();
                std::fs::rename(&partial, &finished).unwrap();
            },
        );
        futures::pin_mut!(stream);
        let mut received = Vec::new();
        while let Some(chunk) = stream.next().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            received, b"firsttail",
            "Atomic completion lost the bytes written after the previous EOF"
        );
    }

    #[tokio::test]
    async fn rename_between_existence_checks_is_completion_not_failure() {
        let fixture = Fixture::new();
        let partial = fixture.0.join("output.mp4.part");
        let finished = fixture.0.join("output.mp4");
        std::fs::write(&partial, b"first").unwrap();
        let mut completed = false;
        let stream = growing_mp4_with_eof(
            partial.clone(),
            finished.clone(),
            std::time::Duration::from_secs(2),
            move |before_partial| {
                if completed || !before_partial {
                    return;
                }
                completed = true;
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&partial)
                    .unwrap()
                    .write_all(b"tail")
                    .unwrap();
                std::fs::rename(&partial, &finished).unwrap();
            },
        );
        futures::pin_mut!(stream);
        let mut received = Vec::new();
        while let Some(chunk) = stream.next().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, b"firsttail");
    }

    #[test]
    fn early_playback_and_finished_seekable_output_have_distinct_status() {
        let fixture = Fixture::new();
        let partial = fixture.0.join("output.mp4.part");
        let mut bytes = atom(b"ftyp", b"isom");
        bytes.extend(atom(b"moov", b"init"));
        std::fs::write(&partial, &bytes).unwrap();
        assert!(playable_output_status(&fixture.0, true).is_none());
        bytes.extend(atom(b"moof", b"fragment"));
        bytes.extend(atom(b"mdat", b"video"));
        std::fs::write(&partial, bytes).unwrap();
        assert!(!playable_output_status(&fixture.0, true).unwrap().complete);
        assert!(
            playable_output_status(&fixture.0, false).is_none(),
            "An orphan partial is not an active stream"
        );
        std::fs::rename(partial, fixture.0.join("output.mp4")).unwrap();
        let finished = playable_output_status(&fixture.0, false).unwrap();
        assert!(finished.complete);
        assert_eq!(finished.status, "ready");
    }

    #[tokio::test]
    async fn failed_remux_does_not_leave_a_successful_truncated_stream() {
        let fixture = Fixture::new();
        let partial = fixture.0.join("output.mp4.part");
        std::fs::write(&partial, b"first").unwrap();
        let stream = growing_mp4(
            partial.clone(),
            fixture.0.join("output.mp4"),
            std::time::Duration::from_secs(2),
        );
        futures::pin_mut!(stream);
        assert!(stream.next().await.unwrap().is_ok());
        std::fs::remove_file(partial).unwrap();
        assert!(stream.next().await.unwrap().is_err());
    }
}

/// Register fMP4 routes on the Actix service config.
pub fn configure_fmp4_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(serve_fmp4);
}

#[cfg(test)]
mod audio_compatibility_tests {
    use super::*;

    #[tokio::test]
    async fn failed_http_remux_reports_the_decoder_error_without_the_source_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = std::env::temp_dir().join(format!(
            "terarelay-invalid-http-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("output.mp4.part");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/bad?token=qa-private-test",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nnot a video file").await.unwrap();
        });
        let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel();
        let error = run_fmp4_remux(
            Path::new("ffmpeg"),
            Path::new(&url),
            &output,
            None,
            &mut cancel_rx,
            |_| {},
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            error.contains("Invalid data"),
            "Decoder failure was reduced to an opaque exit code: {error}"
        );
        assert!(
            !error.contains("qa-private-test"),
            "Stream token leaked into the playback error"
        );
    }

    #[tokio::test]
    async fn stalled_http_input_fails_instead_of_waiting_indefinitely() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-stalled-http-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("output.mp4.part");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stalled", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(24),
            run_fmp4_remux(
                Path::new("ffmpeg"),
                Path::new(&url),
                &output,
                None,
                &mut cancel_rx,
                |_| {},
            ),
        )
        .await;
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            result.is_ok(),
            "HTTP video input remained stalled with no timeout"
        );
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn eac3_surround_audio_becomes_aac_without_changing_the_video_codec() {
        let ffmpeg = Path::new("ffmpeg");
        if tokio::process::Command::new(ffmpeg)
            .arg("-version")
            .output()
            .await
            .is_err()
        {
            eprintln!("SKIP audio compatibility fixture: FFmpeg unavailable");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("terarelay-eac3-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("surround.mkv");
        let output = root.join("playback.mp4");
        let generated = tokio::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=24",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=5.1:sample_rate=48000",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "24",
                "-c:a",
                "eac3",
            ])
            .arg(&source)
            .output()
            .await
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel();
        run_fmp4_remux(ffmpeg, &source, &output, None, &mut cancel_rx, |_| {})
            .await
            .unwrap();
        let probe = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name,channels",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = info["streams"].as_array().unwrap();
        let video = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
        let audio = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
        let video_codec = video["codec_name"].as_str().unwrap().to_string();
        let audio_codec = audio["codec_name"].as_str().unwrap().to_string();
        let audio_channels = audio["channels"].as_u64().unwrap();
        let mut packet_hashes = Vec::new();
        for path in [&source, &output] {
            let probe = tokio::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_packets",
                    "-show_data_hash",
                    "sha256",
                    "-show_entries",
                    "packet=data_hash",
                    "-of",
                    "json",
                ])
                .arg(path)
                .output()
                .await
                .unwrap();
            let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            packet_hashes.push(info["packets"].clone());
        }
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            packet_hashes[0], packet_hashes[1],
            "Video packets were re-encoded"
        );
        assert_eq!(
            audio_channels, 2,
            "Fallback did not produce browser-compatible stereo"
        );
        assert_eq!(video_codec, "h264", "Remux changed the video codec");
        assert_eq!(
            audio_codec, "aac",
            "EAC3 was copied into the browser fallback instead of compatible AAC"
        );
    }
}

#[cfg(test)]
mod seek_playback_tests {
    use super::*;
    use crate::server::parse_range_header;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    mod logical {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/logical_stream.rs"
        ));
    }

    #[tokio::test]
    async fn superseded_probe_and_stale_cancellation_cannot_replace_the_latest_window() {
        let state = Fmp4RemuxState::new();
        let first = state.reserve_source("movie").await;
        let latest = state.reserve_source("movie").await;
        let (old_tx, mut old_rx) = tokio::sync::oneshot::channel();
        assert!(state
            .install_job("movie", first, "old", Some(old_tx))
            .await
            .is_err());
        assert!(old_rx.try_recv().is_err());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        state
            .install_job("movie", latest, "latest", Some(tx))
            .await
            .unwrap();
        assert!(!state.cancel_key("old").await);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.cancel_key("latest").await);
        rx.await.unwrap();
    }

    #[derive(Clone)]
    struct MultipartFixture {
        paths: Vec<std::path::PathBuf>,
        sizes: Vec<u64>,
        sent: Arc<AtomicU64>,
        part_two_media: Arc<AtomicBool>,
    }
    async fn fixture_response(req: HttpRequest, data: web::Data<MultipartFixture>) -> HttpResponse {
        let data = data.get_ref().clone();
        logical::range_response(
            &data.sizes.clone(),
            "video/x-matroska",
            &req,
            move |ranges| {
                async_stream::stream! {
                    use tokio::io::{AsyncReadExt, AsyncSeekExt};
                    for range in ranges {
                        if range.index == 1 && range.start + 65536 < data.sizes[1] {
                            data.part_two_media.store(true, Ordering::Relaxed);
                        }
                        let mut file = tokio::fs::File::open(&data.paths[range.index]).await.unwrap();
                        file.seek(std::io::SeekFrom::Start(range.start)).await.unwrap();
                        let mut remaining = range.length;
                        let mut buffer = vec![0u8; 65536];
                        while remaining > 0 {
                            let length = remaining.min(buffer.len() as u64) as usize;
                            let read = file.read(&mut buffer[..length]).await.unwrap();
                            if read == 0 { break; }
                            remaining -= read as u64;
                            data.sent.fetch_add(read as u64, Ordering::Relaxed);
                            yield Ok(web::Bytes::copy_from_slice(&buffer[..read]));
                            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        }
                    }
                }
            },
        )
    }
    async fn probe(path: &Path, options: &[&str]) -> serde_json::Value {
        let output = tokio::process::Command::new("ffprobe")
            .args(["-v", "error"])
            .args(options)
            .args(["-of", "json"])
            .arg(path)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn complete_prefix(bytes: &[u8]) -> &[u8] {
        let mut end = 0;
        while end + 8 <= bytes.len() {
            let short = u32::from_be_bytes(bytes[end..end + 4].try_into().unwrap());
            let size = if short == 1 {
                if end + 16 > bytes.len() {
                    break;
                }
                u64::from_be_bytes(bytes[end + 8..end + 16].try_into().unwrap()) as usize
            } else {
                short as usize
            };
            if size < 8 || end + size > bytes.len() {
                break;
            }
            end += size;
        }
        &bytes[..end]
    }

    #[actix_web::test]
    async fn multipart_seek_produces_playable_selected_audio_before_full_remux_and_cancels() {
        let fixture = streaming_readiness_tests::Fixture::new();
        let generated = fixture.0.join("source.mkv");
        let supplied = std::env::var_os("TERA_QA_MULTIPART_SOURCE").map(std::path::PathBuf::from);
        let source = supplied.as_deref().unwrap_or(&generated);
        let large = supplied.is_some();
        if !large {
            let output = tokio::process::Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x90:rate=2",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=16000",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=880:sample_rate=16000",
                    // Keep startup metadata reads small relative to the movie;
                    // a two-minute 1.8 MB fixture is smaller than the probe budget.
                    "-t",
                    "600",
                    "-map",
                    "0:v:0",
                    "-map",
                    "1:a:0",
                    "-map",
                    "2:a:0",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "16",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "32k",
                    "-metadata:s:a:1",
                    "language=tel",
                ])
                .arg(source)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let bytes = std::fs::read(source).unwrap();
        let cut = bytes.len() / 2;
        let first = fixture.0.join("movie.tgdpart001-002");
        let second = fixture.0.join("movie.tgdpart002-002");
        std::fs::write(&first, &bytes[..cut]).unwrap();
        std::fs::write(&second, &bytes[cut..]).unwrap();
        let sent = Arc::new(AtomicU64::new(0));
        let part_two = Arc::new(AtomicBool::new(false));
        let data = MultipartFixture {
            paths: vec![first, second],
            sizes: vec![cut as u64, (bytes.len() - cut) as u64],
            sent: sent.clone(),
            part_two_media: part_two.clone(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source_url = format!("http://{}/source?token=qa", listener.local_addr().unwrap());
        let server = actix_web::HttpServer::new(move || {
            actix_web::App::new()
                .app_data(web::Data::new(data.clone()))
                .route("/source", web::get().to(fixture_response))
        })
        .workers(1)
        .disable_signals()
        .listen(listener)
        .unwrap()
        .run();
        let handle = server.handle();
        let server_task = tokio::spawn(server);
        let response = reqwest::Client::new()
            .get(&source_url)
            .header("Range", format!("bytes={}-{}", cut - 16, cut + 15))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            &bytes[cut - 16..cut + 16]
        );
        part_two.store(false, Ordering::Relaxed);
        let requested = if large { 5403.25 } else { 400.25 };
        let anchor = probe_seek_anchor(Path::new("ffprobe"), &source_url, requested)
            .await
            .unwrap();
        let expected = if large { 5400.064 } else { 400.064 };
        assert!((anchor.window_start - expected).abs() < 0.001, "{anchor:?}");
        eprintln!(
            "QA multipart source bytes={}, probe bytes={}",
            bytes.len(),
            sent.load(Ordering::Relaxed)
        );
        let partial = fixture.0.join("output.mp4.part");
        let snapshot = fixture.0.join("playable.mp4");
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
        let output = partial.clone();
        let url = source_url.clone();
        let remux = tokio::spawn(async move {
            run_fmp4_remux_at(
                Path::new("ffmpeg"),
                Path::new(&url),
                &output,
                Some(2),
                Some(anchor),
                &mut cancel_rx,
                |_| {},
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !has_playable_fragment(&partial) {
                assert!(
                    !remux.is_finished(),
                    "Remux ended before publishing a fragment"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !remux.is_finished(),
            "Playback still waits for the entire remux"
        );
        let available = std::fs::read(&partial).unwrap();
        std::fs::write(&snapshot, complete_prefix(&available)).unwrap();
        assert!(
            part_two.load(Ordering::Relaxed),
            "Seek never reached the second physical file"
        );
        let transferred = sent.load(Ordering::Relaxed);
        assert!(
            transferred < bytes.len() as u64 / 2,
            "Seek downloaded the movie prefix: {transferred}"
        );
        cancel_tx.send(()).unwrap();
        assert_eq!(remux.await.unwrap().unwrap_err(), "Cancelled");
        assert!(
            !partial.exists(),
            "Cancelled remux left an incomplete output"
        );
        let info = probe(
            &snapshot,
            &[
                "-show_entries",
                "stream=codec_name,codec_type,channels:stream_tags=language",
            ],
        )
        .await;
        let streams = info["streams"].as_array().unwrap();
        assert_eq!(
            streams.iter().find(|s| s["codec_type"] == "video").unwrap()["codec_name"],
            "h264"
        );
        let audio = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
        assert_eq!(audio["codec_name"], "aac");
        assert_eq!(audio["channels"], 2);
        assert_eq!(audio["tags"]["language"], "tel");
        let source_packets = probe(
            source,
            &[
                "-read_intervals",
                &format!("{}%+#1", requested - 0.064),
                "-select_streams",
                "v:0",
                "-show_packets",
                "-show_data_hash",
                "sha256",
                "-show_entries",
                "packet=data_hash",
            ],
        )
        .await;
        let output_packets = probe(
            &snapshot,
            &[
                "-read_intervals",
                "%+#1",
                "-select_streams",
                "v:0",
                "-show_packets",
                "-show_data_hash",
                "sha256",
                "-show_entries",
                "packet=data_hash",
            ],
        )
        .await;
        assert_eq!(
            source_packets["packets"][0]["data_hash"],
            output_packets["packets"][0]["data_hash"]
        );
        println!("PASS multipart movie seek at {requested}s; first fragment after {transferred} / {} bytes", bytes.len());
        handle.stop(true).await;
        server_task.await.unwrap().unwrap();
    }
}
