use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
struct DestinationStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    is_directory: bool,
    is_symlink: bool,
    #[cfg(unix)]
    file_identity: (u64, u64, i64, i64),
}

async fn destination_stamp(path: &Path) -> Result<Option<DestinationStamp>, String> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot inspect download destination: {error}")),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(DestinationStamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        is_directory: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        #[cfg(unix)]
        file_identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
    }))
}

#[cfg(target_os = "linux")]
async fn reclaim_crashed_stages(parent: &Path, prefix: &str) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(parent)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".part"))
        else {
            continue;
        };
        let Some((pid, nonce)) = suffix.split_once('-') else {
            continue;
        };
        let (Ok(pid), Ok(_nonce)) = (pid.parse::<u32>(), nonce.parse::<u64>()) else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let alive = if pid == std::process::id() {
            true
        } else {
            match tokio::fs::metadata(format!("/proc/{pid}")).await {
                Ok(_) => true,
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            }
        };
        if alive {
            return Err("This download already has an active output writer. Wait for it to stop before retrying.".to_string());
        }
        // Only this transfer's private stage is reclaimed. Its destination
        // and TDLib's confirmed-part cache are never removed.
        tokio::fs::remove_file(entry.path())
            .await
            .map_err(|e| format!("Cannot reclaim interrupted download output: {e}"))?;
    }
    Ok(())
}

/// Own a unique sibling output. Transports write here; only verified complete
/// data is published, so their failures cannot truncate/delete the destination.
pub struct DownloadOutput {
    destination: PathBuf,
    original: Option<DestinationStamp>,
    staging: Option<PathBuf>,
    path_text: String,
    #[cfg(test)]
    prefix: String,
}

impl DownloadOutput {
    pub async fn create(destination: &str, transfer_id: &str) -> Result<Self, String> {
        let destination = PathBuf::from(destination);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Cannot create download destination directory: {e}"))?;
        let original = destination_stamp(&destination).await?;
        let identity = serde_json::to_vec(&(destination.to_string_lossy(), transfer_id))
            .map_err(|e| e.to_string())?;
        let prefix = format!(".terarelay-download-{:x}-", Sha256::digest(&identity));
        #[cfg(target_os = "linux")]
        reclaim_crashed_stages(parent, &prefix).await?;
        for _ in 0..8 {
            let staging = parent.join(format!(
                "{prefix}{}-{}.part",
                std::process::id(),
                rand::random::<u64>()
            ));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .await
            {
                Ok(file) => {
                    drop(file);
                    let path_text = staging.to_string_lossy().into_owned();
                    return Ok(Self {
                        destination,
                        original,
                        staging: Some(staging),
                        path_text,
                        #[cfg(test)]
                        prefix,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("Cannot create staged download output: {error}")),
            }
        }
        Err("Cannot allocate a unique download output".to_string())
    }

    pub fn path(&self) -> &str {
        &self.path_text
    }

    pub async fn publish(&mut self) -> Result<(), String> {
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| "Download output was already published".to_string())?;
        if destination_stamp(&self.destination).await? != self.original {
            return Err("Destination changed during download. Choose another save location before retrying.".to_string());
        }
        let file = tokio::fs::File::open(staging)
            .await
            .map_err(|e| format!("Cannot open completed download output: {e}"))?;
        file.sync_all()
            .await
            .map_err(|e| format!("Cannot sync completed download output: {e}"))?;
        drop(file);
        tokio::fs::rename(staging, &self.destination)
            .await
            .map_err(|e| format!("Cannot publish completed download: {e}"))?;
        self.staging = None;
        #[cfg(unix)]
        {
            let parent = self
                .destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if let Err(error) =
                std::fs::File::open(parent).and_then(|directory| directory.sync_all())
            {
                log::warn!("Completed download was saved but directory sync failed: {error}");
            }
        }
        Ok(())
    }
}

impl Drop for DownloadOutput {
    fn drop(&mut self) {
        if let Some(staging) = &self.staging {
            let _ = std::fs::remove_file(staging);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "terarelay-output-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn failures_and_delayed_cleanup_preserve_destination_and_newer_download() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        std::fs::write(&destination, b"previous complete file").unwrap();
        let first = DownloadOutput::create(destination.to_str().unwrap(), "first")
            .await
            .unwrap();
        let first_path = first.path().to_string();
        std::fs::write(&first_path, b"truncated transfer").unwrap();
        let mut second = DownloadOutput::create(destination.to_str().unwrap(), "second")
            .await
            .unwrap();
        std::fs::write(second.path(), b"verified new file").unwrap();
        second.publish().await.unwrap();
        drop(first);
        assert!(!std::path::Path::new(&first_path).exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified new file");
        let failed = DownloadOutput::create(destination.to_str().unwrap(), "failed")
            .await
            .unwrap();
        drop(failed);
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified new file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn publication_rejects_destination_changes_and_rename_errors() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        std::fs::write(&destination, b"old").unwrap();
        let mut stage = DownloadOutput::create(destination.to_str().unwrap(), "changed")
            .await
            .unwrap();
        std::fs::write(stage.path(), b"new download").unwrap();
        std::fs::write(&destination, b"someone changed this file").unwrap();
        assert!(stage
            .publish()
            .await
            .unwrap_err()
            .contains("Destination changed"));
        drop(stage);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"someone changed this file"
        );
        let directory = dir.join("existing-directory");
        std::fs::create_dir(&directory).unwrap();
        let mut stage = DownloadOutput::create(directory.to_str().unwrap(), "rename")
            .await
            .unwrap();
        std::fs::write(stage.path(), b"verified").unwrap();
        assert!(stage.publish().await.is_err());
        drop(stage);
        assert!(directory.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reopening_reclaims_only_owned_dead_process_stages() {
        let dir = scratch();
        let destination = dir.join("movie.bin");
        let first = DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
            .await
            .unwrap();
        assert!(
            DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
                .await
                .is_err()
        );
        let abandoned = format!("{}4294967295-123.part", first.prefix);
        let abandoned = dir.join(abandoned);
        std::fs::rename(first.path(), &abandoned).unwrap();
        // Simulate a killed process: its destructor cannot remove the moved stage.
        drop(first);
        std::fs::write(dir.join("unrelated.part"), b"keep").unwrap();
        let resumed = DownloadOutput::create(destination.to_str().unwrap(), "same-transfer")
            .await
            .unwrap();
        assert!(!abandoned.exists());
        assert!(dir.join("unrelated.part").exists());
        drop(resumed);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
