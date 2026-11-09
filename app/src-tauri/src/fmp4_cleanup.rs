//! fMP4 cache disposal after player release, cancellation and crash.
//! Only our own strict "_job_<hex>" streaming window directories are removed.
use super::*;
use std::fs;

fn canonical_source(source: &str) -> bool {
    crate::transcode::validate_cache_clear_selection(Some(source), None).is_ok()
}
fn owned_fmp4_window(name: &str, source: &str) -> bool {
    if !canonical_source(source) {
        return false;
    }
    let Some(rest) = name.strip_prefix(&format!("{source}_a")) else {
        return false;
    };
    let Some((audio, after_audio)) = rest.split_once("_s") else {
        return false;
    };
    if audio != "default" && (audio.is_empty() || !audio.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let Some((milliseconds, nonce)) = after_audio
        .split_once("_seek1_aac_job_")
        .or_else(|| after_audio.split_once("_seek2_avc_job_"))
    else {
        return false;
    };
    !milliseconds.is_empty()
        && milliseconds.bytes().all(|c| c.is_ascii_digit())
        && nonce.len() == 16
        && nonce.bytes().all(|c| c.is_ascii_hexdigit())
}
fn possible_source_from_window(name: &str) -> Option<&str> {
    let (source, _rest) = name.split_once("_a")?;
    if !canonical_source(source) {
        return None;
    }
    owned_fmp4_window(name, source).then_some(source)
}
fn valid_private_directory(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() && !info.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Unexpected streaming directory type (or symlink)".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Cannot inspect private fMP4 path: {e}")),
    }
}
fn remove_known_window(root: &Path, name: &str) -> Result<bool, String> {
    let Some(source) = possible_source_from_window(name) else {
        return Ok(false);
    };
    if !owned_fmp4_window(name, source) || !valid_private_directory(root)? {
        return Ok(false);
    }
    let path = root.join(name);
    if !valid_private_directory(&path)? {
        return Ok(false);
    }
    for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(false);
        };
        if !matches!(name, "output.mp4" | "output.mp4.part" | "output.mp4.ready") {
            return Ok(false); // Unknown files are never disposable evidence.
        }
        let info = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !info.is_file() || info.file_type().is_symlink() {
            return Ok(false);
        }
    }
    fs::remove_dir_all(&path).map_err(|e| format!("Could not discard fMP4 cache: {e}"))?;
    Ok(true)
}
fn purge_window_prefix(root: &Path, source: Option<&str>) -> Result<usize, String> {
    if !valid_private_directory(root)? {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(owner) = possible_source_from_window(name) else {
            continue;
        };
        if source.is_some_and(|source| source != owner) {
            continue;
        }
        if remove_known_window(root, name)? {
            removed += 1;
        }
    }
    Ok(removed)
}
/// Called once at startup: the old process has no running FFmpeg jobs/readers.
pub(crate) fn purge_previous_fmp4_cache(cache_root: &Path) {
    if let Err(error) = purge_window_prefix(&cache_root.join(FMP4_DIR), None) {
        log::warn!("Could not discard previous-session fMP4 cache: {error}");
    }
}

