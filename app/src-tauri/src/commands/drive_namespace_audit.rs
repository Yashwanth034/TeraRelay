//! Read-only, bounded Drive namespace and backing-content audit.
//! Does not repair, delete, upload, read file bytes, or traverse local paths.
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, validate_drive_name, DriveSnapshot,
};
use crate::db::DbConnection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tauri::State;

const MAX_RAW: usize = 5_000;
const MAX_ISSUES: usize = 200;
const MAX_ANCESTRY: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct NamespaceAuditIssue {
    pub severity: &'static str,
    pub code: &'static str,
    pub kind: String,
    pub name: String,
    pub explanation: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct NamespaceAuditReport {
    pub revision: i64,
    pub read_only: bool,
    pub raw_files: usize,
    pub visible_files: usize,
    pub folders: usize,
    pub pending: usize,
    pub links: usize,
    pub truncated: bool,
    pub issues: Vec<NamespaceAuditIssue>,
}
#[derive(Clone, Debug)]
struct RawFile {
    kind: String,
    id: String,
    parent: Option<String>,
    name: String,
    backing_ready: bool,
}
fn note(
    report: &mut NamespaceAuditReport,
    severity: &'static str,
    code: &'static str,
    kind: &str,
    name: &str,
    explanation: &'static str,
) {
    if report.issues.len() >= MAX_ISSUES {
        report.truncated = true;
        return;
    }
    // Never embed arbitrary unbounded database strings in the response.
    let name = name.chars().take(255).collect();
    report.issues.push(NamespaceAuditIssue {
        severity,
        code,
        kind: kind.chars().take(32).collect(),
        name,
        explanation,
    });
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && !id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn ancestors(id: Option<&str>, dirs: &HashMap<&str, Option<&str>>) -> Result<(), &'static str> {
    let mut cur = id;
    let mut seen = HashSet::new();
    while let Some(value) = cur {
        if !valid_id(value) || !seen.insert(value) || seen.len() > MAX_ANCESTRY {
            return Err(
                "Directory ancestry has invalid identities, a cycle, or exceeds 512 levels",
            );
        }
        let Some(parent) = dirs.get(value) else {
            return Err("Parent folder is absent from the active Drive namespace");
        };
        cur = *parent;
    }
    Ok(())
}
fn raw_files(conn: &sqlite::Connection) -> Result<(Vec<RawFile>, bool), String> {
    // Deliberately include unavailable backing files; canonical snapshot
    // filters them out, so a snapshot-only doctor would miss the data-loss risk.
    let mut query = conn
        .prepare(
            "SELECT kind, entry_id, directory_id, display_name, backing_ready FROM (
           SELECT 'legacy' AS kind,e.entry_id,e.directory_id,e.display_name,
             CASE WHEN f.file_id IS NOT NULL AND f.status='complete'
                  AND c.logical_id IS NOT NULL THEN 1 ELSE 0 END AS backing_ready
           FROM drive_file_entries e
           LEFT JOIN logical_files f ON f.file_id=e.file_id
           LEFT JOIN logical_channels c ON c.logical_id=f.logical_channel_id
           WHERE e.trashed_at IS NULL
           UNION ALL
           SELECT 'v2' AS kind,e.entry_id,e.directory_id,e.display_name,
             CASE WHEN o.object_id IS NOT NULL THEN 1 ELSE 0 END AS backing_ready
           FROM drive_entries_v2 e LEFT JOIN drive_objects o ON o.object_id=e.object_id
           WHERE e.trashed_at IS NULL
         ) ORDER BY entry_id, kind LIMIT ?",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    query
        .bind((1, (MAX_RAW + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while query.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if rows.len() == MAX_RAW {
            truncated = true;
            break;
        }
        let kind = query.read::<String, _>(0).map_err(|e| e.to_string())?;
        let id = query.read::<String, _>(1).map_err(|e| e.to_string())?;
        let p = query
            .read::<sqlite::Value, _>(2)
            .map_err(|e| e.to_string())?;
        let parent = match p {
            sqlite::Value::Null => None,
            sqlite::Value::String(x) => Some(x),
            _ => return Err("Corrupt Drive raw file parent field".into()),
        };
        let name = query.read::<String, _>(3).map_err(|e| e.to_string())?;
        let ready = query.read::<i64, _>(4).map_err(|e| e.to_string())? == 1;
        rows.push(RawFile {
            kind,
            id,
            parent,
            name,
            backing_ready: ready,
        });
    }
    Ok((rows, truncated))
}
fn inspect(s: &DriveSnapshot, raw: &[RawFile], truncated: bool) -> NamespaceAuditReport {
    let mut report = NamespaceAuditReport {
        revision: s.revision,
        read_only: true,
        raw_files: raw.len(),
        visible_files: s.files.len(),
        folders: s
            .directories
            .iter()
            .filter(|d| d.trashed_at.is_none())
            .count(),
        pending: s.pending.len(),
        links: s.symlinks.iter().filter(|x| x.trashed_at.is_none()).count(),
        truncated,
        issues: Vec::new(),
    };
    let mut folders = HashMap::new();
    for d in s.directories.iter().filter(|d| d.trashed_at.is_none()) {
        if !valid_id(&d.id) || validate_drive_name(&d.name).is_err() {
            note(
                &mut report,
                "critical",
                "invalid_folder",
                "folder",
                &d.name,
                "Active folder has a malformed identity or nonportable filename",
            );
        }
        if folders
            .insert(d.id.as_str(), d.parent_id.as_deref())
            .is_some()
        {
            note(
                &mut report,
                "critical",
                "duplicate_folder_identity",
                "folder",
                &d.name,
                "Two live folder rows share the same identity",
            );
        }
    }
    for d in s.directories.iter().filter(|d| d.trashed_at.is_none()) {
        if let Err(problem) = ancestors(Some(d.id.as_str()), &folders) {
            note(
                &mut report,
                "critical",
                "broken_folder_ancestry",
                "folder",
                &d.name,
                problem,
            );
        }
    }
    // Canonical active V2 supersedes a legacy fallback with the same ID.
    let mut files = HashMap::<&str, &RawFile>::new();
    let mut raw_duplicates = HashSet::new();
    for f in raw {
        if !valid_id(&f.id) || validate_drive_name(&f.name).is_err() {
            note(
                &mut report,
                "critical",
                "invalid_file_identity",
                "file",
                &f.name,
                "Raw file identity or name is invalid",
            );
        }
        if let Some(prior) = files.get(f.id.as_str()) {
            if prior.kind == f.kind {
                raw_duplicates.insert(f.id.as_str());
            } else if prior.name != f.name || prior.parent != f.parent {
                note(
                    &mut report,
                    "warning",
                    "generation_shadow_drift",
                    "file",
                    &f.name,
                    "Legacy and authoritative V2 records disagree on location or name",
                );
            }
            if f.kind == "v2" {
                files.insert(f.id.as_str(), f);
            }
        } else {
            files.insert(f.id.as_str(), f);
        }
    }
    for id in raw_duplicates {
        note(
            &mut report,
            "critical",
            "duplicate_file_identity",
            "file",
            id,
            "Multiple live file rows of the same storage generation share an identity",
        );
    }
    let visible: HashSet<_> = s.files.iter().map(|f| f.entry_id.as_str()).collect();
    let pending_bases: HashSet<_> = s
        .pending
        .iter()
        .filter_map(|p| p.base_entry_id.as_deref())
        .collect();
    let mut places = HashMap::<(Option<String>, String), (String, String)>::new();
    let mut occupied = |report: &mut NamespaceAuditReport,
                        kind: &str,
                        id: &str,
                        name: &str,
                        parent: &Option<String>| {
        let key = (parent.clone(), portable_name_key(name));
        if let Some((old_kind, old_id)) = places.get(&key) {
            if old_kind != kind || old_id != id {
                note(
                    report,
                    "critical",
                    "portable_name_collision",
                    kind,
                    name,
                    "Two active entries have the same portable name within one folder",
                );
            }
        } else {
            places.insert(key, (kind.to_owned(), id.to_owned()));
        }
    };
    for d in s.directories.iter().filter(|d| d.trashed_at.is_none()) {
        occupied(&mut report, "folder", &d.id, &d.name, &d.parent_id);
    }
    for f in files.values() {
        if let Err(problem) = ancestors(f.parent.as_deref(), &folders) {
            note(
                &mut report,
                "critical",
                "orphan_file_parent",
                "file",
                &f.name,
                problem,
            );
        }
        if !f.backing_ready {
            note(&mut report,"critical","missing_file_backing","file",&f.name,
                "Active file record has no available completed content source; preserve metadata and investigate");
        } else if !visible.contains(f.id.as_str()) && !pending_bases.contains(f.id.as_str()) {
            note(
                &mut report,
                "warning",
                "unlisted_file",
                "file",
                &f.name,
                "Active backing record is absent from normal completed Drive inventory",
            );
        }
        if !pending_bases.contains(f.id.as_str()) {
            occupied(&mut report, "file", &f.id, &f.name, &f.parent);
        }
    }
    for p in &s.pending {
        if !valid_id(&p.id) || validate_drive_name(&p.display_name).is_err() {
            note(
                &mut report,
                "warning",
                "invalid_pending_identity",
                "pending",
                &p.display_name,
                "Unfinished staging record has an invalid identity or display name",
            );
        }
        if let Err(problem) = ancestors(p.directory_id.as_deref(), &folders) {
            note(
                &mut report,
                "warning",
                "orphan_pending_parent",
                "pending",
                &p.display_name,
                problem,
            );
        }
        if let Some(ref base) = p.base_entry_id {
            if !files.contains_key(base.as_str()) {
                note(
                    &mut report,
                    "warning",
                    "pending_base_missing",
                    "pending",
                    &p.display_name,
                    "An unfinished copy-on-write edit references a missing live source identity",
                );
            }
        }
        occupied(
            &mut report,
            "pending",
            &p.id,
            &p.display_name,
            &p.directory_id,
        );
    }
    for l in s.symlinks.iter().filter(|x| x.trashed_at.is_none()) {
        if !valid_id(&l.id) || validate_drive_name(&l.display_name).is_err() {
            note(
                &mut report,
                "critical",
                "invalid_link_identity",
                "link",
                &l.display_name,
                "Active symlink has an invalid name or identity",
            );
        }
        if let Err(problem) = ancestors(l.directory_id.as_deref(), &folders) {
            note(
                &mut report,
                "critical",
                "orphan_link_parent",
                "link",
                &l.display_name,
                problem,
            );
        }
        occupied(&mut report, "link", &l.id, &l.display_name, &l.directory_id);
    }
    report
}
#[tauri::command]
pub fn cmd_drive_namespace_audit(
    db: State<'_, DbConnection>,
) -> Result<NamespaceAuditReport, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive metadata database unavailable".to_string())?;
    let snapshot = drive_snapshot(&conn)?;
    let (raw, truncated) = raw_files(&conn)?;
    Ok(inspect(&snapshot, &raw, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::init_drive_schema;
    fn db() -> sqlite::Connection {
        let db = sqlite::open(":memory:").unwrap();
        db.execute(
            "CREATE TABLE logical_channels (
                logical_id TEXT PRIMARY KEY,backing_channel_id INTEGER UNIQUE,
                name TEXT,role TEXT,storage_version INTEGER,created_at INTEGER,joined_at INTEGER);
             CREATE TABLE logical_files (
                file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,
                first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,
                mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
                storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'complete');"
        ).unwrap();
        init_drive_schema(&db).unwrap();
        db.execute(
            "INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
                VALUES('channel',5,'QA','owner',1,1,1);
             INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
                VALUES ('docs',NULL,'Docs',1,1);
             INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
                total_size,chunk_count,created_at,status)
                VALUES ('good','channel',9,'safe.txt',16,1,1,'complete');
             INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
                VALUES ('entry1','good','docs','safe.txt',1,1);"
        ).unwrap();
        db
    }
    #[test]
    fn clean_namespace_has_no_issues() {
        let db = db();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert_eq!(r.raw_files, 1);
        assert!(!r.truncated);
        assert!(r.issues.is_empty());
    }
    #[test]
    fn unavailable_backing_is_reported_even_when_snapshot_hides_file() {
        let db = db();
        db.execute("UPDATE logical_files SET status='missing' WHERE file_id='good'")
            .unwrap();
        let s = drive_snapshot(&db).unwrap();
        assert!(s.files.is_empty());
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert!(r.issues.iter().any(|i| i.code == "missing_file_backing"));
    }
    #[test]
    fn orphan_directory_and_orphan_file_are_reported() {
        let db = db();
        db.execute("UPDATE drive_directories SET parent_id='gone' WHERE id='docs'")
            .unwrap();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert!(r.issues.iter().any(|i| i.code == "broken_folder_ancestry"));
        assert!(r.issues.iter().any(|i| i.code == "orphan_file_parent"));
    }
    #[test]
    fn portable_name_collision_detects_unavailable_file_and_live_folder() {
        let db = db();
        db.execute(
            "INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
            VALUES('folder2','docs','SAFE.TXT',1,1)",
        )
        .unwrap();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert!(r.issues.iter().any(|i| i.code == "portable_name_collision"));
    }
    #[test]
    fn cyclic_folder_graph_is_detected_with_finite_ancestry() {
        let db = db();
        db.execute("UPDATE drive_directories SET parent_id='docs' WHERE id='docs'")
            .unwrap();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert!(r.issues.iter().any(|i| i.code == "broken_folder_ancestry"));
    }
    #[test]
    fn authoritative_v2_shadow_drift_is_visible_without_double_counting() {
        let db = db();
        db.execute(
            "INSERT INTO drive_objects
             (object_id,plaintext_size,encryption_version,crypto_id,content_fingerprint,block_count,created_at)
             VALUES('object2',16,0,NULL,'sha',0,1);
             INSERT INTO drive_entries_v2
             (entry_id,object_id,inode_id,directory_id,display_name,created_at,updated_at,trashed_at)
             VALUES('entry1','object2',NULL,'docs','renamed.txt',1,2,NULL);",
        ).unwrap();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let report = inspect(&s, &raw, cut);
        assert_eq!(report.raw_files, 2);
        assert!(report
            .issues
            .iter()
            .any(|i| i.code == "generation_shadow_drift"));
        assert!(!report
            .issues
            .iter()
            .any(|i| i.code == "portable_name_collision"));
    }

    #[test]
    fn known_pending_copy_on_write_is_not_falsely_called_unlisted() {
        let db = db();
        db.execute(
            "INSERT INTO drive_pending_files(id,directory_id,display_name,staging_path,
          size,format_version,backing_channel_id,base_entry_id,created_at,updated_at)
          VALUES('p1','docs','safe.txt','/ignored',1,1,5,'entry1',1,1)",
        )
        .unwrap();
        let s = drive_snapshot(&db).unwrap();
        let (raw, cut) = raw_files(&db).unwrap();
        let r = inspect(&s, &raw, cut);
        assert!(!r.issues.iter().any(|i| i.code == "unlisted_file"));
    }
}
