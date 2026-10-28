use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::Manager;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BandwidthStats {
    pub date: String,
    pub up_bytes: u64,
    pub down_bytes: u64,
}

impl Default for BandwidthStats {
    fn default() -> Self {
        Self {
            date: Local::now().format("%Y-%m-%d").to_string(),
            up_bytes: 0,
            down_bytes: 0,
        }
    }
}

pub struct BandwidthManager {
    pub file_path: PathBuf,
    pub stats: Mutex<BandwidthStats>,
}

impl BandwidthManager {
    pub fn new(app_handle: &tauri::AppHandle) -> Self {
        // Resolve app data directory
        let app_data_dir = app_handle
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| PathBuf::from("data"));

        if !app_data_dir.exists() {
            let _ = std::fs::create_dir_all(&app_data_dir);
        }
        let file_path = app_data_dir.join("bandwidth.json");

        let stats = if file_path.exists() {
            let content = fs::read_to_string(&file_path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            BandwidthStats::default()
        };

        Self {
            file_path,
            stats: Mutex::new(stats),
        }
    }

    pub fn check_and_reset(&self) {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut stats = self.stats.lock().unwrap();
        if stats.date != today {
            println!(
                "[Bandwidth] New day detected. Resetting stats. Old date: {}, New date: {}",
                stats.date, today
            );
            stats.date = today;
            stats.up_bytes = 0;
            stats.down_bytes = 0;
            // Save immediately using the existing locked stats reference
            self.save_locked(&stats);
        }
    }

    /// Record an upload in the daily stats.
    /// Call release_up() if the transfer fails so the stats stay accurate.
    pub fn try_reserve_up(&self, bytes: u64) -> Result<(), String> {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.up_bytes;
        stats.up_bytes = previous
            .checked_add(bytes)
            .ok_or_else(|| "Upload bandwidth counter exceeds supported limits".to_string())?;
        if let Err(error) = self.try_save_locked(&stats) {
            stats.up_bytes = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Record a download in the daily stats.
    /// Call release_down() if the transfer fails so the stats stay accurate.
    pub fn try_reserve_down(&self, bytes: u64) -> Result<(), String> {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.down_bytes;
        stats.down_bytes = previous
            .checked_add(bytes)
            .ok_or_else(|| "Download bandwidth counter exceeds supported limits".to_string())?;
        if let Err(error) = self.try_save_locked(&stats) {
            stats.down_bytes = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Release reserved upload bandwidth after a failed transfer.
    pub fn release_up(&self, bytes: u64) {
        let mut stats = self.stats.lock().unwrap();
        stats.up_bytes = stats.up_bytes.saturating_sub(bytes);
        self.save_locked(&stats);
    }

    /// Release reserved download bandwidth after a failed transfer.
    pub fn release_down(&self, bytes: u64) {
        let mut stats = self.stats.lock().unwrap();
        stats.down_bytes = stats.down_bytes.saturating_sub(bytes);
        self.save_locked(&stats);
    }

    pub fn add_up(&self, bytes: u64) {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        stats.up_bytes = stats.up_bytes.saturating_add(bytes);
        self.save_locked(&stats);
    }

    pub fn add_down(&self, bytes: u64) {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        stats.down_bytes = stats.down_bytes.saturating_add(bytes);
        self.save_locked(&stats);
    }

    fn try_save_locked(&self, stats: &BandwidthStats) -> Result<(), String> {
        let json = serde_json::to_vec(stats).map_err(|error| error.to_string())?;
        let parent = self
            .file_path
            .parent()
            .ok_or_else(|| "Bandwidth stats path has no parent".to_string())?;
        let name = self
            .file_path
            .file_name()
            .ok_or_else(|| "Bandwidth stats path has no filename".to_string())?
            .to_string_lossy();
        // The temporary file is a sibling, so rename is a same-filesystem,
        // atomic replacement. create_new cannot follow a planted symlink.
        let temp_path = parent.join(format!(".{name}.{:032x}.tmp", rand::random::<u128>()));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&json)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, &self.file_path)?;
            // Syncing the directory after the rename makes the name durable on
            // Unix. A post-rename sync error must not report a rejected reserve:
            // the new counters are already visible, so rollback would diverge.
            #[cfg(unix)]
            if let Err(error) = fs::File::open(parent).and_then(|directory| directory.sync_all()) {
                log::warn!("Bandwidth stats directory sync failed after rename: {error}");
            }
            Ok(())
        })();
        if result.is_err() {
            // Keep the last complete snapshot, and clean only our random temp.
            let _ = fs::remove_file(&temp_path);
        }
        result.map_err(|error| format!("Could not persist bandwidth counters: {error}"))
    }

