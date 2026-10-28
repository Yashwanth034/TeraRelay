//! Full-content, private device-local SHA-256 integrity ledger for completed Linux Drive files.
//! No cloud mutation and no plaintext disk copy. Only reads securely below a live FUSE mount.
use crate::commands::drive_metadata::{
    drive_snapshot, portable_name_key, validate_drive_name, DriveFileRecord, DriveSnapshot,
};
use crate::db::DbConnection;
use crate::drive_mount::DriveMountState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};
use tauri::State;
const MAX_FILES: usize = 100;
const MAX_TOTAL: u64 = 8 * 1024 * 1024 * 1024;
const MAX_LEDGER_BYTES: usize = 65536;
const SQL: &str = "CREATE TABLE IF NOT EXISTS drive_local_content_integrity_ledger (
    id INTEGER PRIMARY KEY CHECK(id = 1), payload TEXT NOT NULL)";
const BUF: usize = 65536;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineRow {
    entry_id: String,
    file_id: String,
    path: String,
    size: u64,
    encryption_version: u32,
    storage_version: u32,
    crypto_id: Option<String>,
    fingerprint: Option<String>,
    updated_at: i64,
    sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    version: u32,
    rows: Vec<BaselineRow>,
}
#[derive(Debug, Clone)]
struct Plan {
    file: DriveFileRecord,
    names: Vec<String>,
    path: String,
    size: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct IntegrityResult {
    pub path: String,
    pub outcome: &'static str,
    pub explanation: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct IntegrityProgress {
    pub id: String,
    pub operation: &'static str,
    pub phase: &'static str,
    pub current: usize,
    pub count: usize,
    pub bytes_read: u64,
    pub total_bytes: u64,
    pub verified: usize,
    pub issues: usize,
    pub results: Vec<IntegrityResult>,
    pub error: Option<String>,
}
struct Job {
    stopped: AtomicBool,
    state: Mutex<IntegrityProgress>,
}
impl Job {
    fn snapshot(&self) -> Result<IntegrityProgress, String> {
        self.state
            .lock()
            .map(|p| p.clone())
            .map_err(|_| "Integrity status unavailable".into())
    }
    fn update(&self, f: impl FnOnce(&mut IntegrityProgress)) {
        if let Ok(mut p) = self.state.lock() {
            f(&mut p)
        }
    }
    fn guard(&self) -> Result<(), String> {
        if self.stopped.load(Ordering::SeqCst) {
            Err("Integrity scan cancelled; baseline unchanged".into())
        } else {
            Ok(())
        }
    }
}
static ACTIVE: LazyLock<Mutex<Option<Arc<Job>>>> = LazyLock::new(|| Mutex::new(None));

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s != "."
        && s != ".."
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}
fn has_pending(snapshot: &DriveSnapshot, f: &DriveFileRecord) -> bool {
    snapshot.pending.iter().any(|p| {
        p.base_entry_id.as_deref() == Some(f.entry_id.as_str())
            || (p.directory_id == f.directory_id && p.display_name == f.display_name)
    })
}
fn plan_file(snapshot: &DriveSnapshot, f: &DriveFileRecord) -> Result<Plan, String> {
    if !valid_id(&f.entry_id)
        || !valid_id(&f.file_id)
        || validate_drive_name(&f.display_name).is_err()
    {
        return Err("Invalid completed Drive file identity or name".into());
    }
    if has_pending(snapshot, f) {
        return Err("Selected Drive file has unfinished local changes".into());
    }
    let size = f.plaintext_size.unwrap_or(f.total_size);
    if size > MAX_TOTAL {
        return Err("Selected Drive file exceeds the integrity scan size limit".into());
    }
    let dirs: HashMap<_, _> = snapshot
        .directories
        .iter()
        .filter(|d| d.trashed_at.is_none())
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut names = vec![f.display_name.clone()];
    let mut parent = f.directory_id.as_deref();
    let mut visited = HashSet::new();
    while let Some(id) = parent {
        if !valid_id(id) || !visited.insert(id) || visited.len() > 100 {
            return Err("Invalid Drive parent ancestry".into());
        }
        let d = dirs
            .get(id)
            .ok_or("Selected Drive parent missing or trashed")?;
        if validate_drive_name(&d.name).is_err() {
            return Err("Unsafe Drive parent name".into());
        }
        names.push(d.name.clone());
        parent = d.parent_id.as_deref();
    }
    names.reverse();
    let path = format!("/{}", names.join("/"));
    if path.len() > 4096 {
        return Err("Selected Drive path is too long".into());
    }
    Ok(Plan {
        file: f.clone(),
        names,
        path,
        size,
    })
}
fn choose(snapshot: &DriveSnapshot, ids: &[String]) -> Result<Vec<Plan>, String> {
    if ids.is_empty() || ids.len() > MAX_FILES {
        return Err("Choose 1–100 completed Drive files".into());
    }
    let mut unique = HashSet::new();
    let mut names = HashSet::new();
    let mut total = 0u64;
    let mut plans = Vec::new();
    for id in ids {
        if !valid_id(id) || !unique.insert(id.as_str()) {
            return Err("Invalid or duplicate Drive identity".into());
        }
        let file = snapshot
            .files
            .iter()
            .find(|f| f.entry_id == *id)
            .ok_or("Selected completed Drive file missing")?;
        let plan = plan_file(snapshot, file)?;
        let key = plan
            .names
            .iter()
            .map(|n| portable_name_key(n))
            .collect::<Vec<_>>()
            .join("/");
        if !names.insert(key) {
            return Err("Drive selection has case-insensitive path collisions".into());
        }
        total = total
            .checked_add(plan.size)
            .ok_or("Integrity scan size overflow")?;
        if total > MAX_TOTAL {
            return Err("Integrity scan exceeds 8 GiB".into());
        }
        plans.push(plan);
    }
    Ok(plans)
}
fn same_identity(a: &DriveFileRecord, b: &DriveFileRecord) -> bool {
    a.entry_id == b.entry_id
        && a.file_id == b.file_id
        && a.inode_id == b.inode_id
        && a.directory_id == b.directory_id
        && a.display_name == b.display_name
        && a.total_size == b.total_size
        && a.plaintext_size == b.plaintext_size
        && a.encryption_version == b.encryption_version
        && a.storage_version == b.storage_version
        && a.crypto_id == b.crypto_id
        && a.content_fingerprint == b.content_fingerprint
        && a.updated_at == b.updated_at
}
fn check_fresh(pool: &DbConnection, revision: i64, plans: &[Plan]) -> Result<(), String> {
    let conn = pool
        .lock()
        .map_err(|_| "Drive DB unavailable".to_string())?;
    let snapshot = drive_snapshot(&conn)?;
    if snapshot.revision != revision {
        return Err("Drive metadata changed during integrity scan".into());
    }
    for p in plans {
        let current = snapshot
            .files
            .iter()
            .find(|f| f.entry_id == p.file.entry_id)
            .ok_or("Drive file disappeared during scan")?;
        if !same_identity(current, &p.file)
            || has_pending(&snapshot, current)
            || plan_file(&snapshot, current).map_or(true, |new| new.path != p.path)
        {
            return Err("Drive file/path/pending state changed during integrity scan".into());
        }
    }
    Ok(())
}
fn open_under(root: &Path, names: &[String]) -> Result<File, String> {
    if names.is_empty() {
        return Err("Missing Drive path".into());
    }
    let mut node = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|e| format!("Drive mount cannot be opened: {e}"))?;
    for (i, name) in names.iter().enumerate() {
        if validate_drive_name(name).is_err() {
            return Err("Unsafe Drive path component".into());
        }
        let c =
            CString::new(std::ffi::OsStr::new(name).as_bytes()).map_err(|_| "NUL in Drive name")?;
        let last = i == names.len() - 1;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(node.as_raw_fd(), c.as_ptr(), flags) };
        if fd < 0 {
            return Err(format!(
                "Drive mounted read failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: Successful openat yields an exclusively owned descriptor.
        let next = unsafe { File::from_raw_fd(fd) };
        let meta = next.metadata().map_err(|e| e.to_string())?;
        if (last && !meta.is_file()) || (!last && !meta.is_dir()) {
            return Err("Drive path changed into unsupported entry".into());
        }
        node = next;
    }
    Ok(node)
}
fn hash_file(root: &Path, plan: &Plan, job: &Job) -> Result<String, String> {
    job.guard()?;
    let mut f = open_under(root, &plan.names)?;
    if f.metadata().map_err(|e| e.to_string())?.len() != plan.size {
        return Err("Mounted file size differs from Drive metadata".into());
    }
    let mut sum = Sha256::new();
    let mut size = 0u64;
    let mut buf = [0u8; BUF];
    loop {
        job.guard()?;
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("Mounted content read failed: {e}"))?;
        if n == 0 {
            break;
        }
        size = size.checked_add(n as u64).ok_or("Drive read overflow")?;
        if size > plan.size {
            return Err("Mounted file grew during integrity scan".into());
        }
        sum.update(&buf[..n]);
        job.update(|p| p.bytes_read = p.bytes_read.saturating_add(n as u64));
    }
    if size != plan.size {
        return Err("Mounted file ended before its declared size".into());
    }
    Ok(format!("{:x}", sum.finalize()))
}
fn baseline_row(p: &Plan, digest: String) -> BaselineRow {
    BaselineRow {
        entry_id: p.file.entry_id.clone(),
        file_id: p.file.file_id.clone(),
        path: p.path.clone(),
        size: p.size,
        encryption_version: p.file.encryption_version,
        storage_version: p.file.storage_version,
        crypto_id: p.file.crypto_id.clone(),
        fingerprint: p.file.content_fingerprint.clone(),
        updated_at: p.file.updated_at,
        sha256: digest,
    }
}
fn matching(row: &BaselineRow, p: &Plan) -> bool {
    row.file_id == p.file.file_id
        && row.path == p.path
        && row.size == p.size
        && row.encryption_version == p.file.encryption_version
        && row.storage_version == p.file.storage_version
        && row.crypto_id == p.file.crypto_id
        && row.fingerprint == p.file.content_fingerprint
        && row.updated_at == p.file.updated_at
}
fn valid_ledger(b: &Baseline) -> Result<(), String> {
    if b.version != 1 || b.rows.is_empty() || b.rows.len() > MAX_FILES {
        return Err("Invalid stored integrity ledger version or size".into());
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut size = 0u64;
    for r in &b.rows {
        if !valid_id(&r.entry_id)
            || !valid_id(&r.file_id)
            || r.path.len() > 4096
            || !r.path.starts_with('/')
            || r.path
                .split('/')
                .skip(1)
                .any(|p| validate_drive_name(p).is_err())
            || !ids.insert(r.entry_id.clone())
            || !paths.insert(r.path.to_lowercase())
            || r.sha256.len() != 64
            || !r
                .sha256
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !(b'A'..=b'F').contains(&c))
        {
            return Err("Invalid stored integrity ledger record".into());
        }
        size = size
            .checked_add(r.size)
            .ok_or("Invalid stored integrity total")?;
        if size > MAX_TOTAL {
            return Err("Stored integrity ledger exceeds 8 GiB".into());
        }
    }
    Ok(())
}
fn load(conn: &sqlite::Connection) -> Result<Option<Baseline>, String> {
    conn.execute(SQL)
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut q = conn
        .prepare("SELECT payload FROM drive_local_content_integrity_ledger WHERE id=1")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if q.next().map_err(|e| e.to_string())? != sqlite::State::Row {
        return Ok(None);
    }
    if !matches!(
        q.read::<sqlite::Value, _>(0).map_err(|e| e.to_string())?,
        sqlite::Value::String(_)
    ) {
        return Err("Corrupt integrity ledger data type".into());
    }
    let value = q.read::<Vec<u8>, _>(0).map_err(|e| e.to_string())?;
    if value.len() > MAX_LEDGER_BYTES {
        return Err("Stored integrity ledger exceeds size limit".into());
    }
    let baseline: Baseline =
        serde_json::from_slice(&value).map_err(|_| "Stored integrity ledger is corrupt")?;
    valid_ledger(&baseline)?;
    Ok(Some(baseline))
}
fn save(conn: &sqlite::Connection, baseline: &Baseline) -> Result<(), String> {
    valid_ledger(baseline)?;
    let serialized = serde_json::to_string(baseline).map_err(|e| e.to_string())?;
    if serialized.len() > MAX_LEDGER_BYTES {
        return Err("Integrity ledger metadata exceeds size limit".into());
    }
    conn.execute(SQL)
        .map_err(|e: sqlite::Error| e.to_string())?;
    let mut q = conn
        .prepare(
            "INSERT OR REPLACE INTO drive_local_content_integrity_ledger(id,payload) VALUES(1,?)",
        )
        .map_err(|e: sqlite::Error| e.to_string())?;
    q.bind((1, serialized.as_str()))
        .map_err(|e| e.to_string())?;
    q.next().map_err(|e| e.to_string())?;
    Ok(())
}
#[tauri::command]
pub fn cmd_drive_integrity_ledger_info(
    db: State<'_, DbConnection>,
) -> Result<Option<Vec<String>>, String> {
    let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
    Ok(load(&conn)?.map(|b| b.rows.into_iter().map(|r| r.path).collect()))
}
#[tauri::command]
pub fn cmd_drive_integrity_ledger_status() -> Result<Option<IntegrityProgress>, String> {
    let job = ACTIVE
        .lock()
        .map_err(|_| "Integrity worker unavailable".to_string())?;
    job.as_ref().map(|j| j.snapshot()).transpose()
}
#[tauri::command]
pub fn cmd_drive_integrity_ledger_cancel(id: String) -> Result<bool, String> {
    let job = ACTIVE
        .lock()
        .map_err(|_| "Integrity worker unavailable".to_string())?;
    let running = job.as_ref().ok_or("No integrity scan available")?;
    if running.snapshot()?.id != id {
        return Err("Integrity scan changed; refresh status".into());
    }
    if running.snapshot()?.phase != "running" {
        return Ok(false);
    }
    running.stopped.store(true, Ordering::SeqCst);
    Ok(true)
}
#[tauri::command]
pub fn cmd_drive_integrity_ledger_start(
    db: State<'_, DbConnection>,
    mount: State<'_, DriveMountState>,
    operation: String,
    entry_ids: Vec<String>,
) -> Result<IntegrityProgress, String> {
    if operation != "capture" && operation != "verify" {
        return Err("Unknown integrity ledger operation".into());
    }
    let context = mount
        .restore_context()?
        .ok_or("Active Linux Drive mount required")?;
    let pool = db.inner().clone();
    let (plans, revision, previous) = {
        let conn = pool
            .lock()
            .map_err(|_| "Drive DB unavailable".to_string())?;
        let snap = drive_snapshot(&conn)?;
        let previous = if operation == "verify" {
            Some(load(&conn)?.ok_or("No local integrity baseline saved")?)
        } else {
            None
        };
        let ids = if let Some(ref b) = previous {
            b.rows
                .iter()
                .map(|r| r.entry_id.clone())
                .collect::<Vec<_>>()
        } else {
            entry_ids
        };
        let plans = if previous.is_none() {
            choose(&snap, &ids)?
        } else {
            // Verification includes disappeared or pending entries as findings, not fake hash matches.
            let mut out = Vec::new();
            for id in &ids {
                if let Some(file) = snap.files.iter().find(|f| f.entry_id == *id) {
                    if let Ok(p) = plan_file(&snap, file) {
                        out.push(p)
                    }
                }
            }
            out
        };
        (plans, snap.revision, previous)
    };
    let all_count = previous.as_ref().map_or(plans.len(), |p| p.rows.len());
    let total = plans.iter().map(|p| p.size).sum();
    let mut slot = ACTIVE
        .lock()
        .map_err(|_| "Integrity worker unavailable".to_string())?;
    if slot
        .as_ref()
        .is_some_and(|j| j.snapshot().map(|p| p.phase == "running").unwrap_or(true))
    {
        return Err("Another integrity scan is already running".into());
    }
    let job = Arc::new(Job {
        stopped: AtomicBool::new(false),
        state: Mutex::new(IntegrityProgress {
            id: format!("integrity-{:032x}", rand::random::<u128>()),
            operation: if operation == "capture" {
                "capture"
            } else {
                "verify"
            },
            phase: "running",
            current: 0,
            count: all_count,
            bytes_read: 0,
            total_bytes: total,
            verified: 0,
            issues: 0,
            results: Vec::new(),
            error: None,
        }),
    });
    let initial = job.snapshot()?;
    *slot = Some(job.clone());
    drop(slot);
    let mount = mount.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = run_scan(&pool, &mount, &context, revision, &plans, previous, &job);
        job.update(|p| match result {
            Ok(()) => p.phase = "done",
            Err(e) => {
                p.phase = if job.stopped.load(Ordering::SeqCst) {
                    "cancelled"
                } else {
                    "failed"
                };
                p.error = Some(e)
            }
        });
    });
    Ok(initial)
}
/// The capture is built entirely in memory. Publication happens only after
/// every full-file hash and a final live metadata/mount guard succeeds.
fn capture_content(
    root: &Path,
    plans: &[Plan],
    job: &Job,
    mut fresh: impl FnMut() -> Result<(), String>,
) -> Result<Vec<BaselineRow>, String> {
    let mut captured = Vec::with_capacity(plans.len());
    for p in plans {
        fresh()?;
        let digest = hash_file(root, p, job)?;
        fresh()?;
        captured.push(baseline_row(p, digest));
        job.update(|state| {
            state.current += 1;
            state.verified += 1;
        });
    }
    fresh()?;
    Ok(captured)
}

