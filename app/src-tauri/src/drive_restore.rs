//! Restores through native creation operations without replacing content identities.
use crate::commands::drive_metadata as metadata;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreKind {
    Folder,
    File,
    Symlink,
}
impl RestoreKind {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "folder" => Ok(Self::Folder),
            "file" => Ok(Self::File),
            "symlink" => Ok(Self::Symlink),
            _ => Err("Unknown Drive trash item type".into()),
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ancestor {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestorePlan {
    pub kind: RestoreKind,
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub ancestors: Vec<Ancestor>,
    pub target: Option<String>,
    source_updated: i64,
    source_deleted: i64,
}
type RestoreSourceRow = (Option<String>, String, Option<String>, i64, i64);

fn read_trash_timestamp(stmt: &sqlite::Statement<'_>, field: &str) -> Result<i64, String> {
    match stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("Corrupt Drive trash {field}: {e}"))?
    {
        sqlite::Value::Integer(value) if value >= 0 => Ok(value),
        _ => Err(format!("Corrupt Drive trash {field}")),
    }
}

fn read_trash_optional_text(
    stmt: &sqlite::Statement<'_>,
    field: &str,
) -> Result<Option<String>, String> {
    // SQLite TEXT affinity does not guarantee TEXT is the stored type. Also,
    // the sqlite crate may truncate a String at an embedded NUL. Compare its
    // UTF-8 bytes to SQLite's complete BLOB length before trusting a restore
    // destination or symlink target; no extra round-trip query is needed.
    let corrupt = || format!("Corrupt Drive trash {field}");
    let value = stmt
        .read::<sqlite::Value, _>(field)
        .map_err(|e| format!("{}: {e}", corrupt()))?;
    let length_column = format!("{field}_bytes");
    let bytes = match stmt
        .read::<sqlite::Value, _>(length_column.as_str())
        .map_err(|e| format!("{}: {e}", corrupt()))?
    {
        sqlite::Value::Null => None,
        sqlite::Value::Integer(bytes) if bytes >= 0 => usize::try_from(bytes).ok(),
        _ => return Err(corrupt()),
    };
    match (value, bytes) {
        (sqlite::Value::Null, None) => Ok(None),
        (sqlite::Value::String(value), Some(bytes)) if value.len() == bytes => Ok(Some(value)),
        _ => Err(corrupt()),
    }
}

fn source(
    conn: &sqlite::Connection,
    query: &str,
    id: &str,
) -> Result<Option<RestoreSourceRow>, String> {
    // Materialize aliased columns once, and measure the original complete
    // SQLite byte strings so embedded NULs cannot be silently truncated.
    let checked_query = format!(
        "SELECT source.*,                 length(CAST(source.parent AS BLOB)) AS parent_bytes,                 length(CAST(source.name AS BLOB)) AS name_bytes,                 length(CAST(source.target AS BLOB)) AS target_bytes          FROM ({query}) AS source"
    );
    let mut s = conn.prepare(checked_query).map_err(|e| e.to_string())?;
    s.bind((1, id)).map_err(|e| e.to_string())?;
    if s.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    Ok(Some((
        read_trash_optional_text(&s, "parent")?,
        read_trash_optional_text(&s, "name")?
            .ok_or_else(|| "Corrupt Drive trash name".to_string())?,
        read_trash_optional_text(&s, "target")?,
        read_trash_timestamp(&s, "updated_at")?,
        read_trash_timestamp(&s, "trashed_at")?,
    )))
}
pub(crate) fn plan(
    conn: &sqlite::Connection,
    kind: RestoreKind,
    id: &str,
) -> Result<RestorePlan, String> {
    let row=match kind {
        RestoreKind::Folder => source(conn,"SELECT parent_id AS parent,name,NULL AS target,updated_at,trashed_at FROM drive_directories WHERE id=? AND trashed_at IS NOT NULL",id)?,
        RestoreKind::Symlink => source(conn,"SELECT directory_id AS parent,display_name AS name,target,updated_at,trashed_at FROM drive_symlinks WHERE id=? AND trashed_at IS NOT NULL",id)?,
        RestoreKind::File => {
            let v2=source(conn,"SELECT e.directory_id AS parent,e.display_name AS name,NULL AS target,e.updated_at,e.trashed_at FROM drive_entries_v2 e JOIN drive_objects o ON o.object_id=e.object_id WHERE e.entry_id=? AND e.trashed_at IS NOT NULL",id)?;
            if v2.is_some() { v2 } else {
                source(conn,"SELECT e.directory_id AS parent,e.display_name AS name,NULL AS target,e.updated_at,e.trashed_at FROM drive_file_entries e JOIN logical_files f ON f.file_id=e.file_id WHERE e.entry_id=? AND e.trashed_at IS NOT NULL AND f.status='complete' AND NOT EXISTS (SELECT 1 FROM drive_entries_v2 shadow JOIN drive_objects shadow_object ON shadow_object.object_id=shadow.object_id WHERE shadow.entry_id=e.entry_id)",id)?
            }
        }
    };
    let (original_parent, original_name, target, source_updated, source_deleted) =
        row.ok_or_else(|| "Drive trash item is unavailable or already restored".to_string())?;
    if original_parent
        .as_deref()
        .is_some_and(|parent| parent.trim().is_empty() || parent.contains('\0'))
    {
        return Err("Corrupt Drive trash parent".to_string());
    }
    metadata::validate_drive_name(&original_name)?;
    if kind == RestoreKind::Symlink && target.is_none() {
        return Err("Corrupt Drive trash target".to_string());
    }
    if let Some(target) = target.as_deref() {
        crate::drive_posix::validate_target(target)?;
    }
    let parent_id = metadata::restore_parent_if_active(conn, original_parent.as_deref())?;
    let name = metadata::available_restored_name(
        conn,
        parent_id.as_deref(),
        &original_name,
        (kind == RestoreKind::Folder).then_some(id),
        (kind == RestoreKind::File).then_some(id),
    )?;
    let mut ancestors = Vec::new();
    let mut cursor = parent_id.clone();
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cursor {
        if ancestors.len() >= 1024 || !seen.insert(id.clone()) {
            return Err("Invalid Drive restore parent ancestry".into());
        }
        let folder = crate::drive_lookup::directory(conn, &id)?
            .ok_or_else(|| "Drive restore parent is unavailable".to_string())?;
        metadata::validate_drive_name(&folder.name)?;
        cursor = folder.parent_id;
        ancestors.push(Ancestor {
            id: folder.id,
            name: folder.name,
        });
    }
    ancestors.reverse();
    Ok(RestorePlan {
        kind,
        id: id.to_string(),
        parent_id,
        name,
        ancestors,
        target,
        source_updated,
        source_deleted,
    })
}
pub(crate) fn commit(conn: &sqlite::Connection, expected: &RestorePlan) -> Result<String, String> {
    conn.execute("SAVEPOINT native_restore_commit")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        if plan(conn, expected.kind, &expected.id)? != *expected {
            return Err(
                "Drive restore source or destination changed; retry restoration".to_string(),
            );
        }
        let name = metadata::restore_drive_trash_item(conn, expected.kind.as_str(), &expected.id)?;
        if name != expected.name {
            return Err("Drive restore destination changed".into());
        }
        Ok(name)
    })();
    crate::drive_posix::finish_savepoint(conn, "native_restore_commit", result)
}
pub(crate) struct PendingRestore {
    pub plan: RestorePlan,
    generation: u64,
    result: Mutex<Option<Result<String, String>>>,
}
impl PendingRestore {
    pub(crate) fn complete(&self, result: Result<String, String>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    }
}
#[derive(Default)]
pub(crate) struct RestoreRequests {
    requests: Mutex<HashMap<u32, Arc<PendingRestore>>>,
}
pub(crate) struct RestoreGuard<'a> {
    registry: &'a RestoreRequests,
    owner: u32,
    pub request: Arc<PendingRestore>,
}
impl RestoreRequests {
    pub(crate) fn contains(&self, owner: u32) -> Result<bool, String> {
        Ok(self
            .requests
            .lock()
            .map_err(|_| "Restore requests poisoned")?
            .contains_key(&owner))
    }
    pub(crate) fn reserve(
        &self,
        owner: u32,
        generation: u64,
        plan: RestorePlan,
    ) -> Result<RestoreGuard<'_>, String> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "Restore requests poisoned")?;
        if requests.contains_key(&owner) {
            return Err("A native restore is already running on this thread".into());
        }
        let request = Arc::new(PendingRestore {
            plan,
            generation,
            result: Mutex::new(None),
        });
        requests.insert(owner, request.clone());
        Ok(RestoreGuard {
            registry: self,
            owner,
            request,
        })
    }
    pub(crate) fn take(
        &self,
        owner: u32,
        generation: u64,
        parent: Option<&str>,
        name: &str,
        kind: RestoreKind,
        target: Option<&str>,
    ) -> Option<Result<Arc<PendingRestore>, String>> {
        let request = match self.requests.lock() {
            Ok(mut requests) => requests.remove(&owner)?,
            Err(_) => return Some(Err("Restore requests poisoned".into())),
        };
        if request.generation != generation
            || request.plan.parent_id.as_deref() != parent
            || request.plan.name != name
            || request.plan.kind != kind
            || request.plan.target.as_deref() != target
        {
            let error =
                "Native restore request does not match its mount or destination".to_string();
            request.complete(Err(error.clone()));
            return Some(Err(error));
        }
        Some(Ok(request))
    }
}
impl RestoreGuard<'_> {
    pub(crate) fn result(&self) -> Result<Option<Result<String, String>>, String> {
        Ok(self
            .request
            .result
            .lock()
            .map_err(|_| "Restore receipt poisoned")?
            .clone())
    }
}
impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut requests) = self.registry.requests.lock() {
            if requests
                .get(&self.owner)
                .is_some_and(|request| Arc::ptr_eq(request, &self.request))
            {
                requests.remove(&self.owner);
            }
        }
    }
}

