//! Read-only Drive staging integrity inspector. No network, repair, plaintext
//! reads, cloud mutation, or traversal of SQLite-supplied filesystem paths.
use crate::commands::drive_metadata::DrivePendingRecord;
use crate::db::DbConnection;
use serde::Serialize;
use std::path::Path;
use tauri::{AppHandle, Manager, State};

const MAX_PENDING: usize = 1_000;
const MAX_ISSUES: usize = 200;
const MAX_ENTRIES: usize = 1_024;

#[derive(Debug, Clone, Serialize)]
pub struct DriveHealthIssue {
    pub pending_id: String,
    pub name: String,
    pub severity: &'static str,
    pub code: &'static str,
    pub advice: &'static str,
}
#[derive(Debug, Clone, Serialize)]
pub struct DriveHealthReport {
    pub scanned: usize,
    pub closed: usize,
    pub open_writers: usize,
    pub truncated: bool,
    pub read_only: bool,
    pub issues: Vec<DriveHealthIssue>,
}

fn issue(
    report: &mut DriveHealthReport,
    id: &str,
    name: &str,
    severity: &'static str,
    code: &'static str,
    advice: &'static str,
) {
    if report.issues.len() >= MAX_ISSUES {
        report.truncated = true;
        return;
    }
    report.issues.push(DriveHealthIssue {
        pending_id: id.into(),
        name: name.into(),
        severity,
        code,
        advice,
    });
}

fn dirty(conn: &sqlite::Connection, id: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM drive_pending_ranges WHERE pending_id=? LIMIT 1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    stmt.bind((1, id)).map_err(|e| e.to_string())?;
    Ok(stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row)
}

/// A finite immutable candidate set is collected while holding SQLite, then
/// filesystem inspection can proceed WITHOUT the database mutex.
pub(crate) struct DriveHealthSnapshot {
    records: Vec<PendingCandidate>,
    truncated: bool,
}

struct PendingCandidate {
    id: String,
    record: Result<DrivePendingRecord, ()>,
    dirty: Result<bool, ()>,
}

