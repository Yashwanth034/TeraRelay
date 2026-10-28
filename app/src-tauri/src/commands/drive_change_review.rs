//! Explicit, device-local Drive checkpoint and change comparison.
//! No cloud requests, metadata rollback, or remote file mutation.
use crate::commands::drive_metadata::{drive_snapshot, DriveSnapshot};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tauri::State;

const MAX_ITEMS: usize = 25_000;
const MAX_CHANGES: usize = 5_000;
const MAX_BASELINE_JSON: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ChangeEntry {
    kind: String,
    id: String,
    path: String,
    storage: String,
    fingerprint: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveChange {
    pub kind: String,
    pub status: String,
    pub before_path: Option<String>,
    pub after_path: Option<String>,
    pub content_evidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveChangeReview {
    pub generation: i64,
    pub captured_at: Option<i64>,
    pub baseline_revision: Option<i64>,
    pub current_revision: i64,
    pub current_dirty: bool,
    pub current_pending: usize,
    pub tracked_files: usize,
    pub tracked_folders: usize,
    pub baseline_count: usize,
    pub change_count: usize,
    pub truncated: bool,
    pub changes: Vec<DriveChange>,
}

fn init_local_table(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_local_change_baseline (
             id INTEGER PRIMARY KEY CHECK(id=1),
             generation INTEGER NOT NULL CHECK(generation > 0),
             saved_at INTEGER NOT NULL,
             drive_revision INTEGER NOT NULL,
             entries_json TEXT NOT NULL
         )",
    )
    .map_err(|e| e.to_string())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}

fn folder_path(
    id: &str,
    parents: &HashMap<String, (String, Option<String>)>,
    visiting: &mut HashSet<String>,
    depth: usize,
) -> Result<String, String> {
    if depth > 100 || !visiting.insert(id.to_owned()) {
        return Err(
            "Drive folder ancestry cycle or depth limit; no checkpoint was changed.".into(),
        );
    }
    let (name, parent) = parents
        .get(id)
        .ok_or("Unresolved Drive folder ancestry; no checkpoint was changed.")?;
    let parent_path = if let Some(parent) = parent {
        folder_path(parent, parents, visiting, depth + 1)?
    } else {
        String::new()
    };
    visiting.remove(id);
    Ok(format!("{parent_path}/{name}"))
}

fn snapshot_rows(snapshot: &DriveSnapshot) -> Result<Vec<ChangeEntry>, String> {
    if snapshot
        .files
        .len()
        .saturating_add(snapshot.directories.len())
        > MAX_ITEMS
    {
        return Err(
            "Drive exceeds 25,000 file/folder checkpoint items. No baseline was changed.".into(),
        );
    }
    let mut folders = HashMap::new();
    let mut identities = HashSet::new();
    for folder in snapshot
        .directories
        .iter()
        .filter(|f| f.trashed_at.is_none())
    {
        if !valid_id(&folder.id)
            || !valid_name(&folder.name)
            || !folder.parent_id.as_deref().is_none_or(valid_id)
            || !identities.insert(format!("folder:{}", folder.id))
            || folders
                .insert(
                    folder.id.clone(),
                    (folder.name.clone(), folder.parent_id.clone()),
                )
                .is_some()
        {
            return Err("Invalid or ambiguous Drive folder metadata; baseline unchanged.".into());
        }
    }
    let mut rows = Vec::new();
    for folder in snapshot
        .directories
        .iter()
        .filter(|f| f.trashed_at.is_none())
    {
        rows.push(ChangeEntry {
            kind: "folder".into(),
            id: folder.id.clone(),
            path: folder_path(&folder.id, &folders, &mut HashSet::new(), 0)?,
            storage: String::new(),
            fingerprint: None,
            size: None,
        });
    }
    for file in &snapshot.files {
        if !valid_id(&file.entry_id)
            || !valid_id(&file.file_id)
            || !valid_name(&file.display_name)
            || !file.directory_id.as_deref().is_none_or(valid_id)
            || !identities.insert(format!("file:{}", file.entry_id))
        {
            return Err("Invalid or duplicate Drive file identity; baseline unchanged.".into());
        }
        let parent = match file.directory_id.as_deref() {
            Some(id) => folder_path(id, &folders, &mut HashSet::new(), 0)?,
            None => String::new(),
        };
        let size = file.plaintext_size.unwrap_or(file.total_size);
        if file
            .content_fingerprint
            .as_ref()
            .is_some_and(|fingerprint| {
                fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err("Invalid Drive content fingerprint; baseline unchanged.".into());
        }
        rows.push(ChangeEntry {
            kind: "file".into(),
            id: file.entry_id.clone(),
            path: format!("{parent}/{}", file.display_name),
            storage: format!("{}:{}:{}", file.storage_version, file.file_id, size),
            fingerprint: file
                .content_fingerprint
                .as_ref()
                .map(|hash| hash.to_ascii_lowercase()),
            size: Some(size),
        });
    }
    rows.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.id.cmp(&b.id)));
    let mut full_paths = HashSet::new();
    for entry in &rows {
        // Cross-kind namespace collisions create misleading comparisons.
        if !full_paths.insert(entry.path.to_lowercase()) {
            return Err("Ambiguous Drive paths; no checkpoint was changed.".into());
        }
    }
    Ok(rows)
}

