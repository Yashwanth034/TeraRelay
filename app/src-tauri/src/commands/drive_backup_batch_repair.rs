//! Atomic, additive repair for ALL explicitly saved Drive backup Watchlist pairs.
//! One bounded preflight/confirmation; every pair commits, or no pair changes.
//! Only references completed Drive content; never uploads bytes or overwrites.
use crate::commands::drive_folder_mirror::{self, MirrorRequest};
use crate::commands::drive_metadata::{drive_snapshot, validate_drive_name, DriveDirectoryRecord};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, State as SqlState, Value};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use tauri::Manager;
use tauri::{AppHandle, State};

const MAX_PAIRS: usize = 24;
const MAX_TOTAL_FOLDERS: usize = 512;
const MAX_TOTAL_FILES: usize = 2000;
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRepairRequest {
    pub revision: i64,
    pub plan_token: String,
}
#[derive(Debug, Serialize)]
pub struct BatchRepairPair {
    pub source_id: String,
    pub backup_id: String,
    pub source_name: String,
    pub backup_name: String,
    pub add_folders: usize,
    pub add_files: usize,
    pub matching_files: usize,
    pub added_logical_bytes: u64,
    pub examples: Vec<String>,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BatchRepairPreview {
    pub revision: i64,
    pub plan_token: String,
    pub ready: bool,
    pub total_folders: usize,
    pub total_files: usize,
    pub total_matches: usize,
    pub added_logical_bytes: u64,
    pub pairs: Vec<BatchRepairPair>,
    pub problems: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct BatchRepairApplied {
    pub revision: i64,
    pub dirty: bool,
    pub pairs_updated: usize,
    pub folders_added: usize,
    pub files_added: usize,
    pub files_matched: usize,
}
#[derive(Clone, Serialize)]
struct Saved {
    source_id: String,
    backup_id: String,
    source_name: String,
    backup_name: String,
    source_parent_id: Option<String>,
    backup_parent_id: Option<String>,
    created_at: i64,
}
type Changes = Vec<(Option<String>, String)>;
type Inodes = Vec<(String, String)>;
fn sql_err(e: sqlite::Error) -> String {
    e.to_string()
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.trim() == s
        && s != "."
        && s != ".."
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn nullable(q: &sqlite::Statement, index: usize) -> Result<Option<String>, String> {
    match q.read::<Value, _>(index).map_err(sql_err)? {
        Value::Null => Ok(None),
        Value::String(s) if valid_id(&s) => Ok(Some(s)),
        _ => Err("Corrupted saved backup parent identity".into()),
    }
}
fn saved_pairs(conn: &Connection) -> Result<Vec<Saved>, String> {
    // The normal Watchlist UI creates this local table. A missing Watchlist
    // is not an implicit empty successful repair.
    let mut q = conn
        .prepare(
            "SELECT source_id,backup_id,source_name,backup_name,
          source_parent_id,backup_parent_id,created_at
   FROM drive_backup_watchlist ORDER BY created_at,source_id,backup_id LIMIT 25",
        )
        .map_err(|_| {
            "Open Backup Watchlist and save source/backup pairs before repairing".to_string()
        })?;
    let mut rows = Vec::new();
    while q.next().map_err(sql_err)? == SqlState::Row {
        if rows.len() >= MAX_PAIRS {
            return Err("Backup Watchlist exceeds 24 saved pairs".into());
        }
        let p = Saved {
            source_id: q.read::<String, _>(0).map_err(sql_err)?,
            backup_id: q.read::<String, _>(1).map_err(sql_err)?,
            source_name: q.read::<String, _>(2).map_err(sql_err)?,
            backup_name: q.read::<String, _>(3).map_err(sql_err)?,
            source_parent_id: nullable(&q, 4)?,
            backup_parent_id: nullable(&q, 5)?,
            created_at: q.read::<i64, _>(6).map_err(sql_err)?,
        };
        if !valid_id(&p.source_id)
            || !valid_id(&p.backup_id)
            || p.source_id == p.backup_id
            || p.created_at <= 0
            || validate_drive_name(&p.source_name).is_err()
            || validate_drive_name(&p.backup_name).is_err()
        {
            return Err("Saved Watchlist contains an unsafe or malformed pair".into());
        }
        rows.push(p);
    }
    if rows.is_empty() {
        return Err("No saved Backup Watchlist pairs to repair".into());
    }
    Ok(rows)
}
fn related(a: &str, b: &str, index: &HashMap<&str, &DriveDirectoryRecord>) -> Result<bool, String> {
    let mut cursor = Some(a);
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) || seen.len() > MAX_TOTAL_FOLDERS {
            return Err("Backup pair folder ancestor chain is cyclic or too deep".into());
        }
        if id == b {
            return Ok(true);
        }
        cursor = index
            .get(id)
            .ok_or("Backup pair has a missing ancestor")?
            .parent_id
            .as_deref();
    }
    Ok(false)
}
fn check_roots(
    snap: &crate::commands::drive_metadata::DriveSnapshot,
    pairs: &[Saved],
) -> Result<(), String> {
    let index: HashMap<&str, &DriveDirectoryRecord> = snap
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut sources = Vec::new();
    let mut targets = Vec::new();
    for p in pairs {
        let src = index
            .get(p.source_id.as_str())
            .ok_or("Saved source folder was moved to Trash or disappeared")?;
        let dst = index
            .get(p.backup_id.as_str())
            .ok_or("Saved backup folder was moved to Trash or disappeared")?;
        if src.name != p.source_name
            || src.parent_id != p.source_parent_id
            || dst.name != p.backup_name
            || dst.parent_id != p.backup_parent_id
        {
            return Err("A watched root moved or was renamed; reselect the pair".into());
        }
        sources.push(p.source_id.as_str());
        targets.push(p.backup_id.as_str());
    }
    // The same immutable source may safely feed multiple independent backups,
    // even when source trees nest. A target MUST NOT overlap ANY source and
    // targets may never overlap one another; a prior write must not affect
    // a later source or target.
    for (i, target) in targets.iter().enumerate() {
        for source in &sources {
            if related(target, source, &index)? || related(source, target, &index)? {
                return Err("A backup destination overlaps a watched source tree".into());
            }
        }
        for other in targets.iter().take(i) {
            if related(target, other, &index)? || related(other, target, &index)? {
                return Err("Two watched backup destinations overlap".into());
            }
        }
    }
    Ok(())
}
fn token<T: Serialize>(x: &T) -> Result<String, String> {
    let encoded = serde_json::to_vec(x).map_err(|_| "Could not fingerprint full backup plan")?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}
fn preview(conn: &Connection) -> Result<BatchRepairPreview, String> {
    let snap = drive_snapshot(conn)?;
    let mut result = BatchRepairPreview {
        revision: snap.revision,
        plan_token: String::new(),
        ready: true,
        total_folders: 0,
        total_files: 0,
        total_matches: 0,
        added_logical_bytes: 0,
        pairs: Vec::new(),
        problems: Vec::new(),
    };
    let saved = match saved_pairs(conn) {
        Ok(s) => s,
        Err(e) => {
            result.ready = false;
            result.problems.push(e);
            return Ok(result);
        }
    };
    if let Err(e) = check_roots(&snap, &saved) {
        result.ready = false;
        result.problems.push(e);
    }
    for p in &saved {
        let request = MirrorRequest {
            revision: snap.revision,
            source_id: p.source_id.clone(),
            target_id: p.backup_id.clone(),
        };
        let mut r = drive_folder_mirror::preview(conn, &request)?;
        // Source folder enumeration uses HashMap iteration order. The same
        // unchanged Drive tree must yield the exact same preview token.
        r.examples.sort();
        let pair = BatchRepairPair {
            source_id: p.source_id.clone(),
            backup_id: p.backup_id.clone(),
            source_name: p.source_name.clone(),
            backup_name: p.backup_name.clone(),
            add_folders: r.add_folders,
            add_files: r.add_files,
            matching_files: r.matching_files,
            added_logical_bytes: r.added_logical_bytes,
            examples: r.examples,
            problems: r.problems,
        };
        if !r.ready {
            result.ready = false;
        }
        result.total_folders = result
            .total_folders
            .checked_add(pair.add_folders)
            .ok_or("Too many folders in repair")?;
        result.total_files = result
            .total_files
            .checked_add(pair.add_files)
            .ok_or("Too many files in repair")?;
        result.total_matches = result
            .total_matches
            .checked_add(pair.matching_files)
            .ok_or("Too many source matches")?;
        result.added_logical_bytes = result
            .added_logical_bytes
            .checked_add(pair.added_logical_bytes)
            .ok_or("Byte total overflow")?;
        result.pairs.push(pair);
    }
    if result.total_folders > MAX_TOTAL_FOLDERS || result.total_files > MAX_TOTAL_FILES {
        result.ready = false;
        result
            .problems
            .push("Repair exceeds 512 added folders or 2,000 added files across all pairs".into());
    }
    if result.total_folders == 0 && result.total_files == 0 && result.ready {
        result.ready = false;
        result
            .problems
            .push("All saved backups already match; nothing needs repairing".into());
    }
    // Digest covers the full snapshot (including metadata changes that may
    // not bump revision), saved Watchlist records, and exact repair preview.
    // Native per-pair apply still repeats raw SQL/backing validation at commit.
    result.plan_token = token(&(
        &snap,
        &saved,
        &result.revision,
        &result.ready,
        &result.total_folders,
        &result.total_files,
        &result.total_matches,
        &result.added_logical_bytes,
        &result.pairs,
        &result.problems,
    ))?;
    Ok(result)
}
fn apply(
    conn: &Connection,
    request: &BatchRepairRequest,
) -> Result<(BatchRepairApplied, Changes, Inodes), String> {
    if request.plan_token.len() != 64
        || !request
            .plan_token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("Invalid saved backup repair confirmation".into());
    }
    let before = drive_snapshot(conn)?;
    if request.revision != before.revision {
        return Err("Drive changed; refresh and preview all watched backups again".into());
    }
    let plan = preview(conn)?;
    if !plan.ready || plan.revision != request.revision || plan.plan_token != request.plan_token {
        return Err("Backup Watchlist or repair plan changed; preview and confirm again".into());
    }
    conn.execute("SAVEPOINT drive_watchlist_batch_repair")
        .map_err(sql_err)?;
    let result = (|| {
        let (mut folders, mut files, mut matches, mut updated) = (0usize, 0usize, 0usize, 0usize);
        let (mut locations, mut inodes) = (Vec::new(), Vec::new());
        for pair in &plan.pairs {
            matches = matches
                .checked_add(pair.matching_files)
                .ok_or("Invalid match total")?;
            if pair.add_folders == 0 && pair.add_files == 0 {
                continue;
            }
            // Each native mirror revalidates backing, ancestry, collisions and a
            // new revision after the previous pair. Its nested savepoint is rolled
            // back together with ALL prior pairs by our enclosing savepoint.
            let latest = drive_snapshot(conn)?.revision;
            let request = MirrorRequest {
                revision: latest,
                source_id: pair.source_id.clone(),
                target_id: pair.backup_id.clone(),
            };
            let (done, changed, ids) = drive_folder_mirror::apply(conn, &request)?;
            if done.folders_added != pair.add_folders
                || done.files_added != pair.add_files
                || done.files_matched != pair.matching_files
            {
                return Err("Backup repair changed during commit; roll back every pair".into());
            }
            folders += done.folders_added;
            files += done.files_added;
            updated += 1;
            locations.extend(changed);
            inodes.extend(ids);
        }
        if folders != plan.total_folders
            || files != plan.total_files
            || matches != plan.total_matches
        {
            return Err("Aggregate backup repair did not match confirmed preview".into());
        }
        let after = drive_snapshot(conn)?;
        if !after.dirty || after.revision <= before.revision {
            return Err("Batch repair failed to advance dirty Drive revision".into());
        }
        Ok((
            BatchRepairApplied {
                revision: after.revision,
                dirty: after.dirty,
                pairs_updated: updated,
                folders_added: folders,
                files_added: files,
                files_matched: matches,
            },
            locations,
            inodes,
        ))
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_watchlist_batch_repair", result)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let c = sqlite::open(":memory:").unwrap();
        c.execute(
            "CREATE TABLE logical_channels(
    logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER UNIQUE,
    name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
    CREATE TABLE logical_files(
    file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,first_message_id INTEGER NOT NULL,
    display_name TEXT NOT NULL,total_size INTEGER NOT NULL,mime_type TEXT,
    chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
    storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'complete');",
        )
        .unwrap();
        crate::commands::drive_metadata::init_drive_schema(&c).unwrap();
        c.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
    VALUES('src1',NULL,'Source1',1,1),('src2',NULL,'Source2',1,1),
          ('dst1',NULL,'Backup1',1,1),('dst2',NULL,'Backup2',1,1),
          ('nested','src1','Docs',1,1);
    INSERT INTO logical_channels
    (logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
    VALUES('qa',112,'QA','owner',1,1,1);
    INSERT INTO logical_files
    (file_id,logical_channel_id,first_message_id,display_name,total_size,
     chunk_count,whole_sha256,storage_version,created_at,status)
    VALUES('payload1','qa',1,'one.txt',11,1,'one',1,1,'complete'),
          ('payload2','qa',2,'two.txt',12,1,'two',1,1,'complete');
    INSERT INTO drive_file_entries
    (entry_id,file_id,directory_id,display_name,created_at,updated_at)
    VALUES('e1','payload1','nested','one.txt',1,1),
          ('e2','payload2','src2','two.txt',1,1);
    CREATE TABLE drive_backup_watchlist(
     source_id TEXT NOT NULL,backup_id TEXT NOT NULL,
     source_name TEXT NOT NULL,backup_name TEXT NOT NULL,
     source_parent_id TEXT,backup_parent_id TEXT,
     created_at INTEGER NOT NULL,PRIMARY KEY(source_id,backup_id));
    INSERT INTO drive_backup_watchlist
    (source_id,backup_id,source_name,backup_name,source_parent_id,backup_parent_id,created_at)
    VALUES('src1','dst1','Source1','Backup1',NULL,NULL,1),
          ('src2','dst2','Source2','Backup2',NULL,NULL,2);",
        )
        .unwrap();
        c
    }
    fn request(p: &BatchRepairPreview) -> BatchRepairRequest {
        BatchRepairRequest {
            revision: p.revision,
            plan_token: p.plan_token.clone(),
        }
    }
    #[test]
    fn preview_and_atomic_repair_all_pairs_with_nested_dirs_and_noop_after() {
        let c = fixture();
        let original = drive_snapshot(&c).unwrap().revision;
        let p = preview(&c).unwrap();
        assert!(p.ready);
        assert_eq!((p.pairs.len(), p.total_folders, p.total_files), (2, 1, 2));
        assert_eq!(p.added_logical_bytes, 23);
        let (done, changes, ids) = apply(&c, &request(&p)).unwrap();
        assert_eq!(
            (done.pairs_updated, done.folders_added, done.files_added),
            (2, 1, 2)
        );
        assert_eq!(changes.len(), 3);
        assert_eq!(ids.len(), 3);
        assert!(done.revision > original);
        let after = drive_snapshot(&c).unwrap();
        assert_eq!(after.files.len(), 4);
        assert_eq!(
            after
                .directories
                .iter()
                .filter(|d| d.trashed_at.is_none())
                .count(),
            6
        );
        let second = preview(&c).unwrap();
        assert!(!second.ready);
        assert_eq!(second.total_files, 0);
        assert_eq!(second.total_matches, 2);
        assert!(apply(&c, &request(&second)).is_err());
    }
    #[test]
    fn failure_in_second_pair_rolls_back_all_first_pair_changes() {
        let c = fixture();
        let r = request(&preview(&c).unwrap());
        let before = drive_snapshot(&c).unwrap().revision;
        c.execute(
            "CREATE TRIGGER qa_fail_second BEFORE INSERT ON drive_file_entries
    WHEN NEW.directory_id='dst2'
    BEGIN SELECT RAISE(ABORT,'synthetic second pair failure'); END",
        )
        .unwrap();
        assert!(apply(&c, &r).is_err());
        let after = drive_snapshot(&c).unwrap();
        assert_eq!(after.revision, before);
        assert_eq!(after.files.len(), 2);
        assert_eq!(
            after
                .directories
                .iter()
                .filter(|d| d.trashed_at.is_none())
                .count(),
            5
        );
    }
    #[test]
    fn altered_watchlist_while_revision_unchanged_blocks_stale_confirmation() {
        let c = fixture();
        let before = preview(&c).unwrap();
        c.execute("UPDATE drive_backup_watchlist SET created_at=99 WHERE source_id='src1'")
            .unwrap();
        assert_eq!(drive_snapshot(&c).unwrap().revision, before.revision);
        assert!(apply(&c, &request(&before)).is_err());
        assert_eq!(drive_snapshot(&c).unwrap().files.len(), 2);
    }
    #[test]
    fn overlap_missing_backing_and_changed_names_block_every_pair() {
        let c = fixture();
        c.execute(
            "UPDATE drive_backup_watchlist SET backup_id='nested',
     backup_name='Docs',backup_parent_id='src1' WHERE source_id='src1'",
        )
        .unwrap();
        assert!(!preview(&c).unwrap().ready);
        let c = fixture();
        c.execute("UPDATE logical_files SET status='missing' WHERE file_id='payload2'")
            .unwrap();
        let p = preview(&c).unwrap();
        assert!(!p.ready);
        assert!(p.pairs.iter().any(|t| !t.problems.is_empty()));
        let c = fixture();
        c.execute("UPDATE drive_directories SET name='Moved' WHERE id='dst2'")
            .unwrap();
        assert!(!preview(&c).unwrap().ready);
    }
    #[test]
    fn unchanged_folder_snapshots_yield_stable_confirmation_tokens() {
        let c = fixture();
        let expected = preview(&c).unwrap();
        assert!(expected.ready);
        for _ in 0..15 {
            let next = preview(&c).unwrap();
            assert_eq!(next.plan_token, expected.plan_token);
            assert_eq!(next.total_folders, expected.total_folders);
            assert_eq!(next.total_files, expected.total_files);
        }
    }
    #[test]
    fn one_source_can_feed_two_disjoint_destinations_atomically() {
        let c = fixture();
        c.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
             VALUES('dst3',NULL,'Backup3',1,1);
             INSERT INTO drive_backup_watchlist
             (source_id,backup_id,source_name,backup_name,created_at)
             VALUES('src1','dst3','Source1','Backup3',3);",
        )
        .unwrap();
        let p = preview(&c).unwrap();
        assert!(p.ready);
        assert_eq!((p.pairs.len(), p.total_folders, p.total_files), (3, 2, 3));
        let (done, _, _) = apply(&c, &request(&p)).unwrap();
        assert_eq!(
            (done.pairs_updated, done.folders_added, done.files_added),
            (3, 2, 3)
        );
        assert!(!preview(&c).unwrap().ready);
    }
    #[test]
    fn stale_revision_and_no_saved_pairs_never_commit() {
        let c = fixture();
        let p = preview(&c).unwrap();
        assert!(apply(
            &c,
            &BatchRepairRequest {
                revision: p.revision + 1,
                plan_token: p.plan_token.clone()
            }
        )
        .is_err());
        c.execute("DELETE FROM drive_backup_watchlist").unwrap();
        assert!(!preview(&c).unwrap().ready);
        assert_eq!(drive_snapshot(&c).unwrap().files.len(), 2);
    }
}

#[tauri::command]
pub fn cmd_drive_backup_batch_repair_preview(
    db: State<'_, DbConnection>,
) -> Result<BatchRepairPreview, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    preview(&conn)
}
#[tauri::command]
pub fn cmd_drive_backup_batch_repair_apply(
    app: AppHandle,
    db: State<'_, DbConnection>,
    request: BatchRepairRequest,
) -> Result<BatchRepairApplied, String> {
    let (result, changes, ids) = {
        let conn = db.lock().map_err(|_| "Drive database unavailable")?;
        apply(&conn, &request)?
    };
    #[cfg(target_os = "linux")]
    if let Some(mount) = app.try_state::<crate::drive_mount::DriveMountState>() {
        mount.notify_namespace_changed(&changes, &ids);
    }
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::drive_windows::refresh() {
        log::warn!("Backup Batch Repair committed; Windows refresh failed: {e}");
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::drive_provider_ipc::signal_changes(&app) {
        log::warn!("Backup Batch Repair committed; macOS refresh failed: {e}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&changes, &ids);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = app;
    Ok(result)
}
