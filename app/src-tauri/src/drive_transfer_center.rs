//! User-controlled view of the SAME native deferred-upload retry planner.
//! No second retry loop, cloud deletion, stage cleanup or plaintext reads.
use super::{desktop, recovery_scheduler::DeferredWritePlanner, DriveMountState};
use crate::commands::drive_metadata::drive_pending_by_id;
use crate::db::DbConnection;
use serde::Serialize;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

const MAX_VISIBLE: usize = 100;
type PendingInventoryRow = (String, String, u64);

pub(super) struct QueueRuntime {
    since: Instant,
    pub(super) paused: bool,
    planner: DeferredWritePlanner,
    in_flight: HashSet<String>,
}
impl Default for QueueRuntime {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            paused: false,
            planner: DeferredWritePlanner::default(),
            in_flight: HashSet::new(),
        }
    }
}
impl QueueRuntime {
    fn elapsed(&self) -> Duration {
        self.since.elapsed()
    }

    /// Called exclusively by the existing native worker. Mark candidates as
    /// in flight before release of this mutex; manual retries use this claim.
    pub(super) fn schedule(&mut self, ids: &[String]) -> Vec<String> {
        if self.paused {
            return Vec::new();
        }
        let now = self.elapsed();
        let result = self.planner.select_excluding(ids, &self.in_flight, now);
        for id in &result {
            self.in_flight.insert(id.clone());
        }
        result
    }
    fn begin_manual(&mut self, id: &str) -> bool {
        self.in_flight.insert(id.to_string())
    }
    /// Record outcomes, including network initialization errors, and always
    /// release ephemeral claims. A skipped native writer is NOT a failure.
    pub(super) fn finish(
        &mut self,
        selected: &[String],
        attempted: &[(String, bool)],
        batch_error: bool,
    ) {
        let now = self.elapsed();
        for (id, success) in attempted {
            self.planner.complete(id, *success, now);
        }
        if batch_error {
            for id in selected {
                self.planner.complete(id, false, now);
            }
        }
        for id in selected {
            self.in_flight.remove(id);
        }
    }

    fn status(&self, id: &str) -> (&'static str, u32, u64) {
        let (attempts, retry_in_seconds) = self.planner.retry_state(id, self.elapsed());
        let phase = if self.in_flight.contains(id) {
            "uploading"
        } else if self.paused {
            "paused"
        } else if retry_in_seconds > 0 {
            "backoff"
        } else {
            "queued"
        };
        (phase, attempts, retry_in_seconds)
    }
}

#[derive(Debug, Serialize)]
pub struct PendingTransfer {
    pub pending_id: String,
    pub file_name: String,
    pub size: u64,
    pub status: &'static str,
    pub failed_attempts: u32,
    pub retry_in_seconds: u64,
}
#[derive(Debug, Serialize)]
pub struct TransferSnapshot {
    pub paused: bool,
    pub automatic_worker_running: bool,
    pub total_pending: u32,
    pub truncated: bool,
    pub transfers: Vec<PendingTransfer>,
}
#[derive(Debug, Serialize)]
pub struct TransferAction {
    pub attempted: bool,
    pub recovered: bool,
    pub remains_pending: bool,
}

