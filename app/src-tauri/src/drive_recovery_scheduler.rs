//! Bounded, deterministic scheduling for native Drive writes left pending after
//! transient network/authentication failures. The journal of truth is SQLite;
//! this in-memory planner only remembers retry delays and never owns file data.
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub(super) const SCAN_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const MAX_PER_SCAN: usize = 2;
const FIRST_RETRY: Duration = Duration::from_secs(20);
const MAX_RETRY: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy)]
struct Failure {
    attempts: u32,
    next_attempt_at: Duration,
}

#[derive(Debug, Default)]
pub(super) struct DeferredWritePlanner {
    failures: HashMap<String, Failure>,
    cursor: usize,
}

impl DeferredWritePlanner {
    /// SQL supplies *closed* durable pending IDs. Active native writers and
    /// uploads are excluded by the caller's atomic ownership claim.
    /// Sorting makes fairness independent of SQLite query ordering.
    #[cfg(test)]
    pub(super) fn select(&mut self, pending: &[String], now: Duration) -> Vec<String> {
        self.select_excluding(pending, &HashSet::new(), now)
    }

    /// A manually retried or already-running upload stays in the durable
    /// inventory, so it must NOT lose its failure/backoff record just because
    /// another worker owns its live single-flight claim.
    pub(super) fn select_excluding(
        &mut self,
        pending: &[String],
        in_flight: &HashSet<String>,
        now: Duration,
    ) -> Vec<String> {
        let mut ids = pending.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let available: HashSet<_> = ids.iter().map(String::as_str).collect();
        self.failures
            .retain(|id, _| available.contains(id.as_str()));
        ids.retain(|id| !in_flight.contains(id));
        if ids.is_empty() {
            self.cursor = 0;
            return Vec::new();
        }
        let start = self.cursor % ids.len();
        let mut result = Vec::with_capacity(MAX_PER_SCAN);
        for offset in 0..ids.len() {
            let index = (start + offset) % ids.len();
            let id = &ids[index];
            if self
                .failures
                .get(id)
                .is_some_and(|failure| now < failure.next_attempt_at)
            {
                continue;
            }
            result.push(id.clone());
            if result.len() == MAX_PER_SCAN {
                self.cursor = (index + 1) % ids.len();
                return result;
            }
        }
        self.cursor = (start + 1) % ids.len();
        result
    }

    /// Surface the actual retry scheduler's delay to the UI without copying
    /// scheduling rules into another implementation.
    pub(super) fn retry_state(&self, id: &str, now: Duration) -> (u32, u64) {
        self.failures.get(id).map_or((0, 0), |failure| {
            (
                failure.attempts,
                failure
                    .next_attempt_at
                    .saturating_sub(now)
                    .as_secs()
                    .saturating_add(u64::from(
                        failure.next_attempt_at > now
                            && failure.next_attempt_at.saturating_sub(now).subsec_nanos() > 0,
                    )),
            )
        })
    }

    /// A failed *attempt* backs off; a skipped active writer is not failure.
    /// Use monotonic elapsed time rather than wall-clock timestamps to avoid
    /// clock rollback/suspend producing an indefinite wait.
    pub(super) fn complete(&mut self, id: &str, success: bool, now: Duration) {
        if success {
            self.failures.remove(id);
            return;
        }
        let attempts = self
            .failures
            .get(id)
            .map_or(1, |previous| previous.attempts.saturating_add(1));
        let shift = (attempts - 1).min(4);
        let delay = FIRST_RETRY
            .checked_mul(1u32 << shift)
            .unwrap_or(MAX_RETRY)
            .min(MAX_RETRY);
        self.failures.insert(
            id.to_owned(),
            Failure {
                attempts,
                next_attempt_at: now.saturating_add(delay),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn failures_are_retried_without_new_ui_session_and_backoff_is_bounded() {
        let mut planner = DeferredWritePlanner::default();
        let only = ids(&["upload"]);
        assert_eq!(planner.select(&only, Duration::ZERO), only);
        let mut now = Duration::ZERO;
        for delay in [20, 40, 80, 160, 300, 300] {
            planner.complete("upload", false, now);
            assert!(planner
                .select(&only, now + Duration::from_secs(delay - 1))
                .is_empty());
            now += Duration::from_secs(delay);
            assert_eq!(planner.select(&only, now), only);
        }
        planner.complete("upload", true, now);
        assert_eq!(planner.select(&only, now), only);
    }

    #[test]
    fn one_busy_or_failed_upload_does_not_starve_other_queued_writes() {
        let mut planner = DeferredWritePlanner::default();
        let queue = ids(&["d", "b", "a", "c"]);
        assert_eq!(planner.select(&queue, Duration::ZERO), ids(&["a", "b"]));
        planner.complete("a", false, Duration::ZERO);
        planner.complete("b", false, Duration::ZERO);
        assert_eq!(planner.select(&queue, Duration::ZERO), ids(&["c", "d"]));
        assert_eq!(planner.select(&queue, Duration::ZERO), ids(&["c", "d"]));
        assert_eq!(
            planner.select(&queue, Duration::from_secs(20)),
            ids(&["a", "b"])
        );
    }

    #[test]
    fn deleted_and_finalized_pending_records_leave_no_retry_state() {
        let mut planner = DeferredWritePlanner::default();
        planner.complete("deleted", false, Duration::ZERO);
        assert!(planner
            .select(&ids(&["survivor"]), Duration::ZERO)
            .contains(&"survivor".into()));
        assert_eq!(planner.failures.len(), 0);
        assert!(planner.select(&[], Duration::ZERO).is_empty());
    }

    #[test]
    fn selection_deduplicates_stale_snapshot_ids_and_limits_each_scan() {
        let mut planner = DeferredWritePlanner::default();
        assert_eq!(
            planner.select(&ids(&["c", "b", "a", "a", "b"]), Duration::ZERO),
            ids(&["a", "b"])
        );
        assert_eq!(
            planner.select(&ids(&["c", "b", "a", "a"]), Duration::ZERO),
            ids(&["c", "a"])
        );
    }
}