/// Read at most MAX_PENDING+1 rows. Never touch stage files while SQLite is
/// locked; callers release DbConnection before calling inspect_snapshot.
pub(crate) fn collect_snapshot(conn: &sqlite::Connection) -> Result<DriveHealthSnapshot, String> {
    // Do not initialize/migrate schema in a read-only diagnosis. Missing or
    // corrupt metadata must fail explicitly, not be silently recreated.
    let mut query = conn
        .prepare(
            "SELECT id, directory_id, display_name, staging_path, size, format_version,
                         backing_channel_id, base_entry_id, encryption_version, crypto_id,
                         closed_at, created_at, updated_at
                  FROM drive_pending_files ORDER BY id LIMIT ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    query
        .bind((1, (MAX_PENDING + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    let mut truncated = false;
    while query.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if records.len() == MAX_PENDING {
            truncated = true;
            break;
        }
        let id = match query
            .read::<sqlite::Value, _>(0)
            .map_err(|e| e.to_string())?
        {
            sqlite::Value::String(id) if !id.is_empty() && !id.contains('\0') => id,
            _ => return Err("Corrupt Drive staging identity; manual diagnosis required".into()),
        };
        let record = crate::drive_lookup::read_pending_record(&query).map_err(|_| ());
        records.push(PendingCandidate {
            id,
            record,
            dirty: Ok(false),
        });
    }
    drop(query);
    // Dirty-range checks are run only for closed records and after the cursor
    // is finalized; this never repeats schema initialization per record.
    for candidate in &mut records {
        if let Ok(pending) = &candidate.record {
            if pending.closed_at.is_some() {
                candidate.dirty = dirty(conn, &candidate.id).map_err(|_| ());
            }
        }
    }
    Ok(DriveHealthSnapshot { records, truncated })
}

/// Read-only, limited traversal of ONLY validated app-owned private stage
/// directories. Does not hold SQLite while filesystem accesses may stall.
pub(crate) fn inspect_snapshot(
    snapshot: DriveHealthSnapshot,
    cache_root: &Path,
) -> DriveHealthReport {
    let mut report = DriveHealthReport {
        scanned: 0,
        closed: 0,
        open_writers: 0,
        truncated: snapshot.truncated,
        read_only: true,
        issues: Vec::new(),
    };
    for candidate in snapshot.records {
        let id = &candidate.id;
        report.scanned += 1;
        let pending = match candidate.record {
            Ok(p) => p,
            _ => {
                issue(&mut report, id, "(corrupt)", "critical", "invalid_metadata",
                    "Do not clear pending uploads; preserve the Drive database and request a diagnostic review.");
                continue;
            }
        };
        if pending.closed_at.is_none() {
            report.open_writers += 1;
            continue; // Never inspect a path that is actively being written.
        }
        report.closed += 1;
        let has_dirty = match candidate.dirty {
            Ok(v) => v,
            Err(_) => {
                issue(&mut report, id, &pending.display_name, "critical", "invalid_range_index",
                    "Preserve the Drive database; do not force a retry with an unreadable write-range index.");
                continue;
            }
        };
        // Reuse the native cleanup validator: it verifies the exact private
        // staging parent AND all app-owned ancestors with lstat (no symlinks).
        let owned = match crate::drive_stream::owned_finalized_staging_dir(cache_root, &pending) {
            Ok(dir) => dir,
            Err(_) => {
                issue(&mut report, id, &pending.display_name,
                    if has_dirty { "critical" } else { "warning" }, "missing_or_unsafe_stage",
                    "Do not recreate or delete this staging path; inspect the pending upload in a private QA session.");
                continue;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::symlink_metadata(&owned)
                .is_ok_and(|meta| meta.permissions().mode() & 0o077 != 0)
            {
                issue(
                    &mut report,
                    id,
                    &pending.display_name,
                    "critical",
                    "exposed_stage_directory",
                    "Protect staged plaintext from other local users before recovery.",
                );
            }
        }
        let leaves = match std::fs::read_dir(&owned) {
            Ok(value) => value,
            Err(_) => {
                issue(
                    &mut report,
                    id,
                    &pending.display_name,
                    "critical",
                    "unreadable_stage",
                    "Preserve this directory and inspect its ownership and permissions.",
                );
                continue;
            }
        };
        let mut local_data = false;
        let mut n = 0;
        for entry in leaves {
            n += 1;
            if n > MAX_ENTRIES {
                report.truncated = true;
                issue(&mut report, id, &pending.display_name, "warning", "too_many_stage_entries",
                    "Review the private stage with bounded tooling; no automatic cleanup was attempted.");
                break;
            }
            let Ok(entry) = entry else {
                issue(
                    &mut report,
                    id,
                    &pending.display_name,
                    "warning",
                    "unreadable_stage_entry",
                    "Retry read-only diagnosis; do not assume this staged data is disposable.",
                );
                continue;
            };
            match entry.file_type() {
                Ok(kind) if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) => {
                    issue(&mut report, id, &pending.display_name, "critical", "unsafe_stage_entry",
                        "A symlink or special file appeared in staging; do not follow it or automatically clean it.");
                }
                Ok(kind) if kind.is_file() => {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    // The production ".pending" leaf is only an empty
                    // marker; QA may use a whole plaintext file with that
                    // name. Count it as content only when its size covers the
                    // recorded pending length. Chunk stages must hold bytes.
                    let stage_len = std::fs::symlink_metadata(entry.path())
                        .ok()
                        .filter(|meta| meta.file_type().is_file())
                        .map(|meta| meta.len());
                    if (name == ".pending"
                        && stage_len.is_some_and(|length| length >= pending.size))
                        || (name.starts_with("plain-")
                            && name.ends_with(".part")
                            && stage_len.is_some_and(|length| length > 0))
                    {
                        local_data = true;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if std::fs::symlink_metadata(entry.path()).is_ok_and(|m| {
                            m.file_type().is_file() && m.permissions().mode() & 0o077 != 0
                        }) {
                            issue(
                                &mut report,
                                id,
                                &pending.display_name,
                                "critical",
                                "exposed_stage_file",
                                "Protect staged plaintext from other local users before recovery.",
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => issue(
                    &mut report,
                    id,
                    &pending.display_name,
                    "warning",
                    "unreadable_stage_entry",
                    "Retry inspection without deleting data.",
                ),
            }
        }
        if has_dirty && !local_data && n <= MAX_ENTRIES {
            issue(&mut report, id, &pending.display_name, "critical", "missing_dirty_stage",
                "Local dirty byte ranges have no matching stage file. Preserve all metadata and inspect before retry.");
        }
        if report.issues.len() >= MAX_ISSUES {
            report.truncated = true;
            break;
        }
    }
    report
}

/// Explicit, user-requested diagnostic IPC; never scheduled automatically.
/// Resolves the trusted app cache path, releases the DB lock before touching
/// any filesystem entries and never modifies remote or local Drive contents.
#[tauri::command]
pub async fn cmd_drive_health_inspect(
    app: AppHandle,
    db: State<'_, DbConnection>,
) -> Result<DriveHealthReport, String> {
    let cache_root = app
        .path()
        .app_cache_dir()
        .map_err(|_| "Drive app cache unavailable".to_string())?;
    let pool = db.inner().clone();
    // Never block the UI/event thread on a directory that may be slow or
    // unavailable. The SQLite lock covers only a finite metadata snapshot.
    tauri::async_runtime::spawn_blocking(move || {
        let snapshot = {
            let conn = pool
                .lock()
                .map_err(|_| "Drive database unavailable".to_string())?;
            collect_snapshot(&conn)?
        };
        Ok(inspect_snapshot(snapshot, &cache_root))
    })
    .await
    .map_err(|_| "Drive staging inspection worker failed".to_string())?
}

#[cfg(test)]
fn inspect(conn: &sqlite::Connection, cache_root: &Path) -> Result<DriveHealthReport, String> {
    Ok(inspect_snapshot(collect_snapshot(conn)?, cache_root))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(id: &str) -> (sqlite::Connection, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("terarelay-health-{:x}", rand::random::<u128>()));
        let drive = root.join("drive");
        let staging = drive.join("staging");
        let owned = staging.join(id);
        fs::create_dir_all(&owned).unwrap();
        for path in [&root, &drive, &staging, &owned] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let conn = sqlite::open(":memory:").unwrap();
        crate::commands::drive_metadata::init_drive_schema(&conn).unwrap();
        let stage = owned.join(".pending");
        let mut insert = conn.prepare(
            "INSERT INTO drive_pending_files (id,display_name,staging_path,size,backing_channel_id,closed_at,created_at,updated_at)
             VALUES (?, 'private.bin', ?, 4, 0, 20, 1, 21)").unwrap();
        insert.bind((1, id)).unwrap();
        insert.bind((2, stage.to_str().unwrap())).unwrap();
        insert.next().unwrap();
        drop(insert);
        (conn, root)
    }

    #[test]
    fn read_only_diagnosis_does_not_create_missing_database_schema() {
        let conn = sqlite::open(":memory:").unwrap();
        assert!(collect_snapshot(&conn).is_err());
        let mut query = conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='table'")
            .unwrap();
        assert_eq!(query.next().unwrap(), sqlite::State::Row);
        assert_eq!(query.read::<i64, _>(0).unwrap(), 0);
    }

    #[test]
    fn filesystem_diagnosis_needs_no_live_sqlite_connection_after_snapshot() {
        let (conn, root) = fixture("offline");
        let stage = root.join("drive/staging/offline/.pending");
        fs::write(&stage, b"DATA").unwrap();
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o600)).unwrap();
        conn.execute("INSERT INTO drive_pending_ranges VALUES ('offline',0,4)")
            .unwrap();
        let snapshot = collect_snapshot(&conn).unwrap();
        drop(conn);
        let report = inspect_snapshot(snapshot, &root);
        assert_eq!(report.scanned, 1);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(fs::read(stage).unwrap(), b"DATA");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_directory_permissions_are_reported_without_changing_them() {
        let (conn, root) = fixture("exposed");
        let stage_dir = root.join("drive/staging/exposed");
        fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let report = inspect(&conn, &root).unwrap();
        assert!(report
            .issues
            .iter()
            .any(|item| item.code == "exposed_stage_directory"));
        assert_eq!(
            fs::metadata(&stage_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_private_stage_is_inspected_without_reading_or_mutating_it() {
        let (conn, root) = fixture("healthy");
        let stage = root.join("drive/staging/healthy/.pending");
        fs::write(&stage, b"unchanged-data").unwrap();
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o600)).unwrap();
        conn.execute("INSERT INTO drive_pending_ranges VALUES ('healthy',0,4)")
            .unwrap();
        let report = inspect(&conn, &root).unwrap();
        assert_eq!(
            (report.scanned, report.closed, report.open_writers),
            (1, 1, 0)
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(fs::read(stage).unwrap(), b"unchanged-data");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_data_and_symlink_inside_owned_stage_are_reported_without_following() {
        let (conn, root) = fixture("missing");
        conn.execute("INSERT INTO drive_pending_ranges VALUES ('missing',0,4)")
            .unwrap();
        let first = inspect(&conn, &root).unwrap();
        assert!(first
            .issues
            .iter()
            .any(|item| item.code == "missing_dirty_stage"));
        let sentinel = root.join("sentinel");
        fs::write(&sentinel, b"never touched").unwrap();
        std::os::unix::fs::symlink(&sentinel, root.join("drive/staging/missing/.pending")).unwrap();
        let second = inspect(&conn, &root).unwrap();
        assert!(second
            .issues
            .iter()
            .any(|item| item.code == "unsafe_stage_entry"));
        assert_eq!(fs::read(sentinel).unwrap(), b"never touched");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_stage_marker_is_not_mistaken_for_unuploaded_data() {
        let (conn, root) = fixture("marker");
        conn.execute("INSERT INTO drive_pending_ranges VALUES ('marker',0,4)")
            .unwrap();
        let file = root.join("drive/staging/marker/.pending");
        fs::write(&file, []).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let report = inspect(&conn, &root).unwrap();
        assert!(report
            .issues
            .iter()
            .any(|item| item.code == "missing_dirty_stage"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn foreign_sqlite_stage_path_does_not_escape_cache_root() {
        let (conn, root) = fixture("foreign");
        let sentinel = root.join("keep");
        fs::write(&sentinel, b"still here").unwrap();
        let mut update = conn
            .prepare("UPDATE drive_pending_files SET staging_path=? WHERE id='foreign'")
            .unwrap();
        update.bind((1, sentinel.to_str().unwrap())).unwrap();
        update.next().unwrap();
        drop(update);
        let report = inspect(&conn, &root).unwrap();
        assert!(report
            .issues
            .iter()
            .any(|item| item.code == "missing_or_unsafe_stage"));
        assert_eq!(fs::read(sentinel).unwrap(), b"still here");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_native_writer_is_excluded_from_read_only_inspection() {
        let (conn, root) = fixture("writer");
        conn.execute("UPDATE drive_pending_files SET closed_at=NULL")
            .unwrap();
        let report = inspect(&conn, &root).unwrap();
        assert_eq!(report.open_writers, 1);
        assert!(report.issues.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
