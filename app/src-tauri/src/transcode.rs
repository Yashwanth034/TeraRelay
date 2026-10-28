// ── Transcode Module ────────────────────────────────────────────────────
// Handles: FFmpeg detection, original source caching, HLS transcode jobs,
// and serving HLS playlists/segments via Actix routes.
//
// Cache layout:
//   $APPDATA/streaming/
//     originals/{folder_id}_{message_id}.mp4
//     hls/{folder_id}_{message_id}/
//       360p/index.m3u8 + segment_000.ts ...
//       480p/...
//       720p/...
//       1080p/...

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use actix_web::{web, HttpRequest, HttpResponse, Responder};
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;

use crate::commands::TelegramState;
use crate::mp4_utils;
use grammers_client::types::Media;
use tauri::Manager;

// ── Constants ───────────────────────────────────────────────────────────

/// Maximum total cache size in bytes (5 GB).
pub const MAX_CACHE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Subdirectory name for the streaming cache inside app_data_dir.
/// Originals subdirectory.
const ORIGINALS_DIR: &str = "originals";

/// HLS output subdirectory.
const HLS_DIR: &str = "hls";

/// HLS segment duration in seconds.
const HLS_SEGMENT_TIME: u32 = 4;

// ── Quality presets ─────────────────────────────────────────────────────

#[derive(Clone)]
pub struct QualityPreset {
    pub label: &'static str,
    pub height: u32,
    pub scale_filter: &'static str,
    pub video_bitrate_k: u32,
    pub audio_bitrate_k: u32,
}

pub const QUALITY_PRESETS: &[QualityPreset] = &[
    QualityPreset {
        label: "360p",
        height: 360,
        scale_filter: "scale=-2:360",
        video_bitrate_k: 800,
        audio_bitrate_k: 96,
    },
    QualityPreset {
        label: "480p",
        height: 480,
        scale_filter: "scale=-2:480",
        video_bitrate_k: 1400,
        audio_bitrate_k: 128,
    },
    QualityPreset {
        label: "720p",
        height: 720,
        scale_filter: "scale=-2:720",
        video_bitrate_k: 2800,
        audio_bitrate_k: 128,
    },
    QualityPreset {
        label: "1080p",
        height: 1080,
        scale_filter: "scale=-2:1080",
        video_bitrate_k: 5000,
        audio_bitrate_k: 160,
    },
];

// ── Types ───────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct TranscodeCapabilities {
    pub available: bool,
    pub variants: Vec<QualityVariant>,
    pub mode: String,
}

#[derive(serde::Serialize, Clone)]
pub struct QualityVariant {
    pub label: String,
    pub height: u32,
    pub available: bool,
}

#[derive(serde::Serialize, Clone)]
pub struct TranscodePrepareResult {
    pub job_id: String,
    pub status: String,
    pub progress: f32,
    pub playlist_url: Option<String>,
}

#[derive(serde::Serialize, Clone)]
pub struct TranscodeStatusResult {
    pub job_id: String,
    pub status: String,
    pub progress: f32,
    pub error: Option<String>,
    pub playlist_url: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TranscodeKey {
    pub folder_id: i64, // 0 = root/me
    pub message_id: i32,
    pub quality: String,
}

impl TranscodeKey {
    pub fn file_key(&self) -> String {
        format!("{}_{}", self.folder_id, self.message_id)
    }

    pub fn job_id(&self) -> String {
        format!("{}_{}_{}", self.folder_id, self.message_id, self.quality)
    }
}

#[derive(Clone, Debug)]
pub enum JobPhase {
    NotStarted,
    CachingOriginal { progress: f32 },
    Transcoding { progress: f32 },
    Ready,
    Error(String),
    Cancelled,
}

pub struct TranscodeJob {
    pub key: TranscodeKey,
    pub phase: JobPhase,
    /// True from preparation reservation through worker teardown. A
    /// terminal/cancelled job must never restart while its old FFmpeg task
    /// can still touch the same deterministic output path.
    pub in_flight: bool,
    pub cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub last_access: Instant,
    pub source_height: Option<u32>,
}

/// Progress notifications may complete out of order after cancellation,
/// an error or a successful FFmpeg exit. Ignore phase mismatches and
/// nonmonotonic updates so terminal states cannot become stuck in caching.
fn record_job_progress(job: &mut TranscodeJob, phase: JobPhase) {
    if !job.in_flight {
        return;
    }
    let allowed = match (&job.phase, &phase) {
        (
            JobPhase::CachingOriginal { progress: old },
            JobPhase::CachingOriginal { progress: new },
        )
        | (JobPhase::Transcoding { progress: old }, JobPhase::Transcoding { progress: new }) => {
            new.is_finite() && *new >= *old && *new <= 1.0
        }
        _ => false,
    };
    if allowed {
        job.phase = phase;
    }
}

// ── TranscodeManager ────────────────────────────────────────────────────

/// A playlist can appear while FFmpeg is still writing HLS segments. Only a
/// finished VOD playlist with an end marker and every referenced segment
/// present is eligible for the persistent ready-cache fast path. This is
/// especially important after cancellation or a crash during transcoding.
fn finished_hls_variant(output_dir: &Path) -> bool {
    let path = output_dir.join("index.m3u8");
    if std::fs::metadata(&path).map_or(true, |meta| meta.len() > 4 * 1024 * 1024) {
        return false;
    }
    let Ok(playlist) = std::fs::read_to_string(path) else {
        return false;
    };
    if !playlist.lines().any(|line| line.trim() == "#EXT-X-ENDLIST")
        || !playlist
            .lines()
            .any(|line| line.trim().starts_with("#EXTINF:"))
    {
        return false;
    }
    let mut segments = 0usize;
    for segment in playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        // Reject absolute paths, traversal and external segment URLs.
        if !segment.starts_with("segment_")
            || !segment.ends_with(".ts")
            || !segment
                .strip_prefix("segment_")
                .and_then(|name| name.strip_suffix(".ts"))
                .is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                })
        {
            return false;
        }
        let Ok(metadata) = std::fs::metadata(output_dir.join(segment)) else {
            return false;
        };
        if !metadata.is_file() || metadata.len() == 0 {
            return false;
        }
        segments += 1;
    }
    segments > 0
}

#[derive(Clone)]
pub struct TranscodeManager {
    pub cache_root: PathBuf,
    pub ffmpeg_path: Arc<Mutex<Option<PathBuf>>>,
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<TranscodeJob>>>>>,
    // Different HLS qualities share the same original MP4 on disk. Allow
    // only one cache download per file while other qualities await it.
    original_download_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    max_cache_bytes: Arc<Mutex<u64>>,
}

