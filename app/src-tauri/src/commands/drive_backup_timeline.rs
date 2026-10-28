//! Durable private backup audit timeline. Read-only Drive comparisons.
use crate::commands::drive_backup_watchlist::scan;
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, State};
use tauri::State as TauriState;
const SQL: &str = "CREATE TABLE IF NOT EXISTS drive_backup_timeline
(id INTEGER PRIMARY KEY AUTOINCREMENT,payload TEXT NOT NULL,digest TEXT NOT NULL);";
const LIMIT: usize = 40;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRow {
    pub source_id: String,
    pub backup_id: String,
    pub status: String,
    pub explanation: String,
    pub source_files: usize,
    pub backup_files: usize,
    pub only_source: usize,
    pub only_backup: usize,
    pub different: usize,
    pub unverified: usize,
    pub samples: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditScan {
    pub revision: i64,
    pub dirty: bool,
    pub checked_at: i64,
    pub healthy: usize,
    pub drift: usize,
    pub unverified: usize,
    pub blocked: usize,
    pub checks: Vec<AuditRow>,
}
#[derive(Debug, Serialize)]
pub struct AuditRun {
    pub id: i64,
    pub scan: AuditScan,
}
#[derive(Debug, Serialize)]
pub struct AuditHistory {
    pub runs: Vec<AuditRun>,
    pub max_runs: usize,
}
fn err(e: sqlite::Error) -> String {
    e.to_string()
}
fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn valid(s: &AuditScan) -> bool {
    if s.revision < 0
        || s.checked_at <= 0
        || s.checks.is_empty()
        || s.checks.len() > 24
        || [s.healthy, s.drift, s.unverified, s.blocked]
            .iter()
            .copied()
            .try_fold(0usize, usize::checked_add)
            != Some(s.checks.len())
    {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    let mut counts = [0usize; 4];
    for c in &s.checks {
        if c.source_id.is_empty()
            || c.backup_id.is_empty()
            || c.source_id == c.backup_id
            || c.source_id.len() > 256
            || c.backup_id.len() > 256
            || c.source_id.trim() != c.source_id
            || c.backup_id.trim() != c.backup_id
            || c.source_id
                .chars()
                .any(|x| x.is_control() || x == '/' || x == '\\')
            || c.backup_id
                .chars()
                .any(|x| x.is_control() || x == '/' || x == '\\')
            || [c.source_files, c.backup_files].iter().any(|x| *x > 2000)
            || [c.only_source, c.only_backup, c.different, c.unverified]
                .iter()
                .any(|x| *x > 5000)
            || !seen.insert((&c.source_id, &c.backup_id))
            || c.explanation.is_empty()
            || c.explanation.len() > 1500
            || c.samples.len() > 6
            || c.samples.iter().any(|x| x.len() > 4096)
        {
            return false;
        }
        match c.status.as_str() {
            "healthy"
                if !s.dirty && c.only_source + c.only_backup + c.different + c.unverified == 0 =>
            {
                counts[0] += 1
            }
            "drift" if c.only_source + c.only_backup + c.different > 0 => counts[1] += 1,
            "unverified" => counts[2] += 1,
            "blocked" => counts[3] += 1,
            _ => return false,
        };
    }
    counts == [s.healthy, s.drift, s.unverified, s.blocked]
}
fn history(conn: &Connection) -> Result<AuditHistory, String> {
    conn.execute(SQL).map_err(err)?;
    let mut q = conn
        .prepare(
            "SELECT id,payload,digest FROM drive_backup_timeline
  ORDER BY id DESC LIMIT 41",
        )
        .map_err(err)?;
    let mut runs = Vec::new();
    while q.next().map_err(err)? == State::Row {
        if runs.len() == LIMIT {
            return Err("Backup audit retention is corrupt".into());
        }
        let id = q.read::<i64, _>(0).map_err(err)?;
        let payload = q.read::<String, _>(1).map_err(err)?;
        let digest = q.read::<String, _>(2).map_err(err)?;
        if id <= 0 || payload.len() > 100000 || hash(&payload) != digest {
            return Err("Stored backup audit checksum does not match".into());
        }
        let scan: AuditScan =
            serde_json::from_str(&payload).map_err(|_| "Invalid saved backup audit schema")?;
        if !valid(&scan) {
            return Err("Invalid saved backup audit evidence".into());
        }
        runs.push(AuditRun { id, scan });
    }
    Ok(AuditHistory {
        runs,
        max_runs: LIMIT,
    })
}
fn record(conn: &Connection) -> Result<AuditHistory, String> {
    let current = scan(conn)?;
    if current.checks.is_empty() {
        return Err("Save a backup Watchlist pair before auditing".into());
    }
    let entry = AuditScan {
        revision: current.revision,
        dirty: current.dirty,
        checked_at: current.checked_at,
        healthy: current.healthy,
        drift: current.drift,
        unverified: current.unverified,
        blocked: current.blocked,
        checks: current
            .checks
            .into_iter()
            .map(|c| AuditRow {
                source_id: c.source_id,
                backup_id: c.backup_id,
                status: c.status.to_owned(),
                explanation: c.explanation,
                source_files: c.source_files,
                backup_files: c.backup_files,
                only_source: c.only_source,
                only_backup: c.only_backup,
                different: c.different,
                unverified: c.unverified,
                samples: c.samples,
            })
            .collect(),
    };
    if !valid(&entry) {
        return Err("Backup Watchlist scan cannot be safely recorded".into());
    }
    let before = crate::commands::drive_metadata::drive_snapshot(conn)?;
    if before.revision != entry.revision || before.dirty != entry.dirty {
        return Err("Drive changed during audit; nothing was recorded".into());
    }
    conn.execute(SQL).map_err(err)?;
    conn.execute("SAVEPOINT drive_backup_timeline_save")
        .map_err(err)?;
    let result = (|| {
        let payload = serde_json::to_string(&entry).map_err(|_| "Cannot serialize audit")?;
        if payload.len() > 100000 {
            return Err("Audit evidence exceeds bounded storage".into());
        }
        let digest = hash(&payload);
        let mut q = conn
            .prepare("INSERT INTO drive_backup_timeline(payload,digest) VALUES(?,?)")
            .map_err(err)?;
        q.bind((1, payload.as_str())).map_err(err)?;
        q.bind((2, digest.as_str())).map_err(err)?;
        q.next().map_err(err)?;
        drop(q);
        conn.execute(
            "DELETE FROM drive_backup_timeline WHERE id IN
   (SELECT id FROM drive_backup_timeline ORDER BY id DESC LIMIT -1 OFFSET 40)",
        )
        .map_err(err)?;
        let list = history(conn)?;
        let latest = list
            .runs
            .first()
            .ok_or("Saved audit could not be read back")?;
        if latest.scan.checked_at != entry.checked_at
            || latest.scan.checks.len() != entry.checks.len()
        {
            return Err("Backup audit write was incomplete".into());
        }
        let after = crate::commands::drive_metadata::drive_snapshot(conn)?;
        if after.revision != before.revision || after.dirty != before.dirty {
            return Err("Audit modified Drive metadata unexpectedly".into());
        }
        Ok(list)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_backup_timeline_save", result)
}
fn clear(conn: &Connection) -> Result<AuditHistory, String> {
    conn.execute(SQL).map_err(err)?;
    conn.execute("SAVEPOINT drive_backup_timeline_clear")
        .map_err(err)?;
    let result = (|| {
        conn.execute("DELETE FROM drive_backup_timeline")
            .map_err(err)?;
        let list = history(conn)?;
        if !list.runs.is_empty() {
            return Err("Timeline could not be cleared".into());
        }
        Ok(list)
    })();
    crate::drive_posix::finish_savepoint(conn, "drive_backup_timeline_clear", result)
}
#[tauri::command]
pub fn cmd_drive_backup_timeline_list(
    db: TauriState<'_, DbConnection>,
) -> Result<AuditHistory, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    history(&conn)
}
#[tauri::command]
pub fn cmd_drive_backup_timeline_record(
    db: TauriState<'_, DbConnection>,
) -> Result<AuditHistory, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    record(&conn)
}
#[tauri::command]
pub fn cmd_drive_backup_timeline_clear(
    db: TauriState<'_, DbConnection>,
) -> Result<AuditHistory, String> {
    let conn = db.lock().map_err(|_| "Drive database unavailable")?;
    clear(&conn)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> AuditScan {
        AuditScan {
            revision: 4,
            dirty: false,
            checked_at: 123,
            healthy: 1,
            drift: 0,
            unverified: 0,
            blocked: 0,
            checks: vec![AuditRow {
                source_id: "left".into(),
                backup_id: "right".into(),
                status: "healthy".into(),
                explanation: "Verified metadata only".into(),
                source_files: 2,
                backup_files: 2,
                only_source: 0,
                only_backup: 0,
                different: 0,
                unverified: 0,
                samples: vec![],
            }],
        }
    }
    #[test]
    fn persisted_audit_has_integrity_and_strict_totals() {
        let c = sqlite::open(":memory:").unwrap();
        c.execute(SQL).unwrap();
        let payload = serde_json::to_string(&sample()).unwrap();
        let digest = hash(&payload);
        let mut q = c
            .prepare("INSERT INTO drive_backup_timeline(payload,digest) VALUES(?,?)")
            .unwrap();
        q.bind((1, payload.as_str())).unwrap();
        q.bind((2, digest.as_str())).unwrap();
        q.next().unwrap();
        drop(q);
        assert_eq!(history(&c).unwrap().runs.len(), 1);
        c.execute("UPDATE drive_backup_timeline SET digest='wrong'")
            .unwrap();
        assert!(history(&c).is_err());
    }
    #[test]
    fn contradictory_healthy_or_duplicate_identity_is_refused() {
        let mut scan = sample();
        scan.dirty = true;
        assert!(!valid(&scan));
        scan.dirty = false;
        scan.checks[0].different = 1;
        assert!(!valid(&scan));
        scan.checks[0].different = 0;
        scan.checks.push(scan.checks[0].clone());
        scan.healthy = 2;
        assert!(!valid(&scan));
    }
    #[test]
    fn manual_audit_records_actual_backup_drift_and_preserves_drive_revision() {
        use crate::commands::drive_metadata::{drive_snapshot, init_drive_schema};
        let c = sqlite::open(":memory:").unwrap();
        c.execute(
            "CREATE TABLE logical_channels(logical_id TEXT PRIMARY KEY,
          backing_channel_id INTEGER UNIQUE,name TEXT,role TEXT,storage_version INTEGER,
          created_at INTEGER,joined_at INTEGER);
          CREATE TABLE logical_files(file_id TEXT PRIMARY KEY,logical_channel_id TEXT NOT NULL,
          first_message_id INTEGER NOT NULL,display_name TEXT NOT NULL,total_size INTEGER NOT NULL,
          mime_type TEXT,chunk_count INTEGER NOT NULL DEFAULT 1,whole_sha256 TEXT,
          storage_version INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,
          status TEXT NOT NULL DEFAULT 'complete');",
        )
        .unwrap();
        init_drive_schema(&c).unwrap();
        c.execute("INSERT INTO drive_directories(id,parent_id,name,created_at,updated_at)
          VALUES('src',NULL,'Original',1,1),('bak',NULL,'Backup',1,1),
          ('a','src','Docs',1,1),('b','bak','Docs',1,1);
          INSERT INTO logical_channels(logical_id,backing_channel_id,name,role,storage_version,created_at,joined_at)
          VALUES('qa',5,'QA','owner',1,1,1);
          INSERT INTO logical_files(file_id,logical_channel_id,first_message_id,display_name,
          total_size,chunk_count,whole_sha256,storage_version,created_at,status) VALUES
          ('x','qa',1,'f.txt',5,1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,1,'complete'),
          ('y','qa',2,'f.txt',5,1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,1,'complete');
          INSERT INTO drive_file_entries(entry_id,file_id,directory_id,display_name,created_at,updated_at)
          VALUES('x1','x','a','f.txt',1,1),('y1','y','b','f.txt',1,1);
          CREATE TABLE drive_backup_watchlist(source_id TEXT NOT NULL,backup_id TEXT NOT NULL,
          source_name TEXT NOT NULL,backup_name TEXT NOT NULL,source_parent_id TEXT,backup_parent_id TEXT,
          created_at INTEGER NOT NULL,PRIMARY KEY(source_id,backup_id));
          INSERT INTO drive_backup_watchlist VALUES('src','bak','Original','Backup',NULL,NULL,123);").unwrap();
        let rev = drive_snapshot(&c).unwrap().revision;
        assert_eq!(record(&c).unwrap().runs[0].scan.healthy, 1);
        c.execute("UPDATE drive_file_entries SET trashed_at=99 WHERE entry_id='y1'")
            .unwrap();
        assert_eq!(record(&c).unwrap().runs[0].scan.drift, 1);
        assert_eq!(history(&c).unwrap().runs.len(), 2);
        assert_eq!(drive_snapshot(&c).unwrap().revision, rev);
        c.execute(
            "CREATE TRIGGER fail_backup_audit BEFORE INSERT ON drive_backup_timeline
          BEGIN SELECT RAISE(ABORT,'injected audit write failure'); END;",
        )
        .unwrap();
        assert!(record(&c).is_err());
        assert_eq!(history(&c).unwrap().runs.len(), 2);
        assert_eq!(drive_snapshot(&c).unwrap().revision, rev);
    }
    #[test]
    fn retained_history_is_bounded_and_corruption_fails_closed() {
        let c = sqlite::open(":memory:").unwrap();
        c.execute(SQL).unwrap();
        let payload = serde_json::to_string(&sample()).unwrap();
        let digest = hash(&payload);
        for _ in 0..40 {
            let mut q = c
                .prepare("INSERT INTO drive_backup_timeline(payload,digest) VALUES(?,?)")
                .unwrap();
            q.bind((1, payload.as_str())).unwrap();
            q.bind((2, digest.as_str())).unwrap();
            q.next().unwrap();
        }
        assert_eq!(history(&c).unwrap().runs.len(), 40);
        c.execute(format!(
            "INSERT INTO drive_backup_timeline(payload,digest) VALUES('{}','{}')",
            payload, digest
        ))
        .unwrap();
        assert!(history(&c).is_err());
        c.execute("DELETE FROM drive_backup_timeline WHERE id=(SELECT MIN(id) FROM drive_backup_timeline)").unwrap();
        assert_eq!(history(&c).unwrap().runs.len(), 40);
        c.execute("UPDATE drive_backup_timeline SET payload='{}' WHERE id=(SELECT MAX(id) FROM drive_backup_timeline)").unwrap();
        assert!(history(&c).is_err());
    }
    #[test]
    fn clear_drops_only_local_evidence() {
        let c = sqlite::open(":memory:").unwrap();
        assert!(clear(&c).unwrap().runs.is_empty());
    }
}
