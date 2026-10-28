//! Session-only protection for the Linux-mounted TeraRelay Drive.
//! All FUSE mutating callbacks must check the shared atomic before any
//! staging/SQLite/network operation. This is not a cloud permissions feature.
use super::DriveMountState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use tauri::State;

#[derive(Debug, Serialize)]
pub struct SafeMountStatus {
    pub read_only: bool,
    pub mounted: bool,
    pub in_flight_uploads_may_finish: bool,
    pub scope: &'static str,
}

pub(super) fn writable_open(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
        || flags & (libc::O_TRUNC | libc::O_APPEND | libc::O_CREAT) != 0
}

#[cfg(test)]
pub(super) fn mutation_errno(read_only: bool) -> Result<(), i32> {
    if read_only {
        Err(libc::EROFS)
    } else {
        Ok(())
    }
}

impl DriveMountState {
    pub(super) fn safe_mount_read_only(&self) -> bool {
        self.inner.safe_mount_read_only.load(Ordering::SeqCst)
    }
    pub(super) fn set_safe_mount_read_only(&self, read_only: bool) {
        self.inner
            .safe_mount_read_only
            .store(read_only, Ordering::SeqCst);
    }
}

#[tauri::command]
pub fn cmd_drive_safe_mount_status(mount: State<'_, DriveMountState>) -> SafeMountStatus {
    SafeMountStatus {
        read_only: mount.safe_mount_read_only(),
        mounted: mount.status().mounted,
        in_flight_uploads_may_finish: true,
        scope: "linux_fuse_mount_only",
    }
}

#[tauri::command]
pub fn cmd_drive_safe_mount_set(
    mount: State<'_, DriveMountState>,
    read_only: bool,
) -> SafeMountStatus {
    mount.set_safe_mount_read_only(read_only);
    cmd_drive_safe_mount_status(mount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn true_readonly_open_is_allowed_and_truncation_is_not() {
        assert!(!writable_open(libc::O_RDONLY));
        assert!(writable_open(libc::O_WRONLY));
        assert!(writable_open(libc::O_RDWR));
        assert!(writable_open(libc::O_RDONLY | libc::O_TRUNC));
        assert!(writable_open(libc::O_RDONLY | libc::O_CREAT));
        assert!(writable_open(libc::O_RDONLY | libc::O_APPEND));
    }

    #[test]
    fn mode_defaults_writable_and_is_shared_across_clone_without_filesystem_effects() {
        let root = std::env::temp_dir().join(format!("safe-fuse-{:032x}", rand::random::<u128>()));
        let mount = DriveMountState::new(0, "fixture".into(), PathBuf::from(&root)).unwrap();
        let clone = mount.clone();
        assert!(!mount.safe_mount_read_only());
        mount.set_safe_mount_read_only(true);
        assert!(clone.safe_mount_read_only());
        assert_eq!(
            mutation_errno(clone.safe_mount_read_only()),
            Err(libc::EROFS)
        );
        clone.set_safe_mount_read_only(false);
        assert_eq!(mutation_errno(mount.safe_mount_read_only()), Ok(()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_native_mutating_callbacks_are_guarded_before_effects() {
        // Source-level contract complements the real FUSE callback tests:
        // no mutation entrypoint can be added without deliberate policy review.
        let src = include_str!("drive_mount.rs");
        for (callback, next) in [
            ("fn mknod(", "fn lookup("),
            ("fn open(&mut self,", "fn read("),
            ("fn create(\n", "fn write(\n"),
            ("fn write(\n", "fn flush(\n"),
            ("fn setattr(\n", "fn mkdir(\n"),
            ("fn mkdir(\n", "fn rename(\n"),
            ("fn rename(\n", "fn unlink("),
            ("fn unlink(", "fn rmdir("),
            ("fn rmdir(", "fn link("),
            ("fn link(", "fn symlink("),
            ("fn symlink(", "fn readlink("),
        ] {
            let start = src
                .rsplit_once(callback)
                .unwrap_or_else(|| panic!("missing {callback}"))
                .1;
            let body = start
                .split_once(next)
                .unwrap_or_else(|| panic!("missing end {next}"))
                .0;
            let gate = body
                .find("safe_mount_read_only()")
                .unwrap_or_else(|| panic!("FUSE {callback} missing safety gate"));
            assert!(gate < 1_000, "FUSE {callback} must guard before mutating");
            assert!(
                body[gate..].contains("reply.error(EROFS)"),
                "FUSE {callback} must return EROFS in protected mode"
            );
        }
    }

    #[test]
    fn protected_close_and_sync_retain_local_bytes_without_telegram_finalize() {
        let src = include_str!("drive_mount.rs");
        for (callback, next, prohibited) in [
            ("fn flush(\n", "fn fsync(\n", "flush_completed_chunks("),
            ("fn fsync(\n", "fn release(\n", "flush_completed_chunks("),
            ("fn release(\n", "fn setattr(\n", "finalize_pending_sync("),
        ] {
            let start = src
                .rsplit_once(callback)
                .unwrap_or_else(|| panic!("missing callback {callback}"))
                .1;
            let body = start
                .split_once(next)
                .unwrap_or_else(|| panic!("missing next callback {next}"))
                .0;
            let guard = body
                .find(if callback.starts_with("fn release(") {
                    "if self.mount_state.safe_mount_read_only()"
                } else {
                    "safe_mount_read_only()"
                })
                .unwrap_or_else(|| panic!("missing safe-mode stage retention in {callback}"));
            let cloud = body
                .find(prohibited)
                .unwrap_or_else(|| panic!("missing normal-mode action {prohibited}"));
            assert!(
                guard < cloud,
                "protected {callback} must branch before cloud finalization"
            );
            assert!(
                body[guard..cloud].contains("sync_local_staging_for_pending(")
                    || body[guard..cloud].contains("mark_drive_pending_closed("),
                "protected {callback} must retain local pending data"
            );
            if callback.starts_with("fn release(") {
                let before_close = &body[guard..cloud];
                let validated = before_close
                    .find("verified_pending_staging(")
                    .expect("protected release must validate private stage ownership");
                let fsynced = before_close
                    .find("sync_local_staging_for_pending(")
                    .expect("protected release must sync staged data to disk");
                let marked = before_close
                    .find("mark_drive_pending_closed(")
                    .expect("protected release must mark a retryable closed stage");
                assert!(
                    validated < fsynced && fsynced < marked,
                    "safe mode close must validate, sync, then mark stage closed"
                );
            }
        }
    }
}