impl TranscodeManager {
    pub fn new(cache_root: PathBuf) -> Self {
        // Ensure subdirectories exist
        let _ = std::fs::create_dir_all(cache_root.join(ORIGINALS_DIR));
        let _ = std::fs::create_dir_all(cache_root.join(HLS_DIR));

        // Clean up partial output from previous sessions
        Self::clean_partial_outputs(&cache_root);

        Self {
            cache_root,
            ffmpeg_path: Arc::new(Mutex::new(None)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            original_download_locks: Arc::new(Mutex::new(HashMap::new())),
            max_cache_bytes: Arc::new(Mutex::new(MAX_CACHE_BYTES)),
        }
    }

    /// Clean up incomplete HLS directories from previous runs.
    fn clean_partial_outputs(cache_root: &Path) {
        let hls_root = cache_root.join(HLS_DIR);
        if let Ok(entries) = std::fs::read_dir(&hls_root) {
            for entry in entries.flatten() {
                let file_dir = entry.path();
                if !file_dir.is_dir() {
                    continue;
                }
                // An old FFmpeg run may have produced an index.m3u8 before
                // exiting. Validate *each* quality independently, preserving
                // finished siblings while removing only interrupted variants.
                if let Ok(variants) = std::fs::read_dir(&file_dir) {
                    for variant in variants.flatten() {
                        let quality_dir = variant.path();
                        if quality_dir.is_dir() && !finished_hls_variant(&quality_dir) {
                            log::info!(
                                "Transcode: Cleaning up incomplete HLS variant: {:?}",
                                quality_dir
                            );
                            let _ = std::fs::remove_dir_all(&quality_dir);
                        }
                    }
                }
                if std::fs::read_dir(&file_dir)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_dir(&file_dir);
                }
            }
        }
        // Also clean incomplete original downloads (zero-size or .part files)
        let orig_root = cache_root.join(ORIGINALS_DIR);
        if let Ok(entries) = std::fs::read_dir(&orig_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let should_remove = if path.extension().and_then(|e| e.to_str()) == Some("part")
                    {
                        true // Orphaned partial download
                    } else if let Ok(meta) = std::fs::metadata(&path) {
                        meta.len() == 0 // Zero-size completed file
                    } else {
                        false
                    };
                    if should_remove {
                        log::info!("Transcode: Removing incomplete file: {:?}", path);
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }

    /// Returns the single serializing gate shared across quality variants
    /// of the same Telegram media. Weak entries avoid retaining one mutex
    /// per movie for the entire application lifetime.
    async fn original_download_lock(&self, file_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.original_download_locks.lock().await;
        if let Some(gate) = locks.get(file_key).and_then(Weak::upgrade) {
            return gate;
        }
        if locks.len() >= 128 {
            locks.retain(|_, weak| weak.strong_count() > 0);
        }
        let gate = Arc::new(Mutex::new(()));
        locks.insert(file_key.to_string(), Arc::downgrade(&gate));
        gate
    }

    /// Get or create a job entry. Returns (job_arc, is_new).
    pub async fn get_or_create_job(&self, key: &TranscodeKey) -> (Arc<Mutex<TranscodeJob>>, bool) {
        let mut jobs = self.jobs.lock().await;
        let job_id = key.job_id();
        if let Some(job) = jobs.get(&job_id) {
            let mut existing = job.lock().await;
            let missing_ready_output = matches!(existing.phase, JobPhase::Ready)
                && !finished_hls_variant(&self.hls_output_dir(&key.file_key(), &key.quality));
            let can_restart = !existing.in_flight
                && (matches!(existing.phase, JobPhase::Cancelled | JobPhase::Error(_))
                    || missing_ready_output);
            if !can_restart {
                existing.last_access = Instant::now();
                drop(existing);
                return (job.clone(), false);
            }
            // Allocate a separate job object after the old worker has
            // finished. Late progress callbacks still holding the retired
            // Arc cannot mutate the new attempt's phase.
            drop(existing);
        }
        let job = Arc::new(Mutex::new(TranscodeJob {
            key: key.clone(),
            phase: JobPhase::NotStarted,
            in_flight: true,
            cancel_tx: None,
            last_access: Instant::now(),
            source_height: None,
        }));
        jobs.insert(job_id, job.clone());
        (job, true)
    }

    /// Remove a job from the map.
    pub async fn remove_job(&self, job_id: &str) {
        let mut jobs = self.jobs.lock().await;
        jobs.remove(job_id);
    }

    /// Snapshot a job's current phase without pinning the global HLS job map
    /// while waiting for that job's mutex or inspecting its output files.
    /// This lets unrelated video qualities register/poll independently.
    async fn get_job_phase_snapshot(&self, job_id: &str) -> Option<(TranscodeKey, JobPhase)> {
        let job_arc = {
            let jobs = self.jobs.lock().await;
            jobs.get(job_id).cloned()
        }?;
        let job = job_arc.lock().await;
        Some((job.key.clone(), job.phase.clone()))
    }

    /// Get a clone of the jobs map for status queries.
    pub async fn get_job_snapshot(&self) -> HashMap<String, Arc<Mutex<TranscodeJob>>> {
        self.jobs.lock().await.clone()
    }

    /// Get current max cache bytes.
    pub async fn get_max_cache_bytes(&self) -> u64 {
        *self.max_cache_bytes.lock().await
    }

    /// Set max cache bytes.
    pub async fn set_max_cache_bytes(&self, bytes: u64) {
        *self.max_cache_bytes.lock().await = bytes;
    }

    /// Get total cache size by walking the cache directory.
    pub fn total_cache_size(&self) -> u64 {
        let mut total: u64 = 0;
        let walker = walkdir::WalkDir::new(&self.cache_root).min_depth(1);
        for entry in walker.into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// Evict oldest files until cache is under the limit.
    /// Never evict files that belong to active jobs.
    pub async fn evict_lru(&self) {
        let max = *self.max_cache_bytes.lock().await;
        let current = self.total_cache_size();
        if current <= max {
            return;
        }

        // Eviction may run when a different transcode finishes. Never delete
        // the original input or HLS output from *any* still-active job,
        // including a worker that is cancelling but has not exited.
        // Hold the registry lock through eviction: a new job cannot reserve
        // a just-deleted original or variant between our active-job snapshot
        // and the filesystem removal.
        let jobs_guard = self.jobs.lock().await;
        let mut active_originals = HashSet::new();
        let mut active_variants = Vec::new();
        for job_arc in jobs_guard.values() {
            let job = job_arc.lock().await;
            if job.in_flight {
                active_originals.insert(self.original_path(&job.key.file_key()));
                active_variants.push(self.hls_output_dir(&job.key.file_key(), &job.key.quality));
            }
        }

        // Collect all files with their modification times
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let walker = walkdir::WalkDir::new(&self.cache_root).min_depth(1);
        for entry in walker.into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Ok(meta) = entry.metadata() {
                    files.push((
                        entry.path().to_path_buf(),
                        meta.len(),
                        meta.modified().unwrap_or(UNIX_EPOCH),
                    ));
                }
            }
        }

        // Sort by modification time (oldest first)
        files.sort_by_key(|(_, _, mtime)| *mtime);

        let mut freed: u64 = 0;
        let target = current.saturating_sub(max);

        for (path, size, _) in &files {
            if freed >= target {
                break;
            }
            if active_originals.contains(path)
                || active_variants
                    .iter()
                    .any(|folder| path.starts_with(folder))
            {
                continue;
            }

            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("Transcode: Failed to evict {:?}: {}", path, e);
            } else {
                freed += size;
                log::debug!("Transcode: Evicted {:?} ({} bytes)", path, size);
            }
        }

        // Clean up empty directories
        Self::clean_empty_dirs(&self.cache_root, 2);

        log::info!(
            "Transcode: LRU eviction complete. Freed {} bytes, target was {} bytes",
            freed,
            target
        );
    }

    fn clean_empty_dirs(path: &Path, depth: usize) {
        if depth == 0 {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    Self::clean_empty_dirs(&p, depth - 1);
                    if std::fs::read_dir(&p)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(&p);
                    }
                }
            }
        }
    }

    /// Validate that a resolved path stays within the HLS cache directory.
    pub fn validate_hls_path(
        &self,
        file_key: &str,
        quality: &str,
        segment: Option<&str>,
    ) -> Option<PathBuf> {
        // Sanitize inputs — only allow alphanumeric, underscores, hyphens, dots
        if file_key
            .chars()
            .any(|c| !c.is_alphanumeric() && c != '_' && c != '-')
        {
            return None;
        }
        if quality.chars().any(|c| !c.is_alphanumeric() && c != 'p') {
            return None;
        }

        let hls_root = self.cache_root.join(HLS_DIR);
        let mut resolved = hls_root.join(file_key).join(quality);

        if let Some(seg) = segment {
            // Only allow .ts and .m3u8 files
            if !seg.ends_with(".ts") && !seg.ends_with(".m3u8") {
                return None;
            }
            if seg.contains("..") || seg.contains('/') || seg.contains('\\') {
                return None;
            }
            resolved = resolved.join(seg);
        } else {
            resolved = resolved.join("index.m3u8");
        }

        // Canonicalize and verify it's within the HLS root
        match resolved.canonicalize() {
            Ok(canon) => {
                let hls_canon = hls_root.canonicalize().unwrap_or_else(|_| hls_root.clone());
                if canon.starts_with(&hls_canon) {
                    Some(canon)
                } else {
                    log::error!(
                        "Transcode: Path traversal attempt: {:?} not under {:?}",
                        canon,
                        hls_canon
                    );
                    None
                }
            }
            Err(_) => None, // File doesn't exist yet or path is invalid
        }
    }

    /// Return the path for a cached original file.
    pub fn original_path(&self, file_key: &str) -> PathBuf {
        self.cache_root
            .join(ORIGINALS_DIR)
            .join(format!("{}.mp4", file_key))
    }

    /// Return the HLS output directory for a job.
    pub fn hls_output_dir(&self, file_key: &str, quality: &str) -> PathBuf {
        self.cache_root.join(HLS_DIR).join(file_key).join(quality)
    }
}

// ── FFmpeg Detection ────────────────────────────────────────────────────

/// Detect FFmpeg availability. Tries sidecar first, then PATH.
pub async fn detect_ffmpeg(app_handle: &tauri::AppHandle) -> Option<PathBuf> {
    // 1. Try sidecar binary in the app's resource directory
    if let Ok(resource_dir) = app_handle.path().resource_dir() {
        #[cfg(target_os = "windows")]
        let sidecar_name = "ffmpeg.exe";
        #[cfg(not(target_os = "windows"))]
        let sidecar_name = "ffmpeg";

        let sidecar_path: PathBuf = resource_dir.join(sidecar_name);
        if sidecar_path.exists() {
            match test_ffmpeg(&sidecar_path).await {
                Ok(true) => {
                    log::info!("Transcode: Found FFmpeg sidecar at {:?}", sidecar_path);
                    return Some(sidecar_path);
                }
                Ok(false) => {
                    log::warn!(
                        "Transcode: FFmpeg sidecar at {:?} failed version check",
                        sidecar_path
                    );
                }
                Err(e) => {
                    log::warn!("Transcode: FFmpeg sidecar check error: {}", e);
                }
            }
        }
    }

    // 2. Fallback to PATH
    match test_ffmpeg(Path::new("ffmpeg")).await {
        Ok(true) => {
            log::info!("Transcode: Found FFmpeg on PATH");
            Some(PathBuf::from("ffmpeg"))
        }
        Ok(false) => {
            log::warn!("Transcode: FFmpeg not found on PATH or version check failed");
            None
        }
        Err(e) => {
            log::warn!("Transcode: FFmpeg not available on PATH: {}", e);
            None
        }
    }
}

async fn test_ffmpeg(path: &Path) -> Result<bool, String> {
    let output = tokio::process::Command::new(path)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run ffmpeg: {}", e))?;

    Ok(output.status.success())
}

// ── Source Cache (Phase 2) ──────────────────────────────────────────────

/// Download the original MP4 file from Telegram to local cache.
/// Returns the total file size on success.
pub async fn cache_original(
    client: &grammers_client::Client,
    media: &Media,
    dest_path: &Path,
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    progress_callback: impl Fn(f32),
) -> Result<u64, String> {
    let total_size = match media {
        Media::Document(d) => d.size() as u64,
        _ => return Err("Not a document".to_string()),
    };

    // Ensure parent directory exists
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create cache dir: {}", e))?;
    }

    let tmp_path = dest_path.with_extension("mp4.part");
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| format!("Failed to create cache file: {}", e))?;

    use tokio::io::AsyncWriteExt;

    let mut download_iter = client.iter_download(media);
    download_iter = download_iter.chunk_size(65536);
    let mut downloaded: u64 = 0;

    loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err("Cancelled".to_string());
            }
            result = download_iter.next() => {
                match result {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk).await.map_err(|e| format!("Write error: {}", e))?;
                        downloaded += chunk.len() as u64;
                        progress_callback(downloaded as f32 / total_size as f32);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return Err(format!("Download error: {}", e));
                    }
                }
            }
        }
    }

    file.flush()
        .await
        .map_err(|e| format!("Flush error: {}", e))?;
    drop(file);

    // Validate file size matches expected size
    let actual_size = tokio::fs::metadata(&tmp_path)
        .await
        .map_err(|e| format!("Metadata error: {}", e))?
        .len();

    if actual_size == 0 {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err("Downloaded zero bytes".to_string());
    }

    if actual_size != total_size {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!(
            "Incomplete download: expected {} bytes, received {} bytes",
            total_size, actual_size
        ));
    }

    // Rename .part → .mp4
    tokio::fs::rename(&tmp_path, dest_path)
        .await
        .map_err(|e| format!("Rename error: {}", e))?;

    log::info!(
        "Transcode: Cached original to {:?} ({} bytes)",
        dest_path,
        actual_size
    );
    Ok(actual_size)
}