impl Fmp4RemuxState {
    /// Release the completed/partial windows after last-viewer close. A
    /// bounded retry allows FFmpeg to exit and HTTP range readers to finish.
    pub(crate) async fn release_stream_source(
        &self,
        manager: Arc<crate::transcode::TranscodeManager>,
        source: String,
    ) {
        if !canonical_source(&source) {
            return;
        }
        // Synchronize with begin_stream_session so a new viewer cannot
        // reopen the same movie between the last-viewer check and cancel.
        let _hls_jobs = manager.jobs.lock().await;
        let sessions = manager.sessions.lock().await;
        if sessions
            .viewers
            .get(&source)
            .is_some_and(|ids| !ids.is_empty())
        {
            return;
        }
        let mut active = self.active_sources.lock().await;
        for (session_key, entry) in active.iter_mut() {
            if !session_key.starts_with(&format!("{source}:")) {
                continue;
            }
            // Prevent an in-flight ffprobe result from installing a new remux
            // after this player has already closed.
            entry.generation = entry.generation.wrapping_add(1);
            if let Some(tx) = entry.cancel.take() {
                let _ = tx.send(());
            }
            entry.key = None;
        }
        drop(active);
        drop(sessions);
        drop(_hls_jobs);
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            for _ in 0..100 {
                match state.purge_source_if_idle(&manager, &source).await {
                    Ok(true) => return,
                    Ok(false) => tokio::time::sleep(Duration::from_millis(300)).await,
                    Err(error) => {
                        log::warn!("Deferred fMP4 stream cleanup: {error}");
                        return;
                    }
                }
            }
            log::warn!("fMP4 source cleanup deferred until app restart because a worker/reader stayed active");
        });
    }

    /// Return true once removed OR retained for a new viewer. False means a
    /// still-running remux worker or active HTTP response requires a retry.
    pub(crate) async fn purge_source_if_idle(
        &self,
        manager: &crate::transcode::TranscodeManager,
        source: &str,
    ) -> Result<bool, String> {
        if !canonical_source(source) {
            return Err("Invalid fMP4 source".into());
        }
        // Same ordering as TeraRelay's HLS player begin/release. Holding these
        // locks guarantees a new viewer cannot reserve a deleted window.
        let _hls_jobs = manager.jobs.lock().await;
        let sessions = manager.sessions.lock().await;
        if sessions
            .viewers
            .get(source)
            .is_some_and(|ids| !ids.is_empty())
        {
            return Ok(true); // Reopened; leave its files intact.
        }
        let active = self.active_sources.lock().await;
        let jobs = self.jobs.lock().await;
        let readers = self
            .http_readers
            .lock()
            .map_err(|_| "fMP4 reader registry poisoned")?;
        let source_prefix = format!("{source}_a");
        if active
            .iter()
            .any(|(key, entry)| key.starts_with(&format!("{source}:")) && entry.key.is_some())
            || jobs
                .iter()
                .any(|(key, status)| key.starts_with(&source_prefix) && status.is_none())
            || readers.keys().any(|key| key.starts_with(&source_prefix))
        {
            return Ok(false);
        }
        purge_window_prefix(&manager.cache_root.join(FMP4_DIR), Some(source))?;
        Ok(true)
    }

    /// Clear app-owned remux windows only if *every* native viewer, FFmpeg
    /// worker and HTTP reader has finished. Unknown files stay untouched.
    pub(crate) async fn clear_idle_fmp4_cache(
        &self,
        manager: &crate::transcode::TranscodeManager,
    ) -> Result<usize, String> {
        let _hls_jobs = manager.jobs.lock().await;
        let sessions = manager.sessions.lock().await;
        if sessions.viewers.values().any(|ids| !ids.is_empty()) {
            return Err("Close all video players before clearing streaming cache".into());
        }
        let active = self.active_sources.lock().await;
        let jobs = self.jobs.lock().await;
        let readers = self
            .http_readers
            .lock()
            .map_err(|_| "fMP4 reader registry poisoned")?;
        if active.values().any(|entry| entry.key.is_some())
            || jobs.values().any(Option::is_none)
            || !readers.is_empty()
        {
            return Err("Remux workers or video readers are still active".into());
        }
        purge_window_prefix(&manager.cache_root.join(FMP4_DIR), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        let r = std::env::temp_dir().join(format!(
            "terarelay-fmp4-release-{:032x}",
            rand::random::<u128>()
        ));
        fs::create_dir_all(r.join(FMP4_DIR)).unwrap();
        r
    }

    #[test]
    fn purge_only_strictly_owned_windows_and_keep_unknown_data() {
        let r = root();
        let folder = r.join(FMP4_DIR);
        let key = "12_34_adefault_s0_seek1_aac_job_0123456789abcdef";
        let other = "12_35_adefault_s0_seek1_aac_job_0123456789abcdef";
        for (name, bytes) in [(key, b"movie".as_slice()), (other, b"other".as_slice())] {
            fs::create_dir_all(folder.join(name)).unwrap();
            fs::write(folder.join(name).join("output.mp4"), bytes).unwrap();
        }
        fs::create_dir_all(folder.join("strange_user_folder")).unwrap();
        fs::write(folder.join("strange_user_folder/data"), b"keep").unwrap();
        assert_eq!(purge_window_prefix(&folder, Some("12_34")).unwrap(), 1);
        assert!(!folder.join(key).exists());
        assert!(folder.join(other).exists());
        assert!(folder.join("strange_user_folder/data").exists());
        fs::remove_dir_all(r).unwrap();
    }
    #[tokio::test]
    async fn active_http_reader_defers_cleanup_and_prevents_deletion() {
        let r = root();
        let manager = crate::transcode::TranscodeManager::new(r.clone());
        let state = Fmp4RemuxState::new();
        let file_key = "12_34_adefault_s0_seek1_aac_job_0123456789abcdef";
        fs::create_dir_all(r.join(FMP4_DIR).join(file_key)).unwrap();
        fs::write(r.join(FMP4_DIR).join(file_key).join("output.mp4"), b"movie").unwrap();
        let lease = state.begin_http_read(file_key).unwrap();
        assert!(!state.purge_source_if_idle(&manager, "12_34").await.unwrap());
        assert!(r.join(FMP4_DIR).join(file_key).exists());
        drop(lease);
        assert!(state.purge_source_if_idle(&manager, "12_34").await.unwrap());
        assert!(!r.join(FMP4_DIR).join(file_key).exists());
        fs::remove_dir_all(r).unwrap();
    }
    #[tokio::test]
    async fn two_players_remux_independently_without_cross_cancellation() {
        let state = Fmp4RemuxState::new();
        let first_source = "12_34:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second_source = "12_34:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let a = state.reserve_source(first_source).await;
        let b = state.reserve_source(second_source).await;
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();
        state
            .install_job(first_source, a, "window_one", Some(first_tx))
            .await
            .unwrap();
        state
            .install_job(second_source, b, "window_two", Some(second_tx))
            .await
            .unwrap();
        assert!(
            !state
                .cancel_key("window_two", Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
                .await
        );
        assert!(
            state
                .cancel_key("window_one", Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
                .await
        );
        assert!(first_rx.await.is_ok());
        assert!(matches!(
            second_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            state
                .cancel_key("window_two", Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
                .await
        );
        assert!(second_rx.await.is_ok());
    }

    #[test]
    fn refuse_symlinked_or_unrecognized_windows() {
        let r = root();
        let folder = r.join(FMP4_DIR);
        fs::create_dir_all(folder.join("12_34_adefault_s0_seek1_aac_job_0123456789abcdef"))
            .unwrap();
        fs::write(
            folder
                .join("12_34_adefault_s0_seek1_aac_job_0123456789abcdef")
                .join("not-ours"),
            b"keep",
        )
        .unwrap();
        assert_eq!(purge_window_prefix(&folder, None).unwrap(), 0);
        assert!(folder
            .join("12_34_adefault_s0_seek1_aac_job_0123456789abcdef/not-ours")
            .exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = root();
            symlink(
                &outside,
                folder.join("10_11_adefault_s0_seek1_aac_job_0123456789abcdef"),
            )
            .unwrap();
            assert!(purge_window_prefix(&folder, None).is_err());
            assert!(outside.exists());
            fs::remove_dir_all(outside).unwrap();
        }
        fs::remove_dir_all(r).unwrap();
    }
}
