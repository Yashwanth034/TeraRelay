//! Private, opt-in backup Watchlist: durable pairs and bounded, read-only parity scans.
//! Scan reuses the exact raw/V1/V2/copy-on-write safety logic of Folder Parity.
//! Watchlist edits do not change Drive contents, revision or remote Telegram data.
use crate::commands::drive_folder_parity::compare;
use crate::commands::drive_metadata::{drive_snapshot, DriveSnapshot};
use crate::db::DbConnection;
use serde::Serialize;
use sqlite::{Connection, State, Value};
use std::collections::HashMap;
use tauri::State as TauriState;

const MAX_PAIRS: usize = 24;
const MAX_SAMPLES: usize = 6;
const SQL: &str = "
CREATE TABLE IF NOT EXISTS drive_backup_watchlist (
 source_id TEXT NOT NULL,
 backup_id TEXT NOT NULL,
 source_name TEXT NOT NULL,
 backup_name TEXT NOT NULL,
 source_parent_id TEXT,
 backup_parent_id TEXT,
 created_at INTEGER NOT NULL,
 PRIMARY KEY(source_id,backup_id)
);";

#[derive(Clone, Debug, Serialize)]
pub struct BackupWatchPair {
    pub source_id: String,
    pub backup_id: String,
    pub source_name: String,
    pub backup_name: String,
    pub source_parent_id: Option<String>,
    pub backup_parent_id: Option<String>,
    pub created_at: i64,
}
#[derive(Debug, Serialize)]
pub struct BackupWatchList {
    pub revision: i64,
    pub pairs: Vec<BackupWatchPair>,
}
#[derive(Debug, Serialize)]
pub struct BackupWatchCheck {
    pub source_id: String,
    pub backup_id: String,
    pub status: &'static str,
    pub explanation: String,
    pub source_files: usize,
    pub backup_files: usize,
    pub only_source: usize,
    pub only_backup: usize,
    pub different: usize,
    pub unverified: usize,
    pub samples: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BackupWatchScan {
    pub revision: i64,
    pub dirty: bool,
    pub checked_at: i64,
    pub healthy: usize,
    pub drift: usize,
    pub unverified: usize,
    pub blocked: usize,
    pub checks: Vec<BackupWatchCheck>,
    pub metadata_only: bool,
}
fn error(err: sqlite::Error) -> String {
    err.to_string()
}
fn id_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.trim() == id
        && id != "."
        && id != ".."
        && !id
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
}
fn parent_from(row: &sqlite::Statement, col: usize) -> Result<Option<String>, String> {
    match row.read::<Value, _>(col).map_err(error)? {
        Value::Null => Ok(None),
        Value::String(s) if id_safe(&s) => Ok(Some(s)),
        _ => Err("Corrupt persisted backup Watchlist parent ID".into()),
    }
}
fn pairs(conn: &Connection) -> Result<Vec<BackupWatchPair>, String> {
    conn.execute(SQL).map_err(error)?;
    let mut query = conn
        .prepare(
            "SELECT source_id,backup_id,source_name,backup_name,source_parent_id,
         backup_parent_id,created_at FROM drive_backup_watchlist
         ORDER BY created_at,source_id,backup_id LIMIT 25",
        )
        .map_err(error)?;
    let mut out = Vec::new();
    while query.next().map_err(error)? == State::Row {
        if out.len() == MAX_PAIRS {
            return Err("Too many persisted backup Watchlist pairs".into());
        }
        let row = BackupWatchPair {
            source_id: query.read::<String, _>(0).map_err(error)?,
            backup_id: query.read::<String, _>(1).map_err(error)?,
            source_name: query.read::<String, _>(2).map_err(error)?,
            backup_name: query.read::<String, _>(3).map_err(error)?,
            source_parent_id: parent_from(&query, 4)?,
            backup_parent_id: parent_from(&query, 5)?,
            created_at: query.read::<i64, _>(6).map_err(error)?,
        };
        if !id_safe(&row.source_id)
            || !id_safe(&row.backup_id)
            || row.source_id == row.backup_id
            || row.created_at <= 0
            || crate::commands::drive_metadata::validate_drive_name(&row.source_name).is_err()
            || crate::commands::drive_metadata::validate_drive_name(&row.backup_name).is_err()
        {
            return Err("Corrupt persisted Drive backup Watchlist identity or name".into());
        }
        out.push(row);
    }
    Ok(out)
}
fn list(conn: &Connection) -> Result<BackupWatchList, String> {
    let snapshot = drive_snapshot(conn)?;
    Ok(BackupWatchList {
        revision: snapshot.revision,
        pairs: pairs(conn)?,
    })
}
fn add(
    conn: &Connection,
    revision: i64,
    source_id: &str,
    backup_id: &str,
) -> Result<BackupWatchList, String> {
    if !id_safe(source_id) || !id_safe(backup_id) || source_id == backup_id {
        return Err("Select two different existing Drive folders".into());
    }
    let snap = drive_snapshot(conn)?;
    if snap.revision != revision {
        return Err("Drive changed since selection; reload folders".into());
    }
    let rows = pairs(conn)?;
    if rows.len() >= MAX_PAIRS {
        return Err("Watchlist can hold at most 24 backup pairs".into());
    }
    if rows.iter().any(|pair| {
        (pair.source_id == source_id && pair.backup_id == backup_id)
            || (pair.source_id == backup_id && pair.backup_id == source_id)
    }) {
        return Err("This folder pair is already being watched".into());
    }
    // Reuse the strict parity analyzer before saving a pair; no false assurance
    // for malformed ancestry, unreadable active files or unfinished COW edits.
    compare(&snap, conn, source_id, backup_id)?;
    let index: HashMap<&str, _> = snap
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let original = index
        .get(source_id)
        .ok_or("Original folder is no longer available")?;
    let backup = index
        .get(backup_id)
        .ok_or("Backup folder is no longer available")?;
    conn.execute(SQL).map_err(error)?;
    let mut statement = conn
        .prepare(
            "INSERT INTO drive_backup_watchlist
         (source_id,backup_id,source_name,backup_name,source_parent_id,backup_parent_id,created_at)
         VALUES (?,?,?,?,?,?,?)",
        )
        .map_err(error)?;
    statement.bind((1, source_id)).map_err(error)?;
    statement.bind((2, backup_id)).map_err(error)?;
    statement.bind((3, original.name.as_str())).map_err(error)?;
    statement.bind((4, backup.name.as_str())).map_err(error)?;
    statement
        .bind((5, original.parent_id.as_deref()))
        .map_err(error)?;
    statement
        .bind((6, backup.parent_id.as_deref()))
        .map_err(error)?;
    statement
        .bind((7, chrono::Utc::now().timestamp_millis()))
        .map_err(error)?;
    statement.next().map_err(error)?;
    drop(statement);
    let result = list(conn)?;
    if !result
        .pairs
        .iter()
        .any(|p| p.source_id == source_id && p.backup_id == backup_id)
    {
        return Err("Saved Watchlist pair was not persisted".into());
    }
    Ok(result)
}
fn remove(conn: &Connection, source_id: &str, backup_id: &str) -> Result<BackupWatchList, String> {
    if !id_safe(source_id) || !id_safe(backup_id) {
        return Err("Invalid Watchlist pair identity".into());
    }
    let old = pairs(conn)?;
    if !old
        .iter()
        .any(|p| p.source_id == source_id && p.backup_id == backup_id)
    {
        return Err("Backup pair is no longer saved; refresh the list".into());
    }
    let mut statement = conn
        .prepare("DELETE FROM drive_backup_watchlist WHERE source_id=? AND backup_id=?")
        .map_err(error)?;
    statement.bind((1, source_id)).map_err(error)?;
    statement.bind((2, backup_id)).map_err(error)?;
    statement.next().map_err(error)?;
    drop(statement);
    let latest = list(conn)?;
    if latest
        .pairs
        .iter()
        .any(|p| p.source_id == source_id && p.backup_id == backup_id)
    {
        return Err("Backup Watchlist removal did not persist".into());
    }
    Ok(latest)
}
fn inspect_pair(conn: &Connection, s: &DriveSnapshot, p: &BackupWatchPair) -> BackupWatchCheck {
    let mut check = BackupWatchCheck {
        source_id: p.source_id.clone(),
        backup_id: p.backup_id.clone(),
        status: "blocked",
        explanation: String::new(),
        source_files: 0,
        backup_files: 0,
        only_source: 0,
        only_backup: 0,
        different: 0,
        unverified: 0,
        samples: Vec::new(),
    };
    let dirs: HashMap<&str, _> = s
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    match (
        dirs.get(p.source_id.as_str()),
        dirs.get(p.backup_id.as_str()),
    ) {
        (Some(a), Some(b))
            if a.name == p.source_name
                && b.name == p.backup_name
                && a.parent_id == p.source_parent_id
                && b.parent_id == p.backup_parent_id => {}
        _ => {
            check.explanation="A saved folder moved, changed name, was trashed or is unavailable; reselect this pair".into();
            return check;
        }
    }
    match compare(s, conn, &p.source_id, &p.backup_id) {
        Err(reason) => {
            check.explanation = reason;
        }
        Ok(report) => {
            check.source_files = report.left_files;
            check.backup_files = report.right_files;
            check.only_source = report.only_left;
            check.only_backup = report.only_right;
            check.different = report.different;
            check.unverified = report.unverified_file_content;
            check.samples = report
                .differences
                .into_iter()
                .take(MAX_SAMPLES)
                .map(|d| format!("{}: {}", d.kind, d.relative_path))
                .collect();
            if check.only_source + check.only_backup + check.different > 0 {
                check.status = "drift";
                check.explanation =
                    "Backup differs from source; inspect the reported paths before action".into();
            } else if check.unverified > 0 || s.dirty {
                check.status = "unverified";
                check.explanation="No detected metadata drift, but content identity or local cloud sync is unverified".into();
            } else {
                check.status = "healthy";
                check.explanation="Metadata matches; file bytes and remote cloud availability were not independently checked".into();
            }
        }
    }
    check
}
pub(crate) fn scan(conn: &Connection) -> Result<BackupWatchScan, String> {
    let saved = pairs(conn)?;
    // Capture one coherent local Drive snapshot for every pair.
    let snapshot = drive_snapshot(conn)?;
    let mut output = BackupWatchScan {
        revision: snapshot.revision,
        dirty: snapshot.dirty,
        checked_at: chrono::Utc::now().timestamp_millis(),
        healthy: 0,
        drift: 0,
        unverified: 0,
        blocked: 0,
        checks: Vec::new(),
        metadata_only: true,
    };
    for pair in &saved {
        let check = inspect_pair(conn, &snapshot, pair);
        match check.status {
            "healthy" => output.healthy += 1,
            "drift" => output.drift += 1,
            "unverified" => output.unverified += 1,
            _ => output.blocked += 1,
        }
        output.checks.push(check);
    }
    Ok(output)
}
#[tauri::command]
pub fn cmd_drive_backup_watchlist_list(
    db: TauriState<'_, DbConnection>,
) -> Result<BackupWatchList, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    list(&conn)
}
#[tauri::command]
pub fn cmd_drive_backup_watchlist_add(
    db: TauriState<'_, DbConnection>,
    revision: i64,
    source_id: String,
    backup_id: String,
) -> Result<BackupWatchList, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    add(&conn, revision, &source_id, &backup_id)
}
#[tauri::command]
pub fn cmd_drive_backup_watchlist_remove(
    db: TauriState<'_, DbConnection>,
    source_id: String,
    backup_id: String,
) -> Result<BackupWatchList, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    remove(&conn, &source_id, &backup_id)
}
#[tauri::command]
pub fn cmd_drive_backup_watchlist_scan(
    db: TauriState<'_, DbConnection>,
) -> Result<BackupWatchScan, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    scan(&conn)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;
    fn db() -> Connection {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_channels(
          logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER UNIQUE,
          name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
         CREATE TABLE logical_files(
          file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,
          first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,
          mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
          storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
          status TEXT NOT NULL DEFAULT 'complete');",
        )
        .unwrap();
        init_drive_schema(&conn).unwrap();
        conn.execute("INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
          VALUES('src',NULL,'Original',1,1),('bak',NULL,'Backup',1,1),
          ('docs1','src','Docs',1,1),('docs2','bak','Docs',1,1);
         INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
          VALUES('qa',5,'QA','owner',1,1,1);
         INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,
            chunk_count,whole_sha256,storage_version,created_at,status)
          VALUES('a','qa',1,'item.bin',12,1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,1,'complete'),
                ('b','qa',2,'item.bin',12,1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,1,'complete');
         INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
          VALUES('entry1','a','docs1','item.bin',1,1),('entry2','b','docs2','item.bin',1,1);").unwrap();
        conn
    }
    fn rev(conn: &Connection) -> i64 {
        drive_snapshot(conn).unwrap().revision
    }
    #[test]
    fn persistent_pairs_and_verified_metadata_scan_with_no_drive_mutation() {
        let c = db();
        let before = rev(&c);
        let list = add(&c, before, "src", "bak").unwrap();
        assert_eq!(list.pairs.len(), 1);
        assert_eq!(rev(&c), before);
        let result = scan(&c).unwrap();
        assert_eq!(
            (
                result.healthy,
                result.drift,
                result.unverified,
                result.blocked
            ),
            (1, 0, 0, 0)
        );
        assert_eq!(result.checks[0].source_files, 1);
        assert_eq!(rev(&c), before);
        assert_eq!(remove(&c, "src", "bak").unwrap().pairs.len(), 0);
        assert_eq!(rev(&c), before);
    }
    #[test]
    fn missing_backup_and_metadata_mismatch_are_drift_not_healthy() {
        let c = db();
        add(&c, rev(&c), "src", "bak").unwrap();
        c.execute("UPDATE drive_file_entries SET trashed_at=42 WHERE entry_id='entry2'")
            .unwrap();
        let out = scan(&c).unwrap();
        assert_eq!(out.drift, 1);
        assert_eq!(out.checks[0].only_source, 1);
    }
    #[test]
    fn moved_root_or_missing_backing_blocks_false_assurance() {
        let c = db();
        add(&c, rev(&c), "src", "bak").unwrap();
        c.execute("UPDATE drive_directories SET name='Moved' WHERE id='bak'")
            .unwrap();
        assert_eq!(scan(&c).unwrap().blocked, 1);
        c.execute(
            "UPDATE drive_directories SET name='Backup' WHERE id='bak';
          UPDATE logical_files SET status='missing' WHERE file_id='b'",
        )
        .unwrap();
        assert_eq!(scan(&c).unwrap().blocked, 1);
    }
    #[test]
    fn stale_revision_and_duplicate_pairs_fail_without_saving() {
        let c = db();
        let baseline = rev(&c);
        assert!(add(&c, baseline - 1, "src", "bak").is_err());
        add(&c, baseline, "src", "bak").unwrap();
        assert!(add(&c, baseline, "src", "bak").is_err());
        assert!(add(&c, baseline, "bak", "src").is_err());
        assert!(add(&c, baseline, "src", "docs1").is_err());
        assert_eq!(list(&c).unwrap().pairs.len(), 1);
    }
    #[test]
    fn incomplete_fingerprint_never_reports_a_verified_pair() {
        let c = db();
        add(&c, rev(&c), "src", "bak").unwrap();
        c.execute("UPDATE logical_files SET whole_sha256=NULL WHERE file_id IN ('a','b')")
            .unwrap();
        let report = scan(&c).unwrap();
        assert_eq!(report.healthy, 0);
        assert_eq!(report.unverified, 1);
        assert_eq!(report.checks[0].unverified, 1);
    }
    #[test]
    fn pending_copy_on_write_and_unsynced_metadata_never_report_healthy() {
        let c = db();
        add(&c, rev(&c), "src", "bak").unwrap();
        c.execute("UPDATE drive_state SET dirty=1 WHERE singleton=1")
            .unwrap();
        assert_eq!(scan(&c).unwrap().unverified, 1);
        c.execute(
            "INSERT INTO drive_pending_files(id,directory_id,display_name,staging_path,
                size,format_version,backing_channel_id,base_entry_id,created_at,updated_at)
             VALUES('staged',NULL,'uncommitted.bin','not-read',1,1,5,'entry1',1,1)",
        )
        .unwrap();
        assert_eq!(scan(&c).unwrap().blocked, 1);
    }
    #[test]
    fn watchlist_hard_limit_rejects_the_twenty_fifth_pair() {
        let c = db();
        c.execute(SQL).unwrap();
        for n in 0..24 {
            let source = format!("source-{n}");
            let backup = format!("backup-{n}");
            let mut q = c
                .prepare(
                    "INSERT INTO drive_backup_watchlist
                 (source_id,backup_id,source_name,backup_name,created_at)
                 VALUES(?,?, 'Original','Backup',1)",
                )
                .unwrap();
            q.bind((1, source.as_str())).unwrap();
            q.bind((2, backup.as_str())).unwrap();
            q.next().unwrap();
        }
        assert_eq!(list(&c).unwrap().pairs.len(), 24);
        assert!(add(&c, rev(&c), "src", "bak")
            .unwrap_err()
            .contains("at most 24"));
    }
    #[test]
    fn scan_fails_closed_for_corrupt_persisted_ids() {
        let c = db();
        add(&c, rev(&c), "src", "bak").unwrap();
        c.execute("UPDATE drive_backup_watchlist SET source_id='../../outside'")
            .unwrap();
        assert!(scan(&c).is_err());
    }
}