// ── HLS Transcode (Phase 3) ─────────────────────────────────────────────

/// Run FFmpeg to generate a single HLS variant.
pub async fn run_transcode(
    ffmpeg_path: &Path,
    input_path: &Path,
    output_dir: &Path,
    quality: &QualityPreset,
    duration_secs: Option<f64>,
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
    progress_callback: impl Fn(f32),
) -> Result<(), String> {
    // Create output directory
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create HLS output dir: {}", e))?;

    let playlist_path = output_dir.join("index.m3u8");
    let segment_pattern = output_dir.join("segment_%03d.ts");

    let mut cmd = tokio::process::Command::new(ffmpeg_path);
    cmd.arg("-y") // Overwrite
        .arg("-i")
        .arg(input_path)
        // Explicit stream mapping: first video, optional audio, no subtitles/data
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-sn") // No subtitles
        .arg("-dn") // No data streams
        .arg("-vf")
        .arg(quality.scale_filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-crf")
        .arg("23")
        // HLS segment boundaries require IDR frames. Without forcing them
        // at the requested interval, a short video can become one giant
        // segment despite -hls_time, defeating responsive HLS seeking.
        .arg("-force_key_frames")
        .arg(format!("expr:gte(t,n_forced*{})", HLS_SEGMENT_TIME))
        .arg("-sc_threshold")
        .arg("0")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg(format!("{}k", quality.audio_bitrate_k))
        .arg("-maxrate")
        .arg(format!("{}k", quality.video_bitrate_k))
        .arg("-bufsize")
        .arg(format!("{}k", quality.video_bitrate_k * 2))
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(HLS_SEGMENT_TIME.to_string())
        .arg("-hls_playlist_type")
        .arg("vod")
        .arg("-hls_segment_filename")
        .arg(&segment_pattern)
        .arg(&playlist_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn FFmpeg: {}", e))?;
    let stderr = child.stderr.take().ok_or("No stderr pipe")?;

    // Parse FFmpeg progress from stderr, filter error lines inline for memory efficiency
    let stderr_reader = tokio::io::BufReader::new(stderr);
    let mut lines = stderr_reader.lines();
    let mut last_progress = 0f32;
    let mut stderr_error_lines: Vec<String> = Vec::new();

    let parse_result: Result<(), String> = loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                // Kill the FFmpeg process
                let _ = child.kill().await;
                let _ = child.wait().await;
                // Clean up partial output
                let _ = std::fs::remove_dir_all(output_dir);
                break Err("Cancelled".to_string());
            }
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        // Only store lines containing 'error' (case-insensitive) — avoids
                        // collecting thousands of progress lines for successful transcodes
                        if line.to_lowercase().contains("error") {
                            stderr_error_lines.push(line.clone());
                        }

                        // Parse time=HH:MM:SS.MS from FFmpeg stderr
                        if let Some(time_str) = line.split("time=").nth(1) {
                            let time_str = time_str.split_whitespace().next().unwrap_or("0");
                            let secs = parse_time_to_secs(time_str);
                            if let Some(dur) = duration_secs {
                                if dur > 0.0 {
                                    let pct = (secs / dur) as f32;
                                    if (pct - last_progress).abs() > 0.01 {
                                        last_progress = pct.clamp(0.0, 0.99);
                                        progress_callback(last_progress);
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => {
                        log::warn!("Transcode: stderr read error: {}", e);
                        break Ok(());
                    }
                }
            }
        }
    };

    // Wait for the process to finish (if not cancelled)
    let status = child
        .wait()
        .await
        .map_err(|e| format!("FFmpeg wait error: {}", e))?;

    // Check for cancellation or earlier error
    parse_result?;

    if !status.success() {
        let _ = std::fs::remove_dir_all(output_dir);
        let tail_msg = if stderr_error_lines.is_empty() {
            String::new()
        } else {
            format!("\nFFmpeg error lines:\n{}", stderr_error_lines.join("\n"))
        };
        return Err(format!(
            "FFmpeg exited with code {:?}{}",
            status.code(),
            tail_msg
        ));
    }

    // Only mark the output ready when FFmpeg has exited and every referenced
    // segment is durable, bounded and seekable via the finished VOD playlist.
    // The same validation is shared by cache reuse, startup cleanup and
    // transcoding status, preventing partial output from appearing as ready.
    if !finished_hls_variant(output_dir) {
        let _ = std::fs::remove_dir_all(output_dir);
        return Err("FFmpeg exited without a complete HLS VOD playlist and segments".to_string());
    }

    log::info!("Transcode: Generated HLS variant at {:?}", output_dir);
    Ok(())
}

fn parse_time_to_secs(time: &str) -> f64 {
    let parts: Vec<&str> = time.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        h * 3600.0 + m * 60.0 + s
    } else {
        0.0
    }
}

/// Detect resolution from a bounded MP4 header; never read the entire
/// multi-gigabyte original into memory just to inspect tkhd dimensions.
pub fn get_source_height(cached_path: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(cached_path).ok()?;
    let mut header = Vec::new();
    file.by_ref()
        .take(2 * 1024 * 1024)
        .read_to_end(&mut header)
        .ok()?;
    mp4_utils::scan_video_tkhd_dimensions(&header).1
}

// ── Execute Full Transcode Pipeline ─────────────────────────────────────

/// Run the full pipeline: cache original → transcode HLS.
/// Runs entirely on the async runtime (FFmpeg runs in its own OS process).
pub async fn execute_transcode_pipeline(
    manager: &TranscodeManager,
    key: &TranscodeKey,
    quality_preset: &QualityPreset,
    client: grammers_client::Client,
    media: Media,
    duration_secs: Option<f64>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let job_arc = {
        let jobs = manager.jobs.lock().await;
        jobs.get(&key.job_id()).cloned()
    };

    let job_arc = match job_arc {
        Some(j) => j,
        None => return,
    };

    let ffmpeg_path = { manager.ffmpeg_path.lock().await.clone() };

    let ffmpeg_path = match ffmpeg_path {
        Some(p) => p,
        None => {
            let mut job = job_arc.lock().await;
            job.phase = JobPhase::Error("FFmpeg not available".to_string());
            return;
        }
    };

    let file_key = key.file_key();
    let original_path = manager.original_path(&file_key);
    let output_dir = manager.hls_output_dir(&file_key, &key.quality);

    // ── Step 1: Cache original if needed ────────────────────────────
    // Serialize the shared original across *different quality* HLS jobs.
    // A concurrent quality must not truncate the first worker's .mp4.part
    // file. Waiting remains cancellable and never locks the global job map.
    let original_gate = manager.original_download_lock(&file_key).await;
    let original_guard = tokio::select! {
        guard = original_gate.lock_owned() => guard,
        _ = &mut cancel_rx => {
            job_arc.lock().await.phase = JobPhase::Cancelled;
            return;
        }
    };
    if cancel_rx.try_recv().is_ok() {
        job_arc.lock().await.phase = JobPhase::Cancelled;
        return;
    }
    if !original_path.exists() {
        {
            let mut job = job_arc.lock().await;
            job.phase = JobPhase::CachingOriginal { progress: 0.0 };
        }

        let job_arc_clone = job_arc.clone();
        match cache_original(
            &client,
            &media,
            &original_path,
            &mut cancel_rx,
            |progress| {
                let job_arc = job_arc_clone.clone();
                tauri::async_runtime::spawn(async move {
                    let mut job = job_arc.lock().await;
                    record_job_progress(&mut job, JobPhase::CachingOriginal { progress });
                });
            },
        )
        .await
        {
            Ok(size) => {
                log::info!(
                    "Transcode: Cached original ({} bytes), starting transcode...",
                    size
                );
            }
            Err(e) => {
                let mut job = job_arc.lock().await;
                if !matches!(job.phase, JobPhase::Cancelled) {
                    job.phase = JobPhase::Error(format!("Cache failed: {}", e));
                }
                return;
            }
        }
    }

    // The atomically completed original is now safe for other qualities.
    drop(original_guard);

    // ── Step 2: Detect source resolution ────────────────────────────
    let source_height = get_source_height(&original_path);

    {
        let mut job = job_arc.lock().await;
        job.source_height = source_height;
    }

    // Check if source is lower than requested quality — skip if so
    if let Some(src_h) = source_height {
        if src_h < quality_preset.height {
            let mut job = job_arc.lock().await;
            job.phase = JobPhase::Error(format!(
                "Source is {}p, cannot transcode to {}p",
                src_h, quality_preset.height
            ));
            return;
        }
    }

    // ── Step 3: Transcode ───────────────────────────────────────────
    {
        let mut job = job_arc.lock().await;
        if matches!(job.phase, JobPhase::Cancelled) {
            return;
        }
        job.phase = JobPhase::Transcoding { progress: 0.0 };
    }

    let job_arc_clone = job_arc.clone();
    let result = run_transcode(
        &ffmpeg_path,
        &original_path,
        &output_dir,
        quality_preset,
        duration_secs,
        &mut cancel_rx,
        |progress| {
            let job_arc = job_arc_clone.clone();
            tauri::async_runtime::spawn(async move {
                let mut job = job_arc.lock().await;
                record_job_progress(&mut job, JobPhase::Transcoding { progress });
            });
        },
    )
    .await;

    let mut job = job_arc.lock().await;
    if matches!(job.phase, JobPhase::Cancelled) {
        return;
    }
    match result {
        Ok(()) => {
            job.phase = JobPhase::Ready;
            log::info!("Transcode: Job {} completed successfully", key.job_id());
        }
        Err(e) => {
            job.phase = JobPhase::Error(e);
        }
    }
}

