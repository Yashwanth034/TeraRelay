//! Temporary streaming-cache lifecycle, independent of Telegram/Drive storage.
//! A player gets a short-lived session ID. Last-viewer release cancels FFmpeg,
//! then removes ONLY app-owned HLS variants and their cached original, after
//! all workers stop. Crash leftovers are pruned on next launch.
use super::*;
use std::fs;

#[derive(Default)]
pub(crate) struct StreamSessions {
    pub viewers: HashMap<String, HashSet<String>>,
    pub discard_when_idle: HashSet<String>,
}

pub(super) fn valid_player_session(session_id: &str) -> bool {
    session_id.len() == 32 && session_id.bytes().all(|c| c.is_ascii_hexdigit())
}

fn safe_owned_stream_path(root: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(root) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err("Streaming cache path is not a private directory".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot inspect streaming cache path: {error}")),
    }
}

/// Remove only the HLS and original cache paths for a canonical media key.
/// Never follows a symlink or accepts a path supplied by the frontend.
pub(super) fn remove_owned_transcode_file(root: &Path, file_key: &str) -> Result<u64, String> {
    super::validate_cache_clear_selection(Some(file_key), None)?;
    if !safe_owned_stream_path(root)? {
        return Ok(0);
    }
    let mut removed = 0;
    for folder in [HLS_DIR, ORIGINALS_DIR] {
        let owned_root = root.join(folder);
        if !safe_owned_stream_path(&owned_root)? {
            continue;
        }
        let items = if folder == HLS_DIR {
            vec![owned_root.join(file_key)]
        } else {
            // A failed or disk-full download may leave an unfinished .part.
            // Remove both only after the worker has stopped.
            vec![
                owned_root.join(format!("{file_key}.mp4")),
                owned_root.join(format!("{file_key}.mp4.part")),
            ]
        };
        for item in items {
            let metadata = match fs::symlink_metadata(&item) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(format!("Cannot inspect private streaming cache: {err}")),
            };
            if metadata.file_type().is_symlink() {
                return Err("Refusing to follow a streaming-cache symlink".into());
            }
            if folder == HLS_DIR && metadata.is_dir() {
                fs::remove_dir_all(&item)
                    .map_err(|e| format!("Could not remove HLS cache: {e}"))?;
                removed += 1;
            } else if folder == ORIGINALS_DIR && metadata.is_file() {
                fs::remove_file(&item)
                    .map_err(|e| format!("Could not remove source cache: {e}"))?;
                removed += 1;
            } else {
                return Err("Unexpected streaming-cache entry type; no deletion performed".into());
            }
        }
    }
    Ok(removed)
}

/// Remove every previous-process HLS/source item we own, but never unknown
/// or user-created files. Used only while the caller has exclusive cache
/// access (startup or after all streams are idle).
pub(super) fn clear_known_transcode_cache(root: &Path) -> Result<usize, String> {
    let mut keys = HashSet::new();
    for section in [HLS_DIR, ORIGINALS_DIR] {
        let dir = root.join(section);
        if !safe_owned_stream_path(root)? || !safe_owned_stream_path(&dir)? {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let key = if section == HLS_DIR {
                name
            } else {
                match name
                    .strip_suffix(".mp4.part")
                    .or_else(|| name.strip_suffix(".mp4"))
                {
                    Some(key) => key,
                    None => continue,
                }
            };
            if super::validate_cache_clear_selection(Some(key), None).is_ok() {
                keys.insert(key.to_owned());
            }
        }
    }
    let mut removed = 0;
    for key in keys {
        removed += remove_owned_transcode_file(root, &key)? as usize;
    }
    Ok(removed)
}

pub(super) fn clean_previous_session_caches(root: &Path) {
    // A new process has no active HLS workers. Finished and interrupted cache
    // entries from a previous session are now disposable. Strictly constrain
    // cleanup to canonical app-owned paths, never downloaded/Drive files.
    for section in [HLS_DIR, ORIGINALS_DIR] {
        let dir = root.join(section);
        if !matches!(safe_owned_stream_path(root), Ok(true))
            || !matches!(safe_owned_stream_path(&dir), Ok(true))
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let key = if section == HLS_DIR {
                name
            } else {
                match name
                    .strip_suffix(".mp4.part")
                    .or_else(|| name.strip_suffix(".mp4"))
                {
                    Some(key) => key,
                    None => continue,
                }
            };
            if super::validate_cache_clear_selection(Some(key), None).is_ok() {
                if let Err(error) = remove_owned_transcode_file(root, key) {
                    log::warn!("Could not clean prior-session streaming data: {error}");
                }
            }
        }
    }
}