#[derive(Debug)]
struct Baseline {
    generation: i64,
    at: i64,
    revision: i64,
    rows: Vec<ChangeEntry>,
}
fn valid_stored_entry(row: &ChangeEntry) -> bool {
    if !valid_id(&row.id)
        || !row.path.starts_with('/')
        || row.path.len() > 26_000
        || !row.path.split('/').skip(1).all(valid_name)
    {
        return false;
    }
    if row.kind == "folder" {
        return row.storage.is_empty() && row.fingerprint.is_none() && row.size.is_none();
    }
    if row.kind != "file" || row.storage.len() > 600 {
        return false;
    }
    let Some(size) = row.size else {
        return false;
    };
    let Some((version, remainder)) = row.storage.split_once(':') else {
        return false;
    };
    let Some((file_id, serialized_size)) = remainder.rsplit_once(':') else {
        return false;
    };
    if !matches!(version, "1" | "2") || !valid_id(file_id) || serialized_size != size.to_string() {
        return false;
    }
    row.fingerprint.as_deref().is_none_or(|fingerprint| {
        fingerprint.len() == 64
            && fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn load(conn: &sqlite::Connection) -> Result<Option<Baseline>, String> {
    init_local_table(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT generation, saved_at, drive_revision, entries_json
         FROM drive_local_change_baseline WHERE id=1",
        )
        .map_err(|e| e.to_string())?;
    if stmt.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    let generation = stmt.read::<i64, _>(0).map_err(|e| e.to_string())?;
    let at = stmt.read::<i64, _>(1).map_err(|e| e.to_string())?;
    let revision = stmt.read::<i64, _>(2).map_err(|e| e.to_string())?;
    let json = stmt.read::<String, _>(3).map_err(|e| e.to_string())?;
    if generation < 1 || at <= 0 || revision < 0 || json.len() > MAX_BASELINE_JSON {
        return Err("Invalid stored Drive checkpoint; no changes will be made.".into());
    }
    let rows: Vec<ChangeEntry> =
        serde_json::from_str(&json).map_err(|_| "Damaged stored Drive checkpoint.")?;
    if rows.len() > MAX_ITEMS {
        return Err("Stored Drive checkpoint exceeds item cap.".into());
    }
    let mut seen = HashSet::new();
    let mut unique_paths = HashSet::new();
    for row in &rows {
        if !valid_stored_entry(row)
            || !seen.insert((row.kind.as_str(), row.id.as_str()))
            || !unique_paths.insert(row.path.to_lowercase())
        {
            return Err("Stored Drive checkpoint contains invalid or ambiguous metadata.".into());
        }
    }
    Ok(Some(Baseline {
        generation,
        at,
        revision,
        rows,
    }))
}

fn diff(old: &[ChangeEntry], now: &[ChangeEntry]) -> (Vec<DriveChange>, usize, bool) {
    let previous: HashMap<_, _> = old
        .iter()
        .map(|x| ((x.kind.as_str(), x.id.as_str()), x))
        .collect();
    let current: HashMap<_, _> = now
        .iter()
        .map(|x| ((x.kind.as_str(), x.id.as_str()), x))
        .collect();
    let mut all = Vec::new();
    for before in old {
        let key = (before.kind.as_str(), before.id.as_str());
        let Some(after) = current.get(&key) else {
            all.push(DriveChange {
                kind: before.kind.clone(),
                status: "missing".into(),
                before_path: Some(before.path.clone()),
                after_path: None,
                content_evidence:
                    "File may be deleted, trashed, or absent from current local metadata.".into(),
            });
            continue;
        };
        if before.path != after.path {
            let before_name = before.path.rsplit('/').next().unwrap_or("");
            let after_name = after.path.rsplit('/').next().unwrap_or("");
            all.push(DriveChange {
                kind: before.kind.clone(),
                status: if before_name == after_name {
                    "moved"
                } else {
                    "renamed"
                }
                .into(),
                before_path: Some(before.path.clone()),
                after_path: Some(after.path.clone()),
                content_evidence: "Path changed; this does not imply content changed.".into(),
            });
        }
        if before.kind == "file"
            && (before.storage != after.storage || before.fingerprint != after.fingerprint)
        {
            let certain = before.fingerprint.is_some()
                && after.fingerprint.is_some()
                && before.fingerprint != after.fingerprint;
            all.push(DriveChange {
                kind: before.kind.clone(), status: "version_changed".into(),
                before_path: Some(before.path.clone()), after_path: Some(after.path.clone()),
                content_evidence: if certain { "Verified content fingerprints differ." }
                    else { "Storage identity or content evidence changed; bytes are not verified identical/different." }.into(),
            });
        }
    }
    for after in now {
        if !previous.contains_key(&(after.kind.as_str(), after.id.as_str())) {
            all.push(DriveChange {
                kind: after.kind.clone(),
                status: "added".into(),
                before_path: None,
                after_path: Some(after.path.clone()),
                content_evidence:
                    "New to the current local Drive metadata, not necessarily a new cloud upload."
                        .into(),
            });
        }
    }
    let count = all.len();
    let truncated = count > MAX_CHANGES;
    all.truncate(MAX_CHANGES);
    (all, count, truncated)
}

fn inspect(conn: &sqlite::Connection) -> Result<DriveChangeReview, String> {
    let saved = load(conn)?;
    let snap = drive_snapshot(conn)?;
    let current = snapshot_rows(&snap)?;
    let tracked_files = current.iter().filter(|x| x.kind == "file").count();
    let tracked_folders = current.len() - tracked_files;
    let (changes, change_count, truncated) = saved
        .as_ref()
        .map(|s| diff(&s.rows, &current))
        .unwrap_or_default();
    Ok(DriveChangeReview {
        generation: saved.as_ref().map_or(0, |s| s.generation),
        captured_at: saved.as_ref().map(|s| s.at),
        baseline_revision: saved.as_ref().map(|s| s.revision),
        current_revision: snap.revision,
        current_dirty: snap.dirty,
        current_pending: snap.pending.len(),
        tracked_files,
        tracked_folders,
        baseline_count: saved.as_ref().map_or(0, |s| s.rows.len()),
        change_count,
        truncated,
        changes,
    })
}

fn capture(
    conn: &sqlite::Connection,
    expected_generation: i64,
) -> Result<DriveChangeReview, String> {
    let saved = load(conn)?;
    let generation = saved.as_ref().map_or(0, |s| s.generation);
    if expected_generation != generation {
        return Err("Another Drive checkpoint was saved. Refresh before replacing it.".into());
    }
    let snap = drive_snapshot(conn)?;
    if !snap.pending.is_empty() {
        return Err(
            "Finish or recover pending Drive writes before capturing a trustworthy checkpoint."
                .into(),
        );
    }
    if !snap.symlinks.is_empty() {
        return Err(
            "Drive symlinks are not yet covered by checkpoints; no checkpoint was changed.".into(),
        );
    }
    let rows = snapshot_rows(&snap)?;
    let serialized = serde_json::to_string(&rows).map_err(|e| e.to_string())?;
    if serialized.len() > MAX_BASELINE_JSON {
        return Err(
            "Checkpoint metadata exceeds 16 MiB safety cap; no checkpoint was changed.".into(),
        );
    }
    let next_generation = generation
        .checked_add(1)
        .ok_or("Checkpoint generation overflow")?;
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute("SAVEPOINT drive_change_checkpoint")
        .map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        let mut stmt = conn.prepare(
            "INSERT INTO drive_local_change_baseline (id,generation,saved_at,drive_revision,entries_json)
             VALUES(1,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,
                 saved_at=excluded.saved_at, drive_revision=excluded.drive_revision,
                 entries_json=excluded.entries_json",
        ).map_err(|e| e.to_string())?;
        stmt.bind((1, next_generation)).map_err(|e| e.to_string())?;
        stmt.bind((2, now)).map_err(|e| e.to_string())?;
        stmt.bind((3, snap.revision)).map_err(|e| e.to_string())?;
        stmt.bind((4, serialized.as_str()))
            .map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        conn.execute("ROLLBACK TO drive_change_checkpoint; RELEASE drive_change_checkpoint")
            .map_err(|rollback| format!("{e}; rollback failed: {rollback}"))?;
        return Err(e);
    }
    conn.execute("RELEASE drive_change_checkpoint")
        .map_err(|e| e.to_string())?;
    inspect(conn)
}

#[tauri::command]
pub fn cmd_drive_change_review_inspect(
    db: State<'_, DbConnection>,
) -> Result<DriveChangeReview, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    inspect(&conn)
}
#[tauri::command]
pub fn cmd_drive_change_review_capture(
    db: State<'_, DbConnection>,
    expected_generation: i64,
) -> Result<DriveChangeReview, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    capture(&conn, expected_generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;

    fn fixture() -> sqlite::Connection {
        fixture_on(sqlite::open(":memory:").unwrap())
    }

    fn fixture_on(conn: sqlite::Connection) -> sqlite::Connection {
        conn.execute(
            "CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY, backing_channel_id INTEGER NOT NULL UNIQUE,
                name TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'owner',
                storage_version INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, joined_at INTEGER NOT NULL);
             CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY, logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL, display_name TEXT NOT NULL,
                total_size INTEGER NOT NULL, mime_type TEXT, chunk_count INTEGER NOT NULL DEFAULT 1,
                whole_sha256 TEXT, storage_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');
             INSERT INTO logical_channels(logical_id,backing_channel_id,name,created_at,joined_at)
                VALUES('ch',971,'qa',1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,total_size,created_at)
                VALUES('file-a','ch',11,'one.txt',10,1);",
        ).unwrap();
        init_drive_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
                VALUES('folder-a',NULL,'Project',1,1);
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
                VALUES('entry-a','file-a','folder-a','one.txt',1,1);"
        ).unwrap();
        conn
    }

    #[test]
    fn local_checkpoint_survives_restart_without_changing_drive_files() {
        let path = std::env::temp_dir().join(format!(
            "terarelay-change-review-{:032x}.sqlite",
            rand::random::<u128>()
        ));
        {
            let conn = fixture_on(sqlite::open(&path).unwrap());
            let initial = capture(&conn, 0).unwrap();
            assert_eq!(initial.baseline_count, 2);
            assert_eq!(initial.generation, 1);
            assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 1);
        }
        {
            let conn = sqlite::open(&path).unwrap();
            let loaded = inspect(&conn).unwrap();
            assert_eq!(loaded.generation, 1);
            assert_eq!(loaded.change_count, 0);
            conn.execute(
                "UPDATE drive_file_entries SET display_name='after-restart.txt' WHERE entry_id='entry-a'"
            ).unwrap();
            assert!(inspect(&conn)
                .unwrap()
                .changes
                .iter()
                .any(|change| change.status == "renamed"
                    && change.before_path.as_deref() == Some("/Project/one.txt")));
            assert_eq!(drive_snapshot(&conn).unwrap().files.len(), 1);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn different_fingerprint_letter_case_is_not_a_content_change() {
        let conn = fixture();
        let mut baseline = drive_snapshot(&conn).unwrap();
        baseline.files[0].content_fingerprint = Some("A".repeat(64));
        let mut updated = baseline.clone();
        updated.files[0].content_fingerprint = Some("a".repeat(64));
        let before = snapshot_rows(&baseline).unwrap();
        let after = snapshot_rows(&updated).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn baseline_and_diff_are_device_local_and_path_sensitive() {
        let conn = fixture();
        let first = inspect(&conn).unwrap();
        assert_eq!(first.generation, 0);
        assert_eq!((first.tracked_files, first.tracked_folders), (1, 1));
        let captured = capture(&conn, 0).unwrap();
        assert_eq!(captured.generation, 1);
        assert_eq!(captured.change_count, 0);
        conn.execute(
            "UPDATE drive_file_entries SET display_name='renamed.txt' WHERE entry_id='entry-a'",
        )
        .unwrap();
        let report = inspect(&conn).unwrap();
        assert_eq!(report.change_count, 1);
        assert_eq!(report.changes[0].status, "renamed");
        assert_eq!(
            report.changes[0].before_path.as_deref(),
            Some("/Project/one.txt")
        );
        assert_eq!(
            report.changes[0].after_path.as_deref(),
            Some("/Project/renamed.txt")
        );
        assert_eq!(
            capture(&conn, 0).unwrap_err(),
            "Another Drive checkpoint was saved. Refresh before replacing it."
        );
        assert_eq!(capture(&conn, 1).unwrap().generation, 2);
        assert_eq!(inspect(&conn).unwrap().change_count, 0);
    }

    #[test]
    fn parent_move_and_missing_are_not_silently_treated_as_deleted_content() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        conn.execute("UPDATE drive_directories SET name='MovedProject' WHERE id='folder-a'")
            .unwrap();
        let report = inspect(&conn).unwrap();
        assert!(report
            .changes
            .iter()
            .any(|x| x.kind == "folder" && x.status == "renamed"));
        assert!(report
            .changes
            .iter()
            .any(|x| x.kind == "file" && x.status == "moved"));
        conn.execute("UPDATE drive_file_entries SET trashed_at=99 WHERE entry_id='entry-a'")
            .unwrap();
        let report = inspect(&conn).unwrap();
        assert!(report.changes.iter().any(|x| x.kind == "file"
            && x.status == "missing"
            && x.content_evidence.contains("trashed")));
    }

    #[test]
    fn content_fingerprint_changes_and_storage_replacements_are_distinct_evidence() {
        let original = vec![ChangeEntry {
            kind: "file".into(),
            id: "f".into(),
            path: "/alpha".into(),
            storage: "1:old:10".into(),
            fingerprint: Some("a".repeat(64)),
            size: Some(10),
        }];
        let mut updated = original.clone();
        updated[0].storage = "2:new:10".into();
        updated[0].fingerprint = Some("b".repeat(64));
        let (events, n, truncated) = diff(&original, &updated);
        assert_eq!((n, truncated), (1, false));
        assert_eq!(events[0].status, "version_changed");
        assert_eq!(
            events[0].content_evidence,
            "Verified content fingerprints differ."
        );
        updated[0].fingerprint = None;
        let (events, ..) = diff(&original, &updated);
        assert!(events[0].content_evidence.contains("not verified"));
    }

    #[test]
    fn invalid_fingerprint_and_case_collisions_fail_closed() {
        let conn = fixture();
        let mut snapshot = drive_snapshot(&conn).unwrap();
        snapshot.files[0].content_fingerprint = Some("not-a-hash".into());
        assert!(snapshot_rows(&snapshot)
            .unwrap_err()
            .contains("fingerprint"));
        snapshot.files[0].content_fingerprint = None;
        let mut alias = snapshot.files[0].clone();
        alias.entry_id = "another-entry".into();
        alias.display_name = "ONE.TXT".into();
        snapshot.files.push(alias);
        assert!(snapshot_rows(&snapshot)
            .unwrap_err()
            .contains("Ambiguous Drive paths"));
    }

    #[test]
    fn malformed_ancestry_fails_without_overwriting_previous_checkpoint() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        conn.execute("UPDATE drive_directories SET parent_id='folder-a' WHERE id='folder-a'")
            .unwrap();
        let error = capture(&conn, 1).unwrap_err();
        assert!(error.contains("cycle"));
        let saved = load(&conn).unwrap().unwrap();
        assert_eq!(saved.generation, 1);
        assert_eq!(saved.rows.len(), 2);
    }

    #[test]
    fn pending_writes_block_baseline_replacement() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        conn.execute(
            "INSERT INTO drive_pending_files
            (id,display_name,staging_path,size,backing_channel_id,created_at,updated_at)
            VALUES('pending-a','unfinished.txt','/unsafe-never-opened',4,0,1,1)",
        )
        .unwrap();
        assert!(capture(&conn, 1).unwrap_err().contains("pending"));
        assert_eq!(load(&conn).unwrap().unwrap().generation, 1);
    }

    #[test]
    fn transaction_failure_preserves_old_checkpoint_bytes() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        let before = load(&conn).unwrap().unwrap();
        conn.execute(
            "CREATE TRIGGER block_checkpoint_change BEFORE UPDATE ON drive_local_change_baseline
             BEGIN SELECT RAISE(ABORT, 'fixture blocked checkpoint'); END;",
        )
        .unwrap();
        assert!(capture(&conn, 1)
            .unwrap_err()
            .contains("fixture blocked checkpoint"));
        let after = load(&conn).unwrap().unwrap();
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.rows, after.rows);
    }

    #[test]
    fn semantically_corrupt_saved_rows_cannot_fabricate_version_events() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        let original = load(&conn).unwrap().unwrap().rows;
        let file_at = original.iter().position(|row| row.kind == "file").unwrap();
        let folder_at = original
            .iter()
            .position(|row| row.kind == "folder")
            .unwrap();
        let mut cases = Vec::new();

        let mut mismatched_size = original.clone();
        mismatched_size[file_at].storage = "1:file-a:999999".into();
        cases.push(mismatched_size);

        let mut invalid_storage = original.clone();
        invalid_storage[file_at].storage = "99:file-a:10".into();
        cases.push(invalid_storage);

        let mut invalid_fingerprint = original.clone();
        invalid_fingerprint[file_at].fingerprint = Some("not-a-hash".into());
        cases.push(invalid_fingerprint);

        let mut noncanonical_fingerprint = original.clone();
        noncanonical_fingerprint[file_at].fingerprint = Some("A".repeat(64));
        cases.push(noncanonical_fingerprint);

        let mut invalid_folder = original.clone();
        invalid_folder[folder_at].size = Some(100);
        cases.push(invalid_folder);

        let mut duplicate_path = original.clone();
        duplicate_path[file_at].path = original[folder_at].path.to_uppercase();
        cases.push(duplicate_path);

        let mut traversal_path = original.clone();
        traversal_path[file_at].path = "/Project/../escape.txt".into();
        cases.push(traversal_path);

        for corrupt in cases {
            let json = serde_json::to_string(&corrupt).unwrap();
            let mut stmt = conn
                .prepare("UPDATE drive_local_change_baseline SET entries_json=?")
                .unwrap();
            stmt.bind((1, json.as_str())).unwrap();
            stmt.next().unwrap();
            drop(stmt);
            assert!(load(&conn).is_err(), "corrupt baseline must fail closed");
            assert!(
                inspect(&conn).is_err(),
                "invalid checkpoint must never claim a trusted diff"
            );
            assert!(
                capture(&conn, 1).is_err(),
                "invalid checkpoint must not be overwritten"
            );
        }
    }

    #[test]
    fn damaged_persisted_checkpoint_is_not_silently_discarded() {
        let conn = fixture();
        capture(&conn, 0).unwrap();
        conn.execute("UPDATE drive_local_change_baseline SET entries_json='{bad json'")
            .unwrap();
        assert!(inspect(&conn).is_err());
        assert!(capture(&conn, 1).is_err());
    }
}