// ── Tauri Commands ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn cmd_get_transcode_capabilities(
    manager: tauri::State<'_, Arc<TranscodeManager>>,
    app_handle: tauri::AppHandle,
) -> Result<TranscodeCapabilities, String> {
    // Lazy detection: if FFmpeg hasn't been detected yet, try now.
    // This fixes the race where the background detection hasn't completed
    // by the time the UI first asks for capabilities.
    let ffmpeg_available = {
        let path_guard = manager.ffmpeg_path.lock().await;
        if path_guard.is_some() {
            true
        } else {
            drop(path_guard);
            // Attempt lazy detection on first call
            if let Some(ffmpeg) = detect_ffmpeg(&app_handle).await {
                *manager.ffmpeg_path.lock().await = Some(ffmpeg);
                true
            } else {
                false
            }
        }
    };

    let variants: Vec<QualityVariant> = QUALITY_PRESETS
        .iter()
        .map(|p| QualityVariant {
            label: p.label.to_string(),
            height: p.height,
            available: ffmpeg_available,
        })
        .collect();

    Ok(TranscodeCapabilities {
        available: ffmpeg_available,
        variants,
        mode: if ffmpeg_available {
            "hls".to_string()
        } else {
            "original".to_string()
        },
    })
}

#[tauri::command]
pub async fn cmd_prepare_transcoded_stream(
    message_id: i32,
    folder_id: Option<i64>,
    quality: String,
    state: tauri::State<'_, TelegramState>,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<TranscodePrepareResult, String> {
    let folder_id = folder_id.unwrap_or(0);
    let key = TranscodeKey {
        folder_id,
        message_id,
        quality: quality.clone(),
    };

    // Validate quality
    let preset = QUALITY_PRESETS
        .iter()
        .find(|p| p.label == quality)
        .ok_or_else(|| format!("Unknown quality: {}", quality))?;

    // An in-progress FFmpeg worker publishes index.m3u8 before it is done.
    // A disk-only existence check can announce partial output as ready.
    // Honor a known job's phase; only reuse persisted output if every segment
    // is present and the VOD end marker confirms completion.
    let output_dir = manager.hls_output_dir(&key.file_key(), &quality);
    let existing_job = {
        let jobs = manager.jobs.lock().await;
        jobs.get(&key.job_id()).cloned()
    };
    let known_ready = if let Some(job) = existing_job.as_ref() {
        let job = job.lock().await;
        matches!(job.phase, JobPhase::Ready) || !job.in_flight
    } else {
        true
    };
    if known_ready && finished_hls_variant(&output_dir) {
        return Ok(TranscodePrepareResult {
            job_id: key.job_id(),
            status: "ready".to_string(),
            progress: 1.0,
            playlist_url: Some(format!("/hls/{}/{}/index.m3u8", key.file_key(), quality)),
        });
    }

    // Check if job already exists
    let (job_arc, is_new) = manager.get_or_create_job(&key).await;
    let phase = {
        let job = job_arc.lock().await;
        job.phase.clone()
    };

    if !is_new {
        // Job exists — return its current status
        return match &phase {
            JobPhase::NotStarted => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "pending".to_string(),
                progress: 0.0,
                playlist_url: None,
            }),
            JobPhase::CachingOriginal { progress } => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "caching".to_string(),
                progress: *progress,
                playlist_url: None,
            }),
            JobPhase::Transcoding { progress } => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "transcoding".to_string(),
                progress: *progress,
                playlist_url: None,
            }),
            JobPhase::Ready => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "ready".to_string(),
                progress: 1.0,
                playlist_url: Some(format!("/hls/{}/{}/index.m3u8", key.file_key(), quality)),
            }),
            JobPhase::Error(_e) => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "error".to_string(),
                progress: 0.0,
                playlist_url: None,
            }),
            JobPhase::Cancelled => Ok(TranscodePrepareResult {
                job_id: key.job_id(),
                status: "cancelled".to_string(),
                progress: 0.0,
                playlist_url: None,
            }),
        };
    }

    // Reserve the job *before* Telegram network preparation. A retry or
    // cancel during this phase cannot start a concurrent task for the same
    // deterministic HLS output directory.
    let prepared = async {
        let client = { state.client.lock().await.clone() };
        let client = client.ok_or_else(|| "Not connected to Telegram".to_string())?;

        let peer = crate::commands::utils::resolve_peer(
            &client,
            if folder_id == 0 {
                None
            } else {
                Some(folder_id)
            },
            &state.peer_cache,
        )
        .await?;

        let messages = client
            .get_messages_by_id(&peer, &[message_id])
            .await
            .map_err(|e| e.to_string())?;

        let msg = messages
            .into_iter()
            .flatten()
            .next()
            .ok_or_else(|| format!("Message {} not found", message_id))?;

        let media = msg.media().ok_or_else(|| "No media".to_string())?;

        // Get duration from mp4parse (quick moov chunk).
        let duration_secs = get_duration_from_media(&client, message_id, folder_id, &state)
            .await
            .ok();
        Ok::<_, String>((client, media, duration_secs))
    }
    .await;

    let (client, media, duration_secs) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut job = job_arc.lock().await;
            if !matches!(job.phase, JobPhase::Cancelled) {
                job.phase = JobPhase::Error(error.clone());
            }
            job.in_flight = false;
            return Err(error);
        }
    };

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    {
        let mut job = job_arc.lock().await;
        if matches!(job.phase, JobPhase::Cancelled) {
            job.in_flight = false;
            return Err("Transcode cancelled during preparation".to_string());
        }
        job.cancel_tx = Some(cancel_tx);
    }

    let manager_clone = manager.inner().clone();
    let key_clone = key.clone();
    let preset_clone = preset.clone();

    // Spawn the pipeline on a background task
    tauri::async_runtime::spawn(async move {
        execute_transcode_pipeline(
            &manager_clone,
            &key_clone,
            &preset_clone,
            client,
            media,
            duration_secs,
            cancel_rx,
        )
        .await;

        // Only now can another attempt reuse the deterministic file/quality
        // key. The old FFmpeg process and cache worker have stopped.
        {
            let mut job = job_arc.lock().await;
            job.cancel_tx = None;
            job.in_flight = false;
        }

        // LRU eviction after job completes
        manager_clone.evict_lru().await;
    });

    Ok(TranscodePrepareResult {
        job_id: key.job_id(),
        status: "started".to_string(),
        progress: 0.0,
        playlist_url: None,
    })
}

async fn get_duration_from_media(
    client: &grammers_client::Client,
    message_id: i32,
    folder_id: i64,
    state: &TelegramState,
) -> Result<f64, String> {
    let peer = crate::commands::utils::resolve_peer(
        client,
        if folder_id == 0 {
            None
        } else {
            Some(folder_id)
        },
        &state.peer_cache,
    )
    .await?;

    let messages = client
        .get_messages_by_id(&peer, &[message_id])
        .await
        .map_err(|e| e.to_string())?;

    let msg = messages
        .into_iter()
        .flatten()
        .next()
        .ok_or_else(|| "Message not found".to_string())?;

    let media = msg.media().ok_or_else(|| "No media".to_string())?;

    let size = match &media {
        Media::Document(d) => d.size() as u64,
        _ => return Err("Not a document".to_string()),
    };

    // Download first 2MB and parse moov
    let max_bytes = std::cmp::min(2 * 1024 * 1024, size) as usize;
    let mut buffer: Vec<u8> = Vec::with_capacity(max_bytes);
    let mut download_iter = client.iter_download(&media);
    download_iter = download_iter.chunk_size(65536);

    while buffer.len() < max_bytes {
        match download_iter.next().await {
            Ok(Some(chunk)) => {
                let remaining = max_bytes.saturating_sub(buffer.len());
                let take = std::cmp::min(chunk.len(), remaining);
                buffer.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("Download error: {}", e)),
        }
    }

    // Parse with mp4parse
    let mut cursor = std::io::Cursor::new(&buffer);
    let context = mp4parse::read_mp4(&mut cursor).map_err(|e| format!("MP4 parse error: {}", e))?;

    let video_track = context
        .tracks
        .iter()
        .find(|t| t.track_type == mp4parse::TrackType::Video);

    video_track
        .and_then(|t| {
            let d = t.duration.as_ref()?;
            let ts = t.timescale.as_ref()?;
            Some((d.0 as f64) / (ts.0 as f64))
        })
        .ok_or_else(|| "No video track duration".to_string())
}