impl TranscodeManager {
    pub(super) async fn begin_stream_session(
        &self,
        file_key: &str,
        session_id: &str,
    ) -> Result<(), String> {
        super::validate_cache_clear_selection(Some(file_key), None)?;
        if !valid_player_session(session_id) {
            return Err("Invalid temporary streaming session".into());
        }
        // Order: jobs -> sessions, consistent with release/cleanup.
        let _jobs = self.jobs.lock().await;
        let mut sessions = self.sessions.lock().await;
        sessions
            .viewers
            .entry(file_key.to_string())
            .or_default()
            .insert(session_id.to_ascii_lowercase());
        sessions.discard_when_idle.remove(file_key);
        Ok(())
    }

    pub(crate) async fn has_stream_session(&self, file_key: &str, session_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .viewers
            .get(file_key)
            .is_some_and(|ids| ids.contains(&session_id.to_ascii_lowercase()))
    }

    /// A prepared HLS job belongs to its remaining viewers, regardless of
    /// which viewer initiated preparation.
    pub(super) async fn arm_prepared_stream(
        &self,
        file_key: &str,
        job: &Mutex<TranscodeJob>,
        cancel: tokio::sync::oneshot::Sender<()>,
    ) -> bool {
        let sessions = self.sessions.lock().await;
        let mut job = job.lock().await;
        let has_viewers = sessions
            .viewers
            .get(file_key)
            .is_some_and(|ids| !ids.is_empty());
        if !has_viewers || matches!(job.phase, JobPhase::Cancelled) {
            job.phase = JobPhase::Cancelled;
            job.in_flight = false;
            return false;
        }
        job.cancel_tx = Some(cancel);
        true
    }

    /// The last viewer stops all variants belonging to this same media file.
    /// Cancellation is asynchronous: the final worker triggers deletion only
    /// after it has exited. A second viewer keeps the source alive.
    pub(super) async fn end_stream_session(
        &self,
        file_key: &str,
        session_id: &str,
    ) -> Result<(), String> {
        super::validate_cache_clear_selection(Some(file_key), None)?;
        if !valid_player_session(session_id) {
            return Err("Invalid temporary streaming session".into());
        }
        let jobs = self.jobs.lock().await;
        let mut sessions = self.sessions.lock().await;
        let Some(active) = sessions.viewers.get_mut(file_key) else {
            return Ok(()); // Idempotent stale release.
        };
        if !active.remove(&session_id.to_ascii_lowercase()) {
            return Ok(());
        }
        if !active.is_empty() {
            return Ok(()); // Another player is still using this movie.
        }
        sessions.viewers.remove(file_key);
        sessions.discard_when_idle.insert(file_key.to_string());
        drop(sessions);
        for job in jobs.values() {
            let mut job = job.lock().await;
            if job.key.file_key() != file_key {
                continue;
            }
            if let Some(tx) = job.cancel_tx.take() {
                let _ = tx.send(());
            }
            if job.in_flight {
                job.phase = JobPhase::Cancelled;
            }
        }
        drop(jobs);
        self.finish_stream_disposal(file_key).await
    }