pub(crate) const IDENTITY_XATTR: &str = "user.terarelay.restore-owner";

fn c_name(value: &str) -> Result<std::ffi::CString, String> {
    std::ffi::CString::new(value).map_err(|_| "Native restore path contains NUL".into())
}
fn open_child(parent: &std::fs::File, name: &str) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    metadata::validate_drive_name(name)?;
    let name = c_name(name)?;
    // O_PATH needs no read permission; O_NOFOLLOW|O_DIRECTORY refuses a symlink.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "Cannot open restore parent: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
pub(crate) fn open_parent(
    mount: &std::path::Path,
    plan: &RestorePlan,
    identity: &str,
) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(mount)
        .map_err(|e| format!("Cannot pin Drive restore mount: {e}"))?;
    let root_info = parent.metadata().map_err(|e| e.to_string())?;
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(parent.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let fs = unsafe { fs.assume_init() };
    if fs.f_type as u64 != 0x65735546 || root_info.ino() != 1 {
        return Err("Restore path is not the owned Drive mount".into());
    }
    let name = c_name(IDENTITY_XATTR)?;
    let mut value = [0u8; 128];
    let length = unsafe {
        libc::fgetxattr(
            parent.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if length < 0 || &value[..length.max(0) as usize] != identity.as_bytes() {
        return Err("Drive restore mount identity changed".into());
    }
    for ancestor in &plan.ancestors {
        let child = open_child(&parent, &ancestor.name)?;
        if child.metadata().map_err(|e| e.to_string())?.dev() != root_info.dev() {
            return Err("Drive restore parent crosses a different mount".into());
        }
        parent = child;
    }
    Ok(parent)
}
pub(crate) fn create_native(parent: &std::fs::File, plan: &RestorePlan) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    metadata::validate_drive_name(&plan.name)?;
    let name = c_name(&plan.name)?;
    let status = match plan.kind {
        RestoreKind::Folder => unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) },
        // A regular-file mknod requires no device privilege and never opens content.
        RestoreKind::File => unsafe {
            libc::mknodat(parent.as_raw_fd(), name.as_ptr(), libc::S_IFREG | 0o600, 0)
        },
        RestoreKind::Symlink => {
            let target = c_name(
                plan.target
                    .as_deref()
                    .ok_or("Missing restored symlink target")?,
            )?;
            unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) }
        }
    };
    if status != 0 {
        return Err(format!(
            "Native Drive restore failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> sqlite::Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute("CREATE TABLE logical_channels (
          logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER NOT NULL UNIQUE,
          name TEXT NOT NULL,role TEXT NOT NULL DEFAULT 'owner',
          storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,joined_at INTEGER NOT NULL);
          CREATE TABLE logical_files (
          file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,first_message_id INTEGER NOT NULL,
          display_name TEXT NOT NULL,total_size INTEGER NOT NULL,mime_type TEXT,
          chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,storage_version INTEGER NOT NULL DEFAULT 1,
          created_at INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'complete');").unwrap();
        metadata::init_drive_schema(&conn).unwrap();
        conn
    }
    fn revision(conn: &sqlite::Connection) -> i64 {
        let mut s = conn
            .prepare("SELECT revision FROM drive_state WHERE singleton=1")
            .unwrap();
        s.next().unwrap();
        s.read::<i64, _>(0).unwrap()
    }
    fn trashed_folder(conn: &sqlite::Connection) -> String {
        let item = metadata::create_drive_directory(conn, None, "restored").unwrap();
        metadata::trash_drive_directory(conn, &item.id).unwrap();
        item.id
    }
    fn fixture_plan() -> RestorePlan {
        RestorePlan {
            kind: RestoreKind::Folder,
            id: "owned".into(),
            parent_id: None,
            name: "restored".into(),
            ancestors: vec![],
            target: None,
            source_updated: 1,
            source_deleted: 1,
        }
    }
    #[test]
    fn native_restore_rejects_corrupt_trash_timestamps_before_any_mutation() {
        for (kind, field, value) in [
            (RestoreKind::Folder, "updated_at", "-1"),
            (RestoreKind::Folder, "trashed_at", "X'FF'"),
            (RestoreKind::Folder, "updated_at", "0.75"),
            (RestoreKind::Symlink, "trashed_at", "-1"),
            (RestoreKind::Symlink, "updated_at", "'not-a-timestamp'"),
            (RestoreKind::Symlink, "trashed_at", "0.75"),
        ] {
            let conn = db();
            let (id, table) = match kind {
                RestoreKind::Folder => (trashed_folder(&conn), "drive_directories"),
                RestoreKind::Symlink => {
                    let link =
                        metadata::create_drive_symlink(&conn, None, "link.txt", "../valid-target")
                            .unwrap();
                    metadata::trash_drive_symlink(&conn, &link.id).unwrap();
                    (link.id, "drive_symlinks")
                }
                RestoreKind::File => unreachable!(),
            };
            assert!(plan(&conn, kind, &id).is_ok(), "valid deleted entry");
            let before = revision(&conn);
            let mut update = conn
                .prepare(format!("UPDATE {table} SET {field}={value} WHERE id=?"))
                .unwrap();
            update.bind((1, id.as_str())).unwrap();
            update.next().unwrap();
            drop(update);
            assert!(
                plan(&conn, kind, &id)
                    .unwrap_err()
                    .contains("Corrupt Drive trash"),
                "invalid {kind:?} {field}={value} must fail closed"
            );
            assert_eq!(
                revision(&conn),
                before,
                "planning corrupt restore must not mutate Drive revision"
            );
        }
    }

    #[test]
    fn native_restore_rejects_corrupt_parent_name_and_target_without_mutation() {
        // TEXT columns can persist raw BLOBs; typed String reads must never
        // coerce their bytes into a restore parent, filename or link target.
        for (kind, column, value) in [
            (RestoreKind::Folder, "parent_id", "X'0102'"),
            (RestoreKind::Folder, "parent_id", "''"),
            (RestoreKind::Folder, "parent_id", "'   '"),
            (RestoreKind::Folder, "parent_id", "char(9) || char(10)"),
            (RestoreKind::Folder, "parent_id", "'a' || char(0) || 'b'"),
            (RestoreKind::Folder, "name", "X'666f6f'"),
            (RestoreKind::Symlink, "directory_id", "X'0102'"),
            (RestoreKind::Symlink, "directory_id", "''"),
            (RestoreKind::Symlink, "directory_id", "'   '"),
            (RestoreKind::Symlink, "directory_id", "char(9) || char(10)"),
            (RestoreKind::Symlink, "display_name", "X'6c696e6b'"),
            (RestoreKind::Symlink, "target", "X'746172676574'"),
        ] {
            let conn = db();
            let (id, table) = match kind {
                RestoreKind::Folder => (trashed_folder(&conn), "drive_directories"),
                RestoreKind::Symlink => {
                    let link =
                        metadata::create_drive_symlink(&conn, None, "link.txt", "../valid-target")
                            .unwrap();
                    metadata::trash_drive_symlink(&conn, &link.id).unwrap();
                    (link.id, "drive_symlinks")
                }
                RestoreKind::File => unreachable!(),
            };
            assert!(plan(&conn, kind, &id).is_ok(), "valid deleted item");
            let before = revision(&conn);
            let mut update = conn
                .prepare(format!("UPDATE {table} SET {column}={value} WHERE id=?"))
                .unwrap();
            update.bind((1, id.as_str())).unwrap();
            update.next().unwrap();
            drop(update);
            let error = match plan(&conn, kind, &id) {
                Ok(accepted) => {
                    panic!("corrupt restore {kind:?} {column}={value} accepted: {accepted:?}")
                }
                Err(error) => error,
            };
            assert!(
                error.contains("Corrupt Drive trash"),
                "invalid {kind:?} {column}={value} escaped restore validation: {error}"
            );
            assert_eq!(
                revision(&conn),
                before,
                "corrupt restore source must not mutate Drive revision"
            );
        }
    }

    #[test]
    fn native_restore_plan_is_read_only_and_preserves_nested_parent() {
        let conn = db();
        let parent = metadata::create_drive_directory(&conn, None, "project").unwrap();
        let item = metadata::create_drive_directory(&conn, Some(&parent.id), "restored").unwrap();
        metadata::trash_drive_directory(&conn, &item.id).unwrap();
        let before_revision = revision(&conn);
        let p =
            plan(&conn, RestoreKind::Folder, &item.id).expect("restore must have a read-only plan");
        assert_eq!(p.parent_id, Some(parent.id.clone()));
        assert_eq!(
            p.ancestors,
            vec![Ancestor {
                id: parent.id,
                name: "project".into()
            }]
        );
        assert_eq!(revision(&conn), before_revision);
        assert!(crate::drive_lookup::directory(&conn, &item.id)
            .unwrap()
            .is_none());
    }
    #[test]
    fn native_restore_commit_preserves_symlink_identity_and_target() {
        let conn = db();
        let item =
            metadata::create_drive_symlink(&conn, None, "link", "../outside/never-follow").unwrap();
        metadata::trash_drive_symlink(&conn, &item.id).unwrap();
        let p = plan(&conn, RestoreKind::Symlink, &item.id).unwrap();
        assert_eq!(p.target.as_deref(), Some("../outside/never-follow"));
        assert_eq!(commit(&conn, &p).unwrap(), "link");
        let restored = crate::drive_posix::symlink_by_id(&conn, &item.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.target, item.target);
        assert_eq!(restored.created_at, item.created_at);
        assert!(commit(&conn, &p).is_err());
    }
    #[test]
    fn native_restore_revalidates_collision_without_overwriting() {
        let conn = db();
        let id = trashed_folder(&conn);
        let p = plan(&conn, RestoreKind::Folder, &id).unwrap();
        let occupied = metadata::create_drive_directory(&conn, None, "RESTORED").unwrap();
        assert!(commit(&conn, &p).is_err());
        assert!(crate::drive_lookup::directory(&conn, &id)
            .unwrap()
            .is_none());
        assert!(crate::drive_lookup::directory(&conn, &occupied.id)
            .unwrap()
            .is_some());
        let retry = plan(&conn, RestoreKind::Folder, &id).unwrap();
        assert_ne!(retry.name.to_lowercase(), "restored");
        assert_eq!(commit(&conn, &retry).unwrap(), retry.name);
    }
    #[test]
    fn native_restore_rejects_parent_move_after_planning() {
        let conn = db();
        let parent = metadata::create_drive_directory(&conn, None, "project").unwrap();
        let link =
            metadata::create_drive_symlink(&conn, Some(&parent.id), "link", "target").unwrap();
        metadata::trash_drive_symlink(&conn, &link.id).unwrap();
        let p = plan(&conn, RestoreKind::Symlink, &link.id).unwrap();
        metadata::rename_drive_directory(&conn, &parent.id, None, "moved").unwrap();
        assert!(commit(&conn, &p).is_err());
        assert!(crate::drive_posix::symlink_by_id(&conn, &link.id)
            .unwrap()
            .is_none());
    }
    #[test]
    fn native_restore_requests_are_one_use_and_scoped() {
        let requests = RestoreRequests::default();
        let p = fixture_plan();
        {
            let guard = requests
                .reserve(42, 7, p.clone())
                .expect("native dispatch must reserve an owned slot");
            assert!(requests.reserve(42, 7, p.clone()).is_err());
            assert!(requests.take(43, 7, None, &p.name, p.kind, None).is_none());
            let owned = requests
                .take(42, 7, None, &p.name, p.kind, None)
                .unwrap()
                .unwrap();
            assert!(requests.take(42, 7, None, &p.name, p.kind, None).is_none());
            owned.complete(Ok(p.name.clone()));
            assert_eq!(guard.result().unwrap(), Some(Ok(p.name.clone())));
        }
        let _next = requests.reserve(42, 8, p).unwrap();
    }
    #[test]
    fn native_restore_rolls_back_when_dirty_update_fails() {
        let conn = db();
        let id = trashed_folder(&conn);
        let p = plan(&conn, RestoreKind::Folder, &id).unwrap();
        let before = revision(&conn);
        conn.execute("CREATE TRIGGER reject_dirty BEFORE UPDATE ON drive_state BEGIN SELECT RAISE(FAIL,'injected dirty failure'); END;").unwrap();
        assert!(commit(&conn, &p).is_err());
        assert!(crate::drive_lookup::directory(&conn, &id)
            .unwrap()
            .is_none());
        assert_eq!(revision(&conn), before);
        assert!(metadata::list_drive_trash(&conn)
            .unwrap()
            .iter()
            .any(|item| item.id == id));
    }
    #[test]
    fn native_restore_deleted_parent_uses_root_and_avoids_symlink_collision() {
        let conn = db();
        let parent = metadata::create_drive_directory(&conn, None, "project").unwrap();
        let link =
            metadata::create_drive_symlink(&conn, Some(&parent.id), "link", "../owned").unwrap();
        metadata::trash_drive_symlink(&conn, &link.id).unwrap();
        metadata::trash_drive_directory(&conn, &parent.id).unwrap();
        let other = metadata::create_drive_symlink(&conn, None, "LINK", "other").unwrap();
        let p = plan(&conn, RestoreKind::Symlink, &link.id).unwrap();
        assert!(p.parent_id.is_none() && p.ancestors.is_empty());
        assert_ne!(p.name.to_lowercase(), "link");
        commit(&conn, &p).unwrap();
        assert_eq!(
            crate::drive_posix::symlink_by_id(&conn, &other.id)
                .unwrap()
                .unwrap()
                .target,
            "other"
        );
    }
    #[test]
    fn native_restore_changed_source_cannot_use_a_precomputed_plan() {
        let conn = db();
        let id = trashed_folder(&conn);
        let p = plan(&conn, RestoreKind::Folder, &id).unwrap();
        let mut update = conn
            .prepare("UPDATE drive_directories SET updated_at=updated_at+1 WHERE id=?")
            .unwrap();
        update.bind((1, id.as_str())).unwrap();
        update.next().unwrap();
        drop(update);
        assert!(commit(&conn, &p).is_err());
        assert!(crate::drive_lookup::directory(&conn, &id)
            .unwrap()
            .is_none());
    }
    #[test]
    fn native_restore_parent_traversal_refuses_symlinks_and_foreign_mounts() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("tr-restore-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(root.join("real")).unwrap();
        symlink("real", root.join("shortcut")).unwrap();
        let fd = std::fs::File::open(&root).unwrap();
        assert!(open_child(&fd, "real").is_ok());
        assert!(open_child(&fd, "shortcut").is_err());
        assert!(open_child(&fd, "..").is_err());
        assert!(open_parent(&root, &fixture_plan(), "wrong-owner").is_err());
        assert!(!root.join("restored").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_restore_wrong_generation_kind_or_path_fails_closed() {
        let requests = RestoreRequests::default();
        let p = fixture_plan();
        for (generation, parent, name, kind) in [
            (8, None, "restored", RestoreKind::Folder),
            (7, Some("wrong-parent"), "restored", RestoreKind::Folder),
            (7, None, "wrong-name", RestoreKind::Folder),
            (7, None, "restored", RestoreKind::Symlink),
        ] {
            let guard = requests.reserve(42, 7, p.clone()).unwrap();
            assert!(requests
                .take(42, generation, parent, name, kind, None)
                .unwrap()
                .is_err());
            assert!(matches!(guard.result().unwrap(), Some(Err(_))));
        }
        {
            let _guard = requests.reserve(42, 7, p.clone()).unwrap();
        }
        assert!(
            requests.reserve(42, 7, p).is_ok(),
            "dropping a failed dispatch must release its slot"
        );
    }
}