#[tauri::command]
pub async fn cmd_get_transcode_status(
    job_id: String,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<TranscodeStatusResult, String> {
    let (key, phase) = manager
        .get_job_phase_snapshot(&job_id)
        .await
        .ok_or_else(|| format!("Job {} not found", job_id))?;

    let (status_str, progress, error, playlist_url) = match &phase {
        JobPhase::NotStarted => ("pending".to_string(), 0.0, None, None),
        JobPhase::CachingOriginal { progress } => ("caching".to_string(), *progress, None, None),
        JobPhase::Transcoding { progress } => ("transcoding".to_string(), *progress, None, None),
        JobPhase::Ready
            if finished_hls_variant(&manager.hls_output_dir(&key.file_key(), &key.quality)) =>
        {
            (
                "ready".to_string(),
                1.0,
                None,
                Some(format!(
                    "/hls/{}/{}/index.m3u8",
                    key.file_key(),
                    key.quality
                )),
            )
        }
        JobPhase::Ready => (
            "error".to_string(),
            0.0,
            Some("The cached HLS playlist is incomplete or was evicted. Select Retry.".to_string()),
            None,
        ),
        JobPhase::Error(e) => ("error".to_string(), 0.0, Some(e.clone()), None),
        JobPhase::Cancelled => ("cancelled".to_string(), 0.0, None, None),
    };

    Ok(TranscodeStatusResult {
        job_id,
        status: status_str,
        progress,
        error,
        playlist_url,
    })
}

#[tauri::command]
pub async fn cmd_cancel_transcode(
    job_id: String,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<(), String> {
    let jobs = manager.jobs.lock().await;
    let job_arc = jobs
        .get(&job_id)
        .ok_or_else(|| format!("Job {} not found", job_id))?;

    let mut job = job_arc.lock().await;
    if let Some(tx) = job.cancel_tx.take() {
        let _ = tx.send(());
    }
    job.phase = JobPhase::Cancelled;

    Ok(())
}

// ── Cache management commands ───────────────────────────────────────

#[derive(serde::Serialize)]
pub struct TranscodeCacheInfo {
    pub current_bytes: u64,
    pub max_bytes: u64,
    pub cached_variants: Vec<String>,
}
#[tauri::command]
pub async fn cmd_get_transcode_cache_info(
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<TranscodeCacheInfo, String> {
    let current = manager.total_cache_size();
    let max = manager.get_max_cache_bytes().await;
    Ok(TranscodeCacheInfo {
        current_bytes: current,
        max_bytes: max,
        cached_variants: vec![],
    })
}

#[tauri::command]
pub async fn cmd_set_transcode_cache_limit(
    max_gb: u32,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<(), String> {
    let gb = max_gb.clamp(1, 50);
    let max_bytes = (gb as u64) * 1024 * 1024 * 1024;
    manager.set_max_cache_bytes(max_bytes).await;
    log::info!(
        "Transcode: Cache limit set to {} GB ({} bytes)",
        gb,
        max_bytes
    );
    Ok(())
}

// ── Cached variants command ─────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct CachedVariantInfo {
    pub quality: String,
    pub available: bool,
}

#[tauri::command]
pub async fn cmd_get_cached_variants(
    message_id: i32,
    folder_id: Option<i64>,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<Vec<CachedVariantInfo>, String> {
    let folder_id = folder_id.unwrap_or(0);
    let file_key = format!("{}_{}", folder_id, message_id);

    let variants: Vec<CachedVariantInfo> = QUALITY_PRESETS
        .iter()
        .map(|p| {
            let output_dir = manager.hls_output_dir(&file_key, p.label);
            CachedVariantInfo {
                quality: p.label.to_string(),
                available: finished_hls_variant(&output_dir),
            }
        })
        .collect();

    Ok(variants)
}

// ── Detailed cache info (per-file per-quality with sizes) ──────────

#[derive(serde::Serialize)]
pub struct CacheEntry {
    pub file_key: String,
    pub quality: String,
    pub size_bytes: u64,
    pub playlist_exists: bool,
}

#[derive(serde::Serialize)]
pub struct DetailedCacheInfo {
    pub entries: Vec<CacheEntry>,
    pub total_bytes: u64,
    pub max_bytes: u64,
}

#[tauri::command]
pub async fn cmd_get_detailed_transcode_cache(
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<DetailedCacheInfo, String> {
    let mut entries: Vec<CacheEntry> = Vec::new();
    let hls_root = manager.cache_root.join(HLS_DIR);

    if let Ok(file_dirs) = std::fs::read_dir(&hls_root) {
        for file_entry in file_dirs.flatten() {
            let file_path = file_entry.path();
            if !file_path.is_dir() {
                continue;
            }
            let file_key = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            if let Ok(quality_dirs) = std::fs::read_dir(&file_path) {
                for q_entry in quality_dirs.flatten() {
                    let q_path = q_entry.path();
                    if !q_path.is_dir() {
                        continue;
                    }
                    let quality = q_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    let playlist_exists = q_path.join("index.m3u8").exists();

                    // Sum file sizes in this quality directory
                    let mut size_bytes = 0u64;
                    if let Ok(files) = std::fs::read_dir(&q_path) {
                        for f in files.flatten() {
                            if let Ok(meta) = f.metadata() {
                                size_bytes += meta.len();
                            }
                        }
                    }

                    entries.push(CacheEntry {
                        file_key: file_key.clone(),
                        quality,
                        size_bytes,
                        playlist_exists,
                    });
                }
            }
        }
    }

    // Also count originals
    let orig_root = manager.cache_root.join(ORIGINALS_DIR);
    if let Ok(orig_files) = std::fs::read_dir(&orig_root) {
        for of in orig_files.flatten() {
            let path = of.path();
            if path.is_file() {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                entries.push(CacheEntry {
                    file_key: stem.to_string(),
                    quality: "original".to_string(),
                    size_bytes,
                    playlist_exists: path.exists(),
                });
            }
        }
    }

    let total_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let max_bytes = manager.get_max_cache_bytes().await;

    Ok(DetailedCacheInfo {
        entries,
        total_bytes,
        max_bytes,
    })
}

// ── Clear transcode cache (all, per-file, or per-variant) ──────────

/// Cache deletion accepts only canonical remote file IDs and supported
/// preset labels. User-provided paths must never reach remove_dir_all.
fn validate_cache_clear_selection(
    file_key: Option<&str>,
    quality: Option<&str>,
) -> Result<(), String> {
    if let Some(key) = file_key {
        let (folder, message) = key
            .rsplit_once('_')
            .ok_or_else(|| "Invalid transcode cache file ID".to_string())?;
        let folder = folder
            .parse::<i64>()
            .map_err(|_| "Invalid transcode cache folder ID".to_string())?;
        let message = message
            .parse::<i32>()
            .map_err(|_| "Invalid transcode cache message ID".to_string())?;
        if format!("{folder}_{message}") != key {
            return Err("Noncanonical transcode cache file ID".to_string());
        }
    } else if quality.is_some() {
        return Err("Cannot clear quality without specifying file_key".to_string());
    }
    if let Some(quality) = quality {
        if !QUALITY_PRESETS.iter().any(|preset| preset.label == quality) {
            return Err("Unknown transcode cache quality".to_string());
        }
    }
    Ok(())
}

/// A cache clear is blocked while a worker still uses the selected source
/// or variant. This check deliberately blocks variant clears for any active
/// quality of that source, because clearing the final variant can also delete
/// the original input shared by the other quality workers.
fn cache_clear_conflicts_with_job(file_key: Option<&str>, job: &TranscodeJob) -> bool {
    job.in_flight && file_key.is_none_or(|key| key == job.key.file_key())
}

#[tauri::command]
pub async fn cmd_clear_transcode_cache(
    file_key: Option<String>,
    quality: Option<String>,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<String, String> {
    validate_cache_clear_selection(file_key.as_deref(), quality.as_deref())?;
    // Keep the registry locked for the entire filesystem deletion, so a
    // concurrent prepare cannot reserve this source just after the safety
    // check and then lose its output or original file.
    let jobs_guard = manager.jobs.lock().await;
    for job in jobs_guard.values() {
        let job_guard = job.lock().await;
        if cache_clear_conflicts_with_job(file_key.as_deref(), &job_guard) {
            return Err("Cannot clear media cache while a transcode is running".to_string());
        }
    }
    match (file_key, quality) {
        // Clear everything
        (None, None) => {
            let hls_root = manager.cache_root.join(HLS_DIR);
            let orig_root = manager.cache_root.join(ORIGINALS_DIR);
            let mut removed_count = 0u64;

            if hls_root.exists() {
                if let Ok(entries) = std::fs::read_dir(&hls_root) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_dir_all(entry.path());
                        removed_count += 1;
                    }
                }
            }
            if orig_root.exists() {
                if let Ok(entries) = std::fs::read_dir(&orig_root) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                        removed_count += 1;
                    }
                }
            }

            log::info!("Transcode: Cleared all cache ({} entries)", removed_count);
            Ok(format!(
                "Cleared all transcode cache ({} entries)",
                removed_count
            ))
        }
        // Clear all variants for a specific file
        (Some(fk), None) => {
            let hls_path = manager.cache_root.join(HLS_DIR).join(&fk);
            let orig_path = manager
                .cache_root
                .join(ORIGINALS_DIR)
                .join(format!("{}.mp4", fk));

            if hls_path.exists() {
                let _ = std::fs::remove_dir_all(&hls_path);
            }
            if orig_path.exists() {
                let _ = std::fs::remove_file(&orig_path);
            }

            log::info!("Transcode: Cleared cache for file {}", fk);
            Ok(format!("Cleared cache for {}", fk))
        }
        // Clear a specific quality variant for a file
        (Some(fk), Some(q)) => {
            let variant_path = manager.hls_output_dir(&fk, &q);
            if variant_path.exists() {
                let _ = std::fs::remove_dir_all(&variant_path);
            }
            // If no more qualities remain for this file, also remove the parent directory
            // and the orphaned original file.
            let file_dir = manager.cache_root.join(HLS_DIR).join(&fk);
            if file_dir.exists() {
                let has_other_variants = std::fs::read_dir(&file_dir)
                    .map(|mut d| d.any(|e| e.ok().map(|e| e.path().is_dir()).unwrap_or(false)))
                    .unwrap_or(false);
                if !has_other_variants {
                    let _ = std::fs::remove_dir_all(&file_dir);
                    // Clean up orphaned original so it doesn't linger on disk
                    let orig_path = manager.original_path(&fk);
                    if orig_path.exists() {
                        let _ = std::fs::remove_file(&orig_path);
                        log::info!("Transcode: Removed orphaned original {:?}", orig_path);
                    }
                }
            }

            log::info!("Transcode: Cleared variant {} for file {}", q, fk);
            Ok(format!("Cleared {} variant for {}", q, fk))
        }
        (None, Some(_)) => Err("Cannot clear quality without specifying file_key".to_string()),
    }
}

#[tauri::command]
pub async fn cmd_get_master_playlist_info(
    message_id: i32,
    folder_id: Option<i64>,
    manager: tauri::State<'_, Arc<TranscodeManager>>,
) -> Result<MasterPlaylistInfo, String> {
    let folder_id = folder_id.unwrap_or(0);
    let file_key = format!("{}_{}", folder_id, message_id);

    let mut variants: Vec<MasterVariant> = Vec::new();

    for preset in QUALITY_PRESETS {
        let output_dir = manager.hls_output_dir(&file_key, preset.label);

        if finished_hls_variant(&output_dir) {
            // Try to read the playlist to get bandwidth info
            let bandwidth =
                estimate_bandwidth(&output_dir).unwrap_or(preset.video_bitrate_k * 1000);

            variants.push(MasterVariant {
                bandwidth,
                resolution: format!("{}x{}", preset.height * 16 / 9, preset.height),
                quality: preset.label.to_string(),
                playlist_path: format!("/hls/{}/{}/index.m3u8", file_key, preset.label),
            });
        }
    }

    let has_variants = !variants.is_empty();
    let master_url = if has_variants {
        Some(format!("/hls/{}/master.m3u8", file_key))
    } else {
        None
    };

    Ok(MasterPlaylistInfo {
        file_key: file_key.clone(),
        variants,
        master_playlist_url: master_url,
    })
}

#[derive(serde::Serialize, Clone)]
pub struct MasterPlaylistInfo {
    pub file_key: String,
    pub variants: Vec<MasterVariant>,
    pub master_playlist_url: Option<String>,
}

#[derive(serde::Serialize, Clone)]
pub struct MasterVariant {
    pub bandwidth: u32,
    pub resolution: String,
    pub quality: String,
    pub playlist_path: String,
}

fn estimate_bandwidth(output_dir: &Path) -> Option<u32> {
    let playlist = output_dir.join("index.m3u8");
    let content = std::fs::read_to_string(&playlist).ok()?;

    // Sum up segment sizes and find total duration from EXTINF tags
    let mut total_bytes = 0u64;
    let mut total_duration = 0f64;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("#EXTINF:") {
            let dur_str = line
                .trim_start_matches("#EXTINF:")
                .split(',')
                .next()
                .unwrap_or("0");
            total_duration += dur_str.parse::<f64>().unwrap_or(0.0);
        } else if line.ends_with(".ts") {
            let seg_path = output_dir.join(line);
            if let Ok(meta) = std::fs::metadata(&seg_path) {
                total_bytes += meta.len();
            }
        }
    }

    if total_duration > 0.0 {
        Some(((total_bytes as f64 * 8.0) / total_duration) as u32)
    } else {
        None
    }
}

// ── HLS Serving Routes (Phase 4) ────────────────────────────────────────

use crate::server::StreamTokenData;

#[derive(serde::Deserialize)]
struct HlsQuery {
    token: Option<String>,
}

/// Add the already-validated loopback stream token to each relative TS segment.
/// Native HLS clients do not run hls.js's xhrSetup on child media requests.
/// Refuse unexpected segment references instead of passing external or
/// traversing URLs through a manifest that contains authenticated media.
fn authenticated_hls_media_playlist(playlist: &str, token: &str) -> Option<String> {
    let token = urlencoding::encode(token);
    let mut body = String::with_capacity(playlist.len() + token.len() * 8);
    for line in playlist.lines() {
        let item = line.trim();
        if item.is_empty() || item.starts_with('#') {
            // This pipeline produces unencrypted MPEG-TS HLS. If an output is
            // unexpectedly switched to keys or mapped media, do not publish
            // URIs without authentication and containment checks.
            if item.starts_with("#EXT-X-KEY:") || item.starts_with("#EXT-X-MAP:") {
                return None;
            }
            body.push_str(line);
        } else {
            let numeric = item.strip_prefix("segment_")?.strip_suffix(".ts")?;
            if numeric.is_empty() || !numeric.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            body.push_str(item);
            body.push_str("?token=");
            body.push_str(&token);
        }
        body.push('\n');
    }
    Some(body)
}

/// Serve an HLS playlist (.m3u8) or segment (.ts).
async fn serve_hls_file(
    _req: HttpRequest,
    file_key: &str,
    quality: &str,
    segment: Option<&str>,
    query: &HlsQuery,
    manager: &TranscodeManager,
    token_data: &StreamTokenData,
) -> impl Responder {
    // Validate token
    match &query.token {
        Some(t) if t == &token_data.token => {}
        _ => return HttpResponse::Forbidden().body("Invalid or missing stream token"),
    }

    // Validate path
    let file_path = match manager.validate_hls_path(file_key, quality, segment) {
        Some(p) => p,
        None => return HttpResponse::NotFound().body("File not found"),
    };

    if !file_path.exists() {
        return HttpResponse::NotFound().body("File not found");
    }

    // Determine MIME type
    let mime = if file_path.extension().map(|e| e == "m3u8").unwrap_or(false) {
        "application/vnd.apple.mpegurl"
    } else if file_path.extension().map(|e| e == "ts").unwrap_or(false) {
        "video/mp2t"
    } else {
        "application/octet-stream"
    };

    // FFmpeg-generated media playlists are small. Rewrite only their
    // relative TS references so native HLS clients inherit authentication
    // even without JavaScript request interceptors. Do not buffer TS media.
    if mime == "application/vnd.apple.mpegurl" {
        const MAX_PLAYLIST_BYTES: u64 = 4 * 1024 * 1024;
        let Ok(metadata) = tokio::fs::metadata(&file_path).await else {
            return HttpResponse::NotFound().body("Playlist not found");
        };
        if !metadata.is_file() || metadata.len() > MAX_PLAYLIST_BYTES {
            return HttpResponse::BadRequest().body("Invalid HLS playlist");
        }
        let Ok(playlist) = tokio::fs::read_to_string(&file_path).await else {
            return HttpResponse::NotFound().body("Playlist unreadable");
        };
        let Some(authorized) = authenticated_hls_media_playlist(&playlist, &token_data.token)
        else {
            return HttpResponse::BadRequest().body("Invalid HLS playlist references");
        };
        return HttpResponse::Ok()
            .content_type("application/vnd.apple.mpegurl")
            .insert_header(("Cache-Control", "private, max-age=10"))
            .body(authorized);
    }

    // Never read an entire encoded segment into memory. NamedFile streams
    // bounded chunks from the owned HLS cache and implements HTTP Range,
    // Content-Range and conditional request semantics for media seeking.
    match actix_files::NamedFile::open_async(&file_path).await {
        Ok(file) => {
            let mut resp = file
                .set_content_type(mime.parse().expect("static HLS MIME is valid"))
                .into_response(&_req);
            // Cache headers: successful segments can be cached longer,
            // playlists shorter; error responses must not be cached as media.
            let cache = if !resp.status().is_success() {
                "no-store"
            } else if mime == "video/mp2t" {
                // The URL contains a session token; keep browser caching,
                // but never authorize shared intermediary caches to retain it.
                "private, max-age=3600"
            } else {
                "private, max-age=10"
            };
            resp.headers_mut().insert(
                actix_web::http::header::CACHE_CONTROL,
                actix_web::http::header::HeaderValue::from_static(cache),
            );
            resp
        }
        Err(e) => {
            log::error!("Transcode: Failed to open HLS file {:?}: {}", file_path, e);
            HttpResponse::InternalServerError().body("Failed to open file")
        }
    }
}

/// GET /hls/{file_key}/master.m3u8
#[actix_web::get("/hls/{file_key}/master.m3u8")]
async fn hls_master_playlist(
    _req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<HlsQuery>,
    manager: web::Data<Arc<TranscodeManager>>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    let file_key = path.into_inner();

    // Validate token
    match &query.token {
        Some(t) if t == &token_data.token => {}
        _ => return HttpResponse::Forbidden().body("Invalid or missing stream token"),
    }

    // Build master playlist from available variants
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");

    for preset in QUALITY_PRESETS {
        // The master playlist is also a filesystem reader. Resolve each
        // variant through the same canonical HLS-root containment check used
        // for playlists and segments, rather than joining an untrusted key
        // directly onto the cache root.
        let Some(playlist_path) = manager.validate_hls_path(&file_key, preset.label, None) else {
            continue;
        };
        let Some(hls_dir) = playlist_path.parent() else {
            continue;
        };
        // FFmpeg creates the playlist before writing its final segments and
        // VOD end marker. Do not advertise a partial/evicted variant through
        // the master playlist just because index.m3u8 exists on disk.
        if !finished_hls_variant(hls_dir) {
            continue;
        }
        let bandwidth = estimate_bandwidth(hls_dir).unwrap_or(preset.video_bitrate_k * 1000);
        let width = preset.height * 16 / 9;
        playlist.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={}x{}\n{}/index.m3u8?token={}\n",
            bandwidth,
            width,
            preset.height,
            preset.label,
            urlencoding::encode(&token_data.token),
        ));
    }

    if playlist.lines().count() <= 2 {
        return HttpResponse::NotFound().body("No HLS variants available");
    }

    HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(("Cache-Control", "private, max-age=5"))
        .body(playlist)
}