fn run_scan(
    pool: &DbConnection,
    mount: &DriveMountState,
    context: &(u64, PathBuf),
    revision: i64,
    plans: &[Plan],
    previous: Option<Baseline>,
    job: &Arc<Job>,
) -> Result<(), String> {
    let fresh = || {
        job.guard()?;
        if mount.restore_context()?.as_ref() != Some(context) {
            return Err("Drive mount changed during scan".into());
        }
        check_fresh(pool, revision, plans)
    };
    fresh()?;
    let indexed: HashMap<_, _> = plans
        .iter()
        .map(|p| (p.file.entry_id.as_str(), p))
        .collect();
    if let Some(ref old) = previous {
        for row in &old.rows {
            job.guard()?;
            fresh()?;
            let Some(p) = indexed.get(row.entry_id.as_str()) else {
                let reason = match pool.lock() {
                    Ok(conn) => match drive_snapshot(&conn) {
                        Ok(snap) => match snap.files.iter().find(|f| f.entry_id == row.entry_id) {
                            None => "missing",
                            Some(f) if has_pending(&snap, f) => "pending",
                            _ => "unsafe_path",
                        },
                        Err(_) => "unavailable",
                    },
                    Err(_) => "unavailable",
                };
                job.update(|status| {
                    status.issues += 1;
                    status.current += 1;
                    status.results.push(IntegrityResult {
                        path: row.path.clone(),
                        outcome: reason,
                        explanation: "Content was not read or compared".into(),
                    });
                });
                continue;
            };
            if !matching(row, p) {
                job.update(|status| {
                    status.issues += 1;
                    status.current += 1;
                    status.results.push(IntegrityResult {
                        path: row.path.clone(),
                        outcome: "metadata_changed",
                        explanation:
                            "Drive identity, location, or content metadata changed since baseline"
                                .into(),
                    });
                });
                continue;
            }
            let digest = hash_file(&context.1, p, job);
            fresh()?;
            match digest {
                Ok(v) => {
                    let match_hash = v == row.sha256;
                    job.update(|status| {
                        status.current += 1;
                        if match_hash {
                            status.verified += 1
                        } else {
                            status.issues += 1
                        }
                        status.results.push(IntegrityResult {
                            path: row.path.clone(),
                            outcome: if match_hash {
                                "verified"
                            } else {
                                "hash_mismatch"
                            },
                            explanation: if match_hash {
                                "Full mounted file SHA-256 matches local baseline".into()
                            } else {
                                "Mounted bytes differ from the previously captured baseline".into()
                            },
                        });
                    });
                }
                Err(e) => {
                    job.guard()?;
                    job.update(|status| {
                        status.current += 1;
                        status.issues += 1;
                        status.results.push(IntegrityResult {
                            path: row.path.clone(),
                            outcome: "read_error",
                            explanation: e,
                        });
                    });
                }
            }
        }
    } else {
        let captured = capture_content(&context.1, plans, job, fresh)?;
        // The prior baseline is retained until EVERY scan completes and validates.
        let conn = pool
            .lock()
            .map_err(|_| "Drive DB unavailable before baseline publication".to_string())?;
        job.guard()?;
        let snap = drive_snapshot(&conn)?;
        if snap.revision != revision
            || plans.iter().any(|p| {
                snap.files
                    .iter()
                    .find(|f| f.entry_id == p.file.entry_id)
                    .is_none_or(|f| {
                        !same_identity(f, &p.file)
                            || has_pending(&snap, f)
                            || plan_file(&snap, f).map_or(true, |new| new.path != p.path)
                    })
            })
        {
            return Err("Drive changed before local integrity baseline was committed".into());
        }
        job.guard()?;
        save(
            &conn,
            &Baseline {
                version: 1,
                rows: captured,
            },
        )?;
        // Publication succeeded: a late cancellation cannot label a committed
        // replacement baseline as "cancelled and unchanged".
        return Ok(());
    }
    job.guard()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::drive_metadata::DriveDirectoryRecord;
    fn fixture() -> (PathBuf, DriveSnapshot) {
        let root =
            std::env::temp_dir().join(format!("trdrive-integrity-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(root.join("Docs")).unwrap();
        std::fs::write(root.join("Docs/a.txt"), b"original content").unwrap();
        let file = DriveFileRecord {
            entry_id: "entry-a".into(),
            file_id: "file-a".into(),
            inode_id: None,
            directory_id: Some("docs".into()),
            display_name: "a.txt".into(),
            total_size: 16,
            mime_type: None,
            first_message_id: 1,
            backing_channel_id: 1,
            logical_channel_id: "test".into(),
            created_at: 1,
            updated_at: 2,
            encryption_version: 1,
            plaintext_size: Some(16),
            crypto_id: Some("key".into()),
            content_fingerprint: Some("manifest-hash".into()),
            storage_version: 1,
        };
        let snap = DriveSnapshot {
            revision: 1,
            dirty: false,
            directories: vec![DriveDirectoryRecord {
                id: "docs".into(),
                parent_id: None,
                name: "Docs".into(),
                backing_logical_channel_id: None,
                created_at: 1,
                updated_at: 1,
                trashed_at: None,
            }],
            files: vec![file],
            pending: vec![],
            symlinks: vec![],
        };
        (root, snap)
    }
    fn dummy() -> Job {
        Job {
            stopped: AtomicBool::new(false),
            state: Mutex::new(IntegrityProgress {
                id: "test".into(),
                operation: "verify",
                phase: "running",
                current: 0,
                count: 1,
                bytes_read: 0,
                total_bytes: 16,
                verified: 0,
                issues: 0,
                results: vec![],
                error: None,
            }),
        }
    }
    #[test]
    fn full_byte_hash_differs_if_content_changes_without_metadata_update() {
        let (root, snap) = fixture();
        let p = choose(&snap, &["entry-a".into()]).unwrap();
        let old = hash_file(&root, &p[0], &dummy()).unwrap();
        std::fs::write(root.join("Docs/a.txt"), b"altered! content").unwrap();
        let new = hash_file(&root, &p[0], &dummy()).unwrap();
        assert_ne!(old, new);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn no_follow_symlink_leaf_and_parent() {
        let (root, snap) = fixture();
        let p = choose(&snap, &["entry-a".into()]).unwrap();
        let outside = root.join("external");
        std::fs::write(&outside, b"personal contents").unwrap();
        std::fs::remove_file(root.join("Docs/a.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("Docs/a.txt")).unwrap();
        assert!(hash_file(&root, &p[0], &dummy()).is_err());
        std::fs::remove_file(root.join("Docs/a.txt")).unwrap();
        std::fs::remove_dir(root.join("Docs")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("Docs")).unwrap();
        assert!(hash_file(&root, &p[0], &dummy()).is_err());
        std::fs::remove_file(root.join("Docs")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pending_same_name_and_base_alias_are_both_rejected() {
        let (_, mut snap) = fixture();
        let mut p = crate::commands::drive_metadata::DrivePendingRecord {
            id: "p".into(),
            directory_id: Some("docs".into()),
            display_name: "a.txt".into(),
            staging_path: "".into(),
            size: 16,
            format_version: 1,
            backing_channel_id: 0,
            base_entry_id: None,
            encryption_version: 0,
            crypto_id: None,
            closed_at: None,
            created_at: 1,
            updated_at: 1,
        };
        snap.pending.push(p.clone());
        assert!(choose(&snap, &["entry-a".into()]).is_err());
        snap.pending.clear();
        p.display_name = "other.txt".into();
        p.base_entry_id = Some("entry-a".into());
        snap.pending.push(p);
        assert!(choose(&snap, &["entry-a".into()]).is_err());
    }
    #[test]
    fn persistent_ledger_roundtrip_and_corruption_fail_closed() {
        let conn = sqlite::open(":memory:").unwrap();
        let (root, snap) = fixture();
        let p = choose(&snap, &["entry-a".into()]).unwrap();
        let hash = hash_file(&root, &p[0], &dummy()).unwrap();
        let b = Baseline {
            version: 1,
            rows: vec![baseline_row(&p[0], hash)],
        };
        save(&conn, &b).unwrap();
        let saved = load(&conn).unwrap().unwrap();
        assert_eq!(saved.rows[0].entry_id, "entry-a");
        conn.execute("UPDATE drive_local_content_integrity_ledger SET payload = '{\"version\":100,\"rows\":[]}'").unwrap();
        assert!(load(&conn).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn capture_aborts_mid_scan_without_publishing_partial_baseline() {
        let (root, snapshot) = fixture();
        let plans = choose(&snapshot, &["entry-a".into()]).unwrap();
        let db = sqlite::open(":memory:").unwrap();
        let original = Baseline {
            version: 1,
            rows: vec![baseline_row(&plans[0], "a".repeat(64))],
        };
        save(&db, &original).unwrap();
        let mut calls = 0;
        let result = capture_content(&root, &plans, &dummy(), || {
            calls += 1;
            if calls == 2 {
                Err("pending content appeared during read".into())
            } else {
                Ok(())
            }
        });
        assert!(result.unwrap_err().contains("pending content"));
        assert_eq!(load(&db).unwrap().unwrap().rows[0].sha256, "a".repeat(64));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn capture_hashes_every_byte_before_completing() {
        let (root, snapshot) = fixture();
        let plans = choose(&snapshot, &["entry-a".into()]).unwrap();
        let job = dummy();
        let records = capture_content(&root, &plans, &job, || Ok(())).unwrap();
        assert_eq!(records.len(), 1);
        assert!(matching(&records[0], &plans[0]));
        assert_eq!(
            records[0].sha256,
            format!("{:x}", Sha256::digest(b"original content"))
        );
        assert_eq!(job.snapshot().unwrap().verified, 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelled_scan_retains_unfinished_baseline() {
        let job = dummy();
        job.stopped.store(true, Ordering::SeqCst);
        assert!(job.guard().is_err());
        let conn = sqlite::open(":memory:").unwrap();
        assert!(load(&conn).unwrap().is_none());
    }
}