    fn save_locked(&self, stats: &BandwidthStats) {
        if let Err(error) = self.try_save_locked(stats) {
            log::warn!("{error}");
        }
    }

    pub fn get_stats(&self) -> BandwidthStats {
        self.check_and_reset();
        self.stats.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{BandwidthManager, BandwidthStats};
    use std::sync::Mutex;

    #[test]
    fn bandwidth_reservations_fail_closed_when_stats_cannot_be_persisted() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-bandwidth-io-qa-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        // An occupied directory cannot be overwritten with JSON as a file.
        let occupied_path = root.join("bandwidth.json");
        std::fs::create_dir(&occupied_path).unwrap();
        let manager = BandwidthManager {
            file_path: occupied_path.clone(),
            stats: Mutex::new(BandwidthStats::default()),
        };
        assert!(
            manager.try_reserve_up(10).is_err(),
            "upload reservations must reject failed persistence"
        );
        assert!(
            manager.try_reserve_down(20).is_err(),
            "download reservations must reject failed persistence"
        );
        let stats = manager.get_stats();
        assert_eq!(
            stats.up_bytes, 0,
            "failed persistence must roll back upload accounting"
        );
        assert_eq!(
            stats.down_bytes, 0,
            "failed persistence must roll back download accounting"
        );
        assert!(occupied_path.is_dir());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "failed replacement must clean its temporary files"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bandwidth_persistence_replaces_complete_snapshots_without_mutating_open_readers() {
        use std::io::Read;

        let root = std::env::temp_dir().join(format!(
            "terarelay-bandwidth-atomic-qa-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let file_path = root.join("bandwidth.json");
        let manager = BandwidthManager {
            file_path: file_path.clone(),
            stats: Mutex::new(BandwidthStats::default()),
        };

        manager.try_reserve_up(10).unwrap();
        let mut old_reader = std::fs::File::open(&file_path).unwrap();
        manager.try_reserve_up(20).unwrap();

        // An independent reader that opened the old inode must continue seeing
        // a complete old snapshot, not a truncated/overwritten JSON document.
        let mut snapshot = Vec::new();
        old_reader.read_to_end(&mut snapshot).unwrap();
        let old: BandwidthStats = serde_json::from_slice(&snapshot).unwrap();
        let latest: BandwidthStats =
            serde_json::from_slice(&std::fs::read(&file_path).unwrap()).unwrap();
        assert_eq!(old.up_bytes, 10, "open snapshot changed during persistence");
        assert_eq!(latest.up_bytes, 30);
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "successful replacement must leave no temporary files"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bandwidth_counters_reject_overflow_without_changing_persisted_totals() {
        let file_path = std::env::temp_dir().join(format!(
            "terarelay-bandwidth-overflow-qa-{:032x}.json",
            rand::random::<u128>()
        ));
        let manager = BandwidthManager {
            file_path: file_path.clone(),
            stats: Mutex::new(BandwidthStats::default()),
        };
        manager.try_reserve_up(u64::MAX).unwrap();
        manager.try_reserve_down(u64::MAX).unwrap();
        assert!(
            manager.try_reserve_up(1).is_err(),
            "upload counter overflow must fail"
        );
        assert!(
            manager.try_reserve_down(1).is_err(),
            "download counter overflow must fail"
        );
        let stats = manager.get_stats();
        assert_eq!(stats.up_bytes, u64::MAX);
        assert_eq!(stats.down_bytes, u64::MAX);
        let on_disk: BandwidthStats =
            serde_json::from_slice(&std::fs::read(&file_path).unwrap()).unwrap();
        assert_eq!(on_disk.up_bytes, u64::MAX);
        assert_eq!(on_disk.down_bytes, u64::MAX);
        manager.add_up(1);
        manager.add_down(1);
        assert_eq!(manager.get_stats().up_bytes, u64::MAX);
        assert_eq!(manager.get_stats().down_bytes, u64::MAX);
        std::fs::remove_file(&file_path).unwrap();
    }
}