/// GET /hls/{file_key}/{quality}/index.m3u8
#[actix_web::get("/hls/{file_key}/{quality}/index.m3u8")]
async fn hls_playlist(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<HlsQuery>,
    manager: web::Data<Arc<TranscodeManager>>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    let (file_key, quality) = path.into_inner();
    serve_hls_file(
        req,
        &file_key,
        &quality,
        None,
        &query,
        &manager,
        &token_data,
    )
    .await
}

/// GET /hls/{file_key}/{quality}/{segment}
#[actix_web::get("/hls/{file_key}/{quality}/{segment}")]
async fn hls_segment(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    query: web::Query<HlsQuery>,
    manager: web::Data<Arc<TranscodeManager>>,
    token_data: web::Data<StreamTokenData>,
) -> impl Responder {
    let (file_key, quality, segment) = path.into_inner();
    serve_hls_file(
        req,
        &file_key,
        &quality,
        Some(&segment),
        &query,
        &manager,
        &token_data,
    )
    .await
}

/// Register HLS routes on an Actix ServiceConfig.
pub fn configure_hls_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(hls_master_playlist)
        .service(hls_playlist)
        .service(hls_segment);
}

#[cfg(test)]
mod playback_manager_tests {
    use super::*;
    use std::any::TypeId;

    fn assert_capability_state<T: Send + Sync + 'static, F>(
        _: impl Fn(tauri::State<'static, T>, tauri::AppHandle) -> F,
    ) {
        assert_eq!(
            TypeId::of::<T>(),
            TypeId::of::<Arc<TranscodeManager>>(),
            "Playback command requests a manager type that app setup never registers"
        );
    }