/// SQLite numeric/string coercions are not accepted for a recovery queue:
/// malformed metadata must never appear to be a healthy staged file.
fn closed_inventory(conn: &sqlite::Connection) -> Result<(Vec<PendingInventoryRow>, u32), String> {
    let total = super::closed_pending_count(conn)?;
    let mut query = conn.prepare(
        "SELECT id,display_name,size FROM drive_pending_files WHERE closed_at IS NOT NULL ORDER BY id LIMIT ?"
    ).map_err(|e: sqlite::Error| e.to_string())?;
    query
        .bind((1, (MAX_VISIBLE + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    while query.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        let id = match query
            .read::<sqlite::Value, _>(0)
            .map_err(|e| e.to_string())?
        {
            sqlite::Value::String(value) => value,
            _ => return Err("Invalid pending upload ID field type".into()),
        };
        let name = match query
            .read::<sqlite::Value, _>(1)
            .map_err(|e| e.to_string())?
        {
            sqlite::Value::String(value) => value,
            _ => return Err("Invalid pending upload name field type".into()),
        };
        let length = match query
            .read::<sqlite::Value, _>(2)
            .map_err(|e| e.to_string())?
        {
            sqlite::Value::Integer(value) => value,
            _ => return Err("Invalid pending upload size field type".into()),
        };
        let size = u64::try_from(length).map_err(|_| "Invalid pending upload size".to_string())?;
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || name.is_empty()
            || name.len() > 1024
        {
            return Err("Invalid pending upload inventory".into());
        }
        rows.push((id, name, size));
    }
    Ok((rows, total))
}

/// Safe read-only, bounded transfer inventory; no whole Drive enumeration.
#[tauri::command]
pub fn cmd_drive_transfer_snapshot(
    mount: State<'_, DriveMountState>,
    db: State<'_, DbConnection>,
) -> Result<TransferSnapshot, String> {
    let (rows, total_pending) = {
        let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
        closed_inventory(&conn)?
    };
    let visible = rows.len().min(MAX_VISIBLE);
    let runtime = mount
        .inner
        .transfer
        .lock()
        .map_err(|_| "Transfer state unavailable".to_string())?;
    let mut transfers = Vec::with_capacity(visible);
    for (id, file_name, size) in rows.into_iter().take(MAX_VISIBLE) {
        let (status, attempts, retry_in_seconds) = runtime.status(&id);
        transfers.push(PendingTransfer {
            pending_id: id,
            file_name,
            size,
            status,
            failed_attempts: attempts,
            retry_in_seconds,
        });
    }
    Ok(TransferSnapshot {
        paused: runtime.paused,
        automatic_worker_running: mount
            .inner
            .automatic_upload_recovery_running
            .load(std::sync::atomic::Ordering::SeqCst),
        total_pending,
        truncated: total_pending as usize > visible,
        transfers,
    })
}

/// Session-local pause. Does not cancel an already-running Telegram send, and
/// never disables explicit manual recovery or alters persisted pending stages.
#[tauri::command]
pub fn cmd_drive_transfer_pause(
    mount: State<'_, DriveMountState>,
    paused: bool,
) -> Result<bool, String> {
    let mut runtime = mount
        .inner
        .transfer
        .lock()
        .map_err(|_| "Transfer state unavailable".to_string())?;
    runtime.paused = paused;
    Ok(runtime.paused)
}

/// Explicitly retry exactly one closed pending write. Never upload arbitrary
/// IDs or retry on a signed-out/unmounted provider. Native single-flight
/// claim in recover_pending_selected_impl excludes FUSE writers as well.
#[tauri::command]
pub async fn cmd_drive_transfer_retry(
    app: AppHandle,
    pending_id: String,
) -> Result<TransferAction, String> {
    if pending_id.is_empty()
        || pending_id.len() > 256
        || !pending_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Invalid pending upload identity".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let mount = app.state::<DriveMountState>();
        if crate::commands::qa_feature_a::enabled() {
            return Err("Remote Drive transfers are disabled in isolated QA".to_string());
        }
        let Some(epoch) = mount.inner.requested_epoch()? else {
            return Err("Drive is not mounted; recovery did not start".into());
        };
        if !mount.status().mounted {
            return Err("Drive is disconnected; recovery did not start".into());
        }
        let db = app.state::<DbConnection>();
        let closed = {
            let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
            drive_pending_by_id(&conn, &pending_id)?.is_some_and(|row| row.closed_at.is_some())
        };
        if !closed {
            return Err("Pending file is missing or still open for writing".into());
        }
        {
            let mut runtime = mount
                .inner
                .transfer
                .lock()
                .map_err(|_| "Transfer state unavailable".to_string())?;
            if !runtime.begin_manual(&pending_id) {
                return Err("That upload is already running".into());
            }
        }
        let chosen = vec![pending_id.clone()];
        let outcome = desktop::recover_pending_selected_impl(
            app.clone(),
            mount,
            db,
            app.state::<crate::commands::TelegramState>(),
            Some(&chosen),
            Some(epoch),
        );
        let (attempted, recovered) = match &outcome {
            Ok((_, attempts)) => (
                attempts.iter().any(|(id, _)| id == &pending_id),
                attempts.iter().any(|(id, ok)| id == &pending_id && *ok),
            ),
            Err(_) => (true, false),
        };
        {
            let state = app.state::<DriveMountState>();
            let mut runtime = state
                .inner
                .transfer
                .lock()
                .map_err(|_| "Transfer state unavailable".to_string())?;
            runtime.finish(
                &chosen,
                &outcome
                    .as_ref()
                    .map(|(_, attempts)| attempts.clone())
                    .unwrap_or_default(),
                outcome.is_err(),
            );
        }
        let result = outcome?;
        let remains_pending = {
            let db = app.state::<DbConnection>();
            let conn = db.lock().map_err(|_| "Drive DB unavailable".to_string())?;
            drive_pending_by_id(&conn, &pending_id)?.is_some()
        };
        let _ = result;
        Ok(TransferAction {
            attempted,
            recovered,
            remains_pending,
        })
    })
    .await
    .map_err(|_| "Drive transfer worker stopped unexpectedly".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|id| id.to_string()).collect()
    }
    #[test]
    fn pause_prevents_new_automatic_attempts_without_erasing_retries() {
        let mut runtime = QueueRuntime {
            paused: true,
            ..Default::default()
        };
        assert!(runtime.schedule(&strings(&["a"])).is_empty());
        runtime.paused = false;
        let first = runtime.schedule(&strings(&["a"]));
        assert_eq!(first, strings(&["a"]));
        assert!(runtime.schedule(&strings(&["a"])).is_empty());
        runtime.finish(&first, &[("a".into(), false)], false);
        assert_eq!(runtime.status("a").0, "backoff");
        runtime.paused = true;
        assert_eq!(runtime.status("a").0, "paused");
        assert!(runtime.schedule(&strings(&["a"])).is_empty());
        runtime.paused = false;
        assert_eq!(runtime.status("a").1, 1);
    }
    #[test]
    fn errors_release_claims_and_backoff_is_visible() {
        let mut runtime = QueueRuntime::default();
        let selected = runtime.schedule(&strings(&["a", "b", "c"]));
        assert_eq!(selected.len(), 2);
        runtime.finish(&selected, &[], true);
        assert!(runtime.in_flight.is_empty());
        assert_eq!(runtime.status("a").1, 1);
        assert!(runtime.status("a").2 > 0);
        assert_eq!(
            runtime.schedule(&strings(&["a", "b", "c"])),
            strings(&["c"])
        );
    }
    #[test]
    fn manual_claim_is_single_flight_and_persistent_pause_does_not_lock_it() {
        let mut runtime = QueueRuntime {
            paused: true,
            ..Default::default()
        };
        assert!(runtime.begin_manual("x"));
        assert!(!runtime.begin_manual("x"));
        assert_eq!(runtime.status("x").0, "uploading");
        runtime.finish(&strings(&["x"]), &[("x".into(), true)], false);
        assert!(runtime.begin_manual("x"));
    }

    #[test]
    fn manual_retry_ownership_never_resets_automatic_failure_history() {
        let mut runtime = QueueRuntime::default();
        let first = runtime.schedule(&strings(&["a"]));
        runtime.finish(&first, &[("a".into(), false)], false);
        assert_eq!(runtime.status("a").1, 1);
        assert!(runtime.begin_manual("a"));
        assert_eq!(runtime.schedule(&strings(&["a", "b"])), strings(&["b"]));
        assert_eq!(runtime.status("a").1, 1);
        runtime.finish(&strings(&["a"]), &[("a".into(), false)], false);
        assert_eq!(runtime.status("a").1, 2);
    }

    #[test]
    fn pending_inventory_is_bounded_and_rejects_sqlite_coercion() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE drive_pending_files(
            id TEXT, display_name TEXT, size INTEGER, closed_at INTEGER);
            INSERT INTO drive_pending_files VALUES
                ('a1','good.bin',4,8), ('still-open','working.bin',99,NULL);",
        )
        .unwrap();
        let (rows, total) = closed_inventory(&conn).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows, vec![("a1".into(), "good.bin".into(), 4)]);
        conn.execute("UPDATE drive_pending_files SET size='wrong' WHERE id='a1'")
            .unwrap();
        assert!(
            closed_inventory(&conn).is_err(),
            "size TEXT cannot pass as numeric bytes"
        );
        conn.execute("UPDATE drive_pending_files SET size=-1 WHERE id='a1'")
            .unwrap();
        assert!(
            closed_inventory(&conn).is_err(),
            "negative size cannot be listed"
        );
        conn.execute("UPDATE drive_pending_files SET size=1,id='../escape' WHERE id='a1'")
            .unwrap();
        assert!(
            closed_inventory(&conn).is_err(),
            "foreign path-like pending IDs fail closed"
        );
    }

    #[test]
    fn pending_inventory_caps_results_without_truncation_by_numeric_cast() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE drive_pending_files(
            id TEXT PRIMARY KEY, display_name TEXT, size INTEGER, closed_at INTEGER);",
        )
        .unwrap();
        for index in 0..105 {
            let mut insert = conn
                .prepare("INSERT INTO drive_pending_files VALUES (?, 'safe', 5, 1)")
                .unwrap();
            let id = format!("id{index:04}");
            insert.bind((1, id.as_str())).unwrap();
            insert.next().unwrap();
        }
        let (rows, total) = closed_inventory(&conn).unwrap();
        assert_eq!(total, 105);
        assert_eq!(rows.len(), MAX_VISIBLE + 1);
    }
}