    /// Called after FFmpeg worker teardown and after any failed preparation.
    /// Holds the registry while deleting, so a new reservation cannot race.
    pub(super) async fn finish_stream_disposal(&self, file_key: &str) -> Result<(), String> {
        let mut jobs = self.jobs.lock().await;
        let mut sessions = self.sessions.lock().await;
        if !sessions.discard_when_idle.contains(file_key)
            || sessions
                .viewers
                .get(file_key)
                .is_some_and(|v| !v.is_empty())
        {
            return Ok(());
        }
        for job in jobs.values() {
            let job = job.lock().await;
            if job.key.file_key() == file_key && job.in_flight {
                return Ok(()); // Last job will clean up after it exits.
            }
        }
        // Preserve the pending marker if filesystem deletion fails; that
        // makes a future idle cleanup attempt safe and observable in logs.
        remove_owned_transcode_file(&self.cache_root, file_key)?;
        sessions.discard_when_idle.remove(file_key);
        jobs.retain(|_, job| match job.try_lock() {
            Ok(active) => active.key.file_key() != file_key,
            Err(_) => true, // Never discard an unrelated busy job.
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "terarelay-stream-disposal-{:032x}",
            rand::random::<u128>()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[tokio::test]
    async fn last_viewer_deletes_only_its_streaming_cache() {
        let root = temp_root();
        let manager = TranscodeManager::new(root.clone());
        fs::create_dir_all(root.join("hls/12_34/720p")).unwrap();
        fs::create_dir_all(root.join("hls/55_66/720p")).unwrap();
        fs::write(root.join("hls/12_34/720p/segment_000.ts"), b"buffer").unwrap();
        fs::write(root.join("hls/55_66/720p/segment_000.ts"), b"unrelated").unwrap();
        fs::write(root.join("originals/12_34.mp4"), b"source").unwrap();
        fs::write(root.join("originals/12_34.mp4.part"), b"half downloaded").unwrap();
        fs::write(root.join("download.mkv"), b"keep forever").unwrap();
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        manager.begin_stream_session("12_34", &a).await.unwrap();
        manager.begin_stream_session("12_34", &b).await.unwrap();
        manager.end_stream_session("12_34", &a).await.unwrap();
        assert!(root.join("originals/12_34.mp4").exists());
        manager.end_stream_session("12_34", &b).await.unwrap();
        assert!(!root.join("hls/12_34").exists());
        assert!(!root.join("originals/12_34.mp4").exists());
        assert!(!root.join("originals/12_34.mp4.part").exists());
        assert!(root.join("hls/55_66/720p/segment_000.ts").exists());
        assert_eq!(
            fs::read(root.join("download.mkv")).unwrap(),
            b"keep forever"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn in_flight_worker_defers_removal_and_new_viewer_cancels_disposal() {
        let root = temp_root();
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 12,
            message_id: 34,
            quality: "720p".into(),
        };
        let (job, _) = manager.get_or_create_job(&key).await;
        let file_key = key.file_key();
        fs::create_dir_all(root.join("hls/12_34/720p")).unwrap();
        fs::write(root.join("hls/12_34/720p/segment_000.ts"), b"inflight").unwrap();
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        manager.begin_stream_session(&file_key, &a).await.unwrap();
        manager.end_stream_session(&file_key, &a).await.unwrap();
        assert!(root.join("hls/12_34").exists());
        manager.begin_stream_session(&file_key, &b).await.unwrap();
        job.lock().await.in_flight = false;
        manager.finish_stream_disposal(&file_key).await.unwrap();
        assert!(
            root.join("hls/12_34").exists(),
            "reopened player owns the cache"
        );
        manager.end_stream_session(&file_key, &b).await.unwrap();
        assert!(!root.join("hls/12_34").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn prepared_transcode_starts_for_the_remaining_viewer() {
        let root = temp_root();
        let manager = TranscodeManager::new(root.clone());
        let key = TranscodeKey {
            folder_id: 12,
            message_id: 34,
            quality: "720p".into(),
        };
        let file_key = key.file_key();
        let first = "a".repeat(32);
        let second = "b".repeat(32);
        manager
            .begin_stream_session(&file_key, &first)
            .await
            .unwrap();
        let (job, _) = manager.get_or_create_job(&key).await;
        manager
            .begin_stream_session(&file_key, &second)
            .await
            .unwrap();
        manager.end_stream_session(&file_key, &first).await.unwrap();

        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        assert!(manager.arm_prepared_stream(&file_key, &job, cancel).await);
        assert!(matches!(
            cancelled.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        manager
            .end_stream_session(&file_key, &second)
            .await
            .unwrap();
        assert!(cancelled.await.is_ok());

        let (cancel, _) = tokio::sync::oneshot::channel();
        assert!(!manager.arm_prepared_stream(&file_key, &job, cancel).await);
        manager.finish_stream_disposal(&file_key).await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_validation_refuses_traversal_and_symlinked_hls() {
        let root = temp_root();
        fs::create_dir_all(root.join("hls")).unwrap();
        for key in ["../../abc", "/tmp", "12_34/../../", "bad", "12_34\\0"] {
            assert!(remove_owned_transcode_file(&root, key).is_err());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = temp_root();
            fs::write(external.join("do-not-delete"), b"safe").unwrap();
            symlink(&external, root.join("hls/12_34")).unwrap();
            assert!(remove_owned_transcode_file(&root, "12_34").is_err());
            assert!(external.join("do-not-delete").exists());
            fs::remove_dir_all(external).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_cleans_only_canonical_stream_files() {
        let root = temp_root();
        fs::create_dir_all(root.join("hls/10_20/720p")).unwrap();
        fs::create_dir_all(root.join("originals")).unwrap();
        fs::write(root.join("hls/10_20/720p/segment_000.ts"), b"old").unwrap();
        fs::write(root.join("originals/10_20.mp4"), b"old").unwrap();
        fs::write(root.join("unrelated-file"), b"precious").unwrap();
        clean_previous_session_caches(&root);
        assert!(!root.join("hls/10_20").exists());
        assert!(!root.join("originals/10_20.mp4").exists());
        assert!(root.join("unrelated-file").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