    #[tokio::test]
    async fn status_snapshot_does_not_hold_global_registry_while_job_is_busy() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-status-registry-lock-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = Arc::new(TranscodeManager::new(root.clone()));
        let key = TranscodeKey {
            folder_id: 7,
            message_id: 95,
            quality: "360p".to_string(),
        };
        let (job, is_new) = manager.get_or_create_job(&key).await;
        assert!(is_new);
        let held_job = job.lock().await;
        let query_manager = manager.clone();
        let job_id = key.job_id();
        let query =
            tokio::spawn(async move { query_manager.get_job_phase_snapshot(&job_id).await });
        // Give status polling an opportunity to reach the intentionally busy
        // per-job lock, while other quality requests need the jobs registry.
        tokio::task::yield_now().await;
        let registry =
            tokio::time::timeout(std::time::Duration::from_millis(300), manager.jobs.lock())
                .await
                .expect("status polling pinned the global HLS jobs map behind a busy job");
        drop(registry);
        drop(held_job);
        let (reported_key, phase) = query
            .await
            .expect("snapshot task failed")
            .expect("status lost an existing HLS job");
        assert_eq!(reported_key.job_id(), key.job_id());
        assert!(matches!(phase, JobPhase::NotStarted));
        drop(manager);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_hls_job_restarts_only_after_old_worker_stops() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-retry-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 27,
            message_id: 1234,
            quality: "480p".to_string(),
        };

        let (first, new) = manager.get_or_create_job(&key).await;
        assert!(new);
        assert!(first.lock().await.in_flight);
        let (same, new) = manager.get_or_create_job(&key).await;
        assert!(!new, "a concurrent preparation spawned a second worker");
        assert!(Arc::ptr_eq(&first, &same));

        {
            let mut job = first.lock().await;
            job.phase = JobPhase::Cancelled;
            // Cancellation is signalled, but the old worker hasn't finished.
            assert!(job.in_flight);
        }
        let (still_same, new) = manager.get_or_create_job(&key).await;
        assert!(!new, "cancelled worker must finish before replacement");
        assert!(Arc::ptr_eq(&first, &still_same));

        first.lock().await.in_flight = false;
        let (retried, new) = manager.get_or_create_job(&key).await;
        assert!(new, "terminal cancelled jobs must permit Retry");
        assert!(!Arc::ptr_eq(&first, &retried));
        assert!(matches!(retried.lock().await.phase, JobPhase::NotStarted));
        assert!(retried.lock().await.in_flight);

        // A late callback holding the old attempt cannot poison the retry.
        first.lock().await.phase = JobPhase::Error("late error".to_string());
        assert!(matches!(retried.lock().await.phase, JobPhase::NotStarted));
        let (same_retry, new) = manager.get_or_create_job(&key).await;
        assert!(!new);
        assert!(Arc::ptr_eq(&retried, &same_retry));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_hls_job_can_retry_after_preparation_releases_reservation() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-failure-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 0,
            message_id: 4312,
            quality: "360p".to_string(),
        };
        let (first, new) = manager.get_or_create_job(&key).await;
        assert!(new);
        {
            let mut job = first.lock().await;
            job.phase = JobPhase::Error("injected preparation failure".to_string());
            job.in_flight = false;
        }
        let (retried, new) = manager.get_or_create_job(&key).await;
        assert!(new, "a failed job must not permanently disable Retry");
        assert!(!Arc::ptr_eq(&first, &retried));
        assert!(retried.lock().await.in_flight);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn evicted_ready_hls_variant_is_recreated_only_after_worker_exit() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-evicted-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 18,
            message_id: 5412,
            quality: "720p".to_string(),
        };
        let (first, _) = manager.get_or_create_job(&key).await;
        first.lock().await.phase = JobPhase::Ready;
        let (during, new) = manager.get_or_create_job(&key).await;
        assert!(
            !new,
            "do not recreate a ready job with its worker still active"
        );
        assert!(Arc::ptr_eq(&first, &during));

        first.lock().await.in_flight = false;
        let (retry, new) = manager.get_or_create_job(&key).await;
        assert!(
            new,
            "an evicted playlist cannot leave a false-ready job forever"
        );
        assert!(!Arc::ptr_eq(&first, &retry));

        let playlist = manager
            .hls_output_dir(&key.file_key(), &key.quality)
            .join("index.m3u8");
        std::fs::create_dir_all(playlist.parent().unwrap()).unwrap();
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        std::fs::write(
            playlist.parent().unwrap().join("segment_000.ts"),
            b"valid-ts",
        )
        .unwrap();
        retry.lock().await.phase = JobPhase::Ready;
        retry.lock().await.in_flight = false;
        let (cached, new) = manager.get_or_create_job(&key).await;
        assert!(
            !new,
            "a valid cached playlist should never be transcoded again"
        );
        assert!(Arc::ptr_eq(&retry, &cached));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_async_progress_never_reopens_cancelled_or_completed_hls_jobs() {
        let key = TranscodeKey {
            folder_id: 0,
            message_id: 415,
            quality: "480p".into(),
        };
        let mut job = TranscodeJob {
            key,
            phase: JobPhase::CachingOriginal { progress: 0.1 },
            in_flight: true,
            cancel_tx: None,
            last_access: Instant::now(),
            source_height: None,
        };
        record_job_progress(&mut job, JobPhase::CachingOriginal { progress: 0.5 });
        assert!(matches!(job.phase, JobPhase::CachingOriginal { progress } if progress == 0.5));
        record_job_progress(&mut job, JobPhase::CachingOriginal { progress: 0.25 });
        assert!(matches!(job.phase, JobPhase::CachingOriginal { progress } if progress == 0.5));
        job.phase = JobPhase::Transcoding { progress: 0.0 };
        record_job_progress(&mut job, JobPhase::CachingOriginal { progress: 0.9 });
        assert!(matches!(job.phase, JobPhase::Transcoding { progress } if progress == 0.0));
        record_job_progress(&mut job, JobPhase::Transcoding { progress: 0.8 });
        assert!(matches!(job.phase, JobPhase::Transcoding { progress } if progress == 0.8));
        job.phase = JobPhase::Cancelled;
        record_job_progress(&mut job, JobPhase::Transcoding { progress: 0.99 });
        assert!(matches!(job.phase, JobPhase::Cancelled));
        job.phase = JobPhase::Ready;
        record_job_progress(&mut job, JobPhase::Transcoding { progress: 1.0 });
        assert!(matches!(job.phase, JobPhase::Ready));
        job.phase = JobPhase::Error("interrupted".into());
        record_job_progress(&mut job, JobPhase::CachingOriginal { progress: 0.7 });
        assert!(matches!(job.phase, JobPhase::Error(_)));
        job.phase = JobPhase::CachingOriginal { progress: 0.0 };
        job.in_flight = false;
        record_job_progress(&mut job, JobPhase::CachingOriginal { progress: 0.3 });
        assert!(matches!(job.phase, JobPhase::CachingOriginal { progress } if progress == 0.0));
    }

    #[test]
    fn startup_cleanup_preserves_finished_quality_and_prunes_crashed_sibling() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-startup-qa-{:032x}",
            rand::random::<u128>()
        ));
        let hls_root = root.join(HLS_DIR).join("7_12");
        let good = hls_root.join("480p");
        let partial = hls_root.join("720p");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(
            good.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        std::fs::write(good.join("segment_000.ts"), b"ts").unwrap();
        std::fs::write(
            partial.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n",
        )
        .unwrap();
        std::fs::write(partial.join("segment_000.ts"), b"interrupted").unwrap();

        let _manager = TranscodeManager::new(root.clone());
        assert!(
            finished_hls_variant(&good),
            "finished HLS was deleted on startup"
        );
        assert!(!partial.exists(), "interrupted quality survived startup");
        assert!(good.join("segment_000.ts").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hls_quality_jobs_share_one_original_download_gate_without_serializing_encodes() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-shared-source-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let a = manager.original_download_lock("44_552").await;
        let b = manager.original_download_lock("44_552").await;
        let other = manager.original_download_lock("44_553").await;
        assert!(
            Arc::ptr_eq(&a, &b),
            "different qualities race on the same .part input"
        );
        assert!(
            !Arc::ptr_eq(&a, &other),
            "different media must download independently"
        );

        let first = a.lock_owned().await;
        assert!(
            b.try_lock().is_err(),
            "concurrent HLS quality could truncate shared MP4"
        );
        assert!(
            other.try_lock().is_ok(),
            "independent movie downloads were serialized"
        );
        drop(first);
        assert!(
            b.try_lock().is_ok(),
            "a finished download never released its gate"
        );

        drop(b);
        let replacement = manager.original_download_lock("44_552").await;
        assert!(replacement.try_lock().is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cache_eviction_preserves_ongoing_transcode_input_and_segments() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-eviction-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 8,
            message_id: 953,
            quality: "480p".to_string(),
        };
        let (job, _) = manager.get_or_create_job(&key).await;
        assert!(job.lock().await.in_flight);

        let input = manager.original_path(&key.file_key());
        let active_dir = manager.hls_output_dir(&key.file_key(), &key.quality);
        let active_segment = active_dir.join("segment_000.ts");
        let other = manager
            .cache_root
            .join(HLS_DIR)
            .join("not-active")
            .join("stale.ts");
        std::fs::create_dir_all(input.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&input, vec![1_u8; 64]).unwrap();
        std::fs::write(&active_segment, vec![2_u8; 64]).unwrap();
        std::fs::write(&other, vec![3_u8; 64]).unwrap();
        manager.set_max_cache_bytes(1).await;
        manager.evict_lru().await;
        assert!(input.is_file(), "eviction deleted an active FFmpeg input");
        assert!(
            active_segment.is_file(),
            "eviction deleted an actively generated HLS segment"
        );
        assert!(!other.exists(), "inactive cache must remain evictable");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_hls_playlist_cannot_be_promoted_to_cached_ready() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-partial-qa-{:032x}",
            rand::random::<u128>()
        ));
        let dir = root.join(HLS_DIR).join("12_15").join("480p");
        std::fs::create_dir_all(&dir).unwrap();
        let playlist = dir.join("index.m3u8");
        std::fs::write(&playlist, "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n").unwrap();
        std::fs::write(dir.join("segment_000.ts"), b"ts").unwrap();
        assert!(
            !finished_hls_variant(&dir),
            "unfinished output must not be ready"
        );
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n#EXTINF:4.0,\nsegment_001.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert!(
            !finished_hls_variant(&dir),
            "missing referenced segments invalidate the entire output"
        );
        std::fs::write(dir.join("segment_001.ts"), b"ts").unwrap();
        assert!(
            finished_hls_variant(&dir),
            "complete VOD playlist is reusable"
        );
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n../../escape.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert!(!finished_hls_variant(&dir), "segment path escape rejected");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transcode_cache_commands_require_registered_arc_manager() {
        fn assert_info<T: Send + Sync + 'static, F>(_: impl Fn(tauri::State<'static, T>) -> F) {
            assert_eq!(TypeId::of::<T>(), TypeId::of::<Arc<TranscodeManager>>());
        }
        fn assert_limit<T: Send + Sync + 'static, F>(
            _: impl Fn(u32, tauri::State<'static, T>) -> F,
        ) {
            assert_eq!(TypeId::of::<T>(), TypeId::of::<Arc<TranscodeManager>>());
        }
        fn assert_clear<T: Send + Sync + 'static, F>(
            _: impl Fn(Option<String>, Option<String>, tauri::State<'static, T>) -> F,
        ) {
            assert_eq!(TypeId::of::<T>(), TypeId::of::<Arc<TranscodeManager>>());
        }
        assert_info(cmd_get_transcode_cache_info);
        assert_info(cmd_get_detailed_transcode_cache);
        assert_limit(cmd_set_transcode_cache_limit);
        assert_clear(cmd_clear_transcode_cache);
    }

    #[tokio::test]
    async fn cache_clear_rejects_path_traversal_and_active_transcodes() {
        for invalid in [
            "../other",
            "/tmp/escape",
            "0_1/../else",
            "0_+1",
            "01_1",
            "0_2147483648",
            "-5_0_2",
        ] {
            assert!(
                validate_cache_clear_selection(Some(invalid), None).is_err(),
                "dangerous or noncanonical cache file selection was accepted: {invalid}"
            );
        }
        assert!(validate_cache_clear_selection(None, Some("480p")).is_err());
        assert!(validate_cache_clear_selection(Some("0_1"), Some("../480p")).is_err());
        assert!(validate_cache_clear_selection(Some("-42_15"), Some("360p")).is_ok());
        assert!(validate_cache_clear_selection(None, None).is_ok());

        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-clear-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 14,
            message_id: 83,
            quality: "480p".to_string(),
        };
        let (job, _) = manager.get_or_create_job(&key).await;
        {
            let job = job.lock().await;
            assert!(cache_clear_conflicts_with_job(None, &job));
            assert!(cache_clear_conflicts_with_job(Some("14_83"), &job));
            assert!(!cache_clear_conflicts_with_job(Some("14_84"), &job));
        }
        job.lock().await.in_flight = false;
        {
            let job_guard = job.lock().await;
            assert!(!cache_clear_conflicts_with_job(None, &job_guard));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ffmpeg_capability_command_uses_the_registered_shared_manager() {
        assert_capability_state(cmd_get_transcode_capabilities);
    }

    #[test]
    fn hls_cache_file_responses_stream_instead_of_loading_entire_segments() {
        let source = include_str!("transcode.rs");
        let handler = source
            .split_once("async fn serve_hls_file(")
            .unwrap()
            .1
            .split_once("/// GET /hls/{file_key}/master.m3u8")
            .unwrap()
            .0;
        assert!(
            handler.contains("NamedFile::open_async(")
                && !handler.contains("std::fs::read(&file_path)"),
            "large cached HLS segments must stream from disk without eager full-file allocation"
        );
    }

    #[test]
    fn master_hls_playlist_applies_the_existing_containment_check() {
        let source = include_str!("transcode.rs");
        let handler = source
            .split_once("async fn hls_master_playlist(")
            .unwrap()
            .1
            .split_once("async fn hls_playlist(")
            .unwrap()
            .0;
        assert!(
            handler.contains("manager.validate_hls_path("),
            "master playlist must validate candidate variants within its HLS cache root"
        );
    }

    #[actix_web::test]
    async fn media_playlist_authorizes_relative_segments_for_native_hls() {
        // Native Safari/OS HLS does not have hls.js xhrSetup to append the
        // loopback bearer token to a relative segment URL.
        let root = std::env::temp_dir().join(format!(
            "terarelay-native-hls-auth-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = Arc::new(TranscodeManager::new(root.clone()));
        let dir = root.join(HLS_DIR).join("qa-native").join("360p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.0,\nsegment_000.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        std::fs::write(dir.join("segment_000.ts"), b"synthetic-segment").unwrap();
        let token = "qa a&b+#?=";
        let encoded = urlencoding::encode(token);
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(web::Data::new(manager))
                .app_data(web::Data::new(StreamTokenData {
                    token: token.into(),
                }))
                .service(hls_playlist)
                .service(hls_segment),
        )
        .await;
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri(&format!("/hls/qa-native/360p/index.m3u8?token={encoded}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), actix_web::http::StatusCode::OK);
        let bytes = actix_web::test::read_body(response).await;
        let playlist = std::str::from_utf8(&bytes).unwrap();
        let segment_uri = playlist
            .lines()
            .find(|line| line.starts_with("segment_"))
            .unwrap();
        assert!(
            segment_uri.starts_with("segment_000.ts?token="),
            "native HLS playlist must authorize its child segment requests"
        );
        assert_eq!(
            segment_uri.split_once("?token=").unwrap().1,
            encoded.as_ref(),
            "native HLS token must be percent-encoded without corruption"
        );
        let segment_path = format!("/hls/qa-native/360p/{segment_uri}");
        let segment = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri(&segment_path)
                .to_request(),
        )
        .await;
        assert_eq!(segment.status(), actix_web::http::StatusCode::OK);
        assert_eq!(
            &actix_web::test::read_body(segment).await[..],
            b"synthetic-segment"
        );
        let anonymous = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/hls/qa-native/360p/segment_000.ts")
                .to_request(),
        )
        .await;
        assert_eq!(anonymous.status(), actix_web::http::StatusCode::FORBIDDEN);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_hls_manifest_rejects_uncontained_and_keyed_media_references() {
        let token = "qa a&b+#?=";
        assert!(authenticated_hls_media_playlist(
            "#EXTM3U\n#EXTINF:4.0,\nhttps://example.invalid/segment_000.ts\n",
            token,
        )
        .is_none());
        assert!(authenticated_hls_media_playlist(
            "#EXTM3U\n#EXTINF:4.0,\n../segment_000.ts\n",
            token,
        )
        .is_none());
        assert!(authenticated_hls_media_playlist(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=key.bin\n",
            token,
        )
        .is_none());
        let body = authenticated_hls_media_playlist(
            "#EXTM3U\n#EXTINF:4.0,\nsegment_001.ts\n#EXT-X-ENDLIST\n",
            token,
        )
        .unwrap();
        assert!(body.contains(&format!(
            "segment_001.ts?token={}",
            urlencoding::encode(token)
        )));
    }

    #[actix_web::test]
    async fn master_hls_playlist_never_advertises_an_incomplete_variant() {
        // FFmpeg creates index.m3u8 while still writing the segment files.
        // Exposing that index through the master playlist is false readiness.
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-incomplete-master-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = Arc::new(TranscodeManager::new(root.clone()));
        let quality = root.join(HLS_DIR).join("qa-incomplete").join("360p");
        std::fs::create_dir_all(&quality).unwrap();
        std::fs::write(
            quality.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n",
        )
        .unwrap();
        std::fs::write(quality.join("segment_000.ts"), b"in-progress").unwrap();
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(web::Data::new(manager))
                .app_data(web::Data::new(StreamTokenData {
                    token: "qa-master-private-token".into(),
                }))
                .service(hls_master_playlist),
        )
        .await;
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/hls/qa-incomplete/master.m3u8?token=qa-master-private-token")
                .to_request(),
        )
        .await;
        let status = response.status();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            status,
            actix_web::http::StatusCode::NOT_FOUND,
            "master HLS advertised an incomplete FFmpeg output as a playable quality"
        );
    }

    #[actix_web::test]
    async fn master_hls_playlist_still_lists_valid_contained_variants() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-master-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = Arc::new(TranscodeManager::new(root.clone()));
        let quality_dir = root.join(HLS_DIR).join("qa-valid").join("360p");
        std::fs::create_dir_all(&quality_dir).unwrap();
        std::fs::write(
            quality_dir.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:4.0,\nsegment_000.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        std::fs::write(quality_dir.join("segment_000.ts"), b"completed-vod-segment").unwrap();
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(web::Data::new(manager))
                .app_data(web::Data::new(StreamTokenData {
                    token: "qa a&b+#?=".into(),
                }))
                .service(hls_master_playlist),
        )
        .await;
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri(&format!(
                    "/hls/qa-valid/master.m3u8?token={}",
                    urlencoding::encode("qa a&b+#?=")
                ))
                .to_request(),
        )
        .await;
        let status = response.status();
        let body = actix_web::test::read_body(response).await;
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(status, actix_web::http::StatusCode::OK);
        let manifest = String::from_utf8_lossy(&body);
        assert!(
            manifest.contains(&format!(
                "360p/index.m3u8?token={}",
                urlencoding::encode("qa a&b+#?=")
            )),
            "native master HLS playlist must authorize its child quality manifest"
        );
    }

    #[actix_web::test]
    async fn hls_segment_respects_byte_ranges_and_rejects_invalid_ranges() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-hls-range-qa-{:032x}",
            rand::random::<u128>()
        ));
        let manager = TranscodeManager::new(root.clone());
        let file = root
            .join(HLS_DIR)
            .join("qa-segment")
            .join("360p")
            .join("chunk0.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"abcdefghij").unwrap();
        let query = HlsQuery {
            token: Some("isolated-hls-test-token".into()),
        };
        let token_data = StreamTokenData {
            token: "isolated-hls-test-token".into(),
        };
        let request = actix_web::test::TestRequest::default()
            .insert_header(("Range", "bytes=2-5"))
            .to_http_request();
        let response = serve_hls_file(
            request.clone(),
            "qa-segment",
            "360p",
            Some("chunk0.ts"),
            &query,
            &manager,
            &token_data,
        )
        .await
        .respond_to(&request);
        let status = response.status();
        let cache_header = response
            .headers()
            .get("Cache-Control")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let range_header = response
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .ok()
            .expect("HLS response body was unreadable");

        let invalid_request = actix_web::test::TestRequest::default()
            .insert_header(("Range", "bytes=30-"))
            .to_http_request();
        let invalid = serve_hls_file(
            invalid_request.clone(),
            "qa-segment",
            "360p",
            Some("chunk0.ts"),
            &query,
            &manager,
            &token_data,
        )
        .await
        .respond_to(&invalid_request);
        let invalid_status = invalid.status();
        let invalid_range = invalid
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(status, actix_web::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            cache_header.as_deref(),
            Some("private, max-age=3600"),
            "token-protected HLS bytes may use a private client cache, not a shared public cache"
        );
        assert_eq!(range_header.as_deref(), Some("bytes 2-5/10"));
        assert_eq!(body.as_ref(), b"cdef");
        assert_eq!(
            invalid_status,
            actix_web::http::StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(invalid_range.as_deref(), Some("bytes */10"));
    }
}
