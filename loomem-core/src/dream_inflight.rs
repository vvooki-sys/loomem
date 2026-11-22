//! Per-stream mutual exclusion for every dream entry point.
//!
//! `memory_dream`, `POST /v1/dream`, the count-based auto-trigger and the
//! scheduled pass all end in [`crate::dream::dream_run`]. Two of them on the
//! same stream at once would merge the same subjects twice, so `dream_run`
//! takes an [`InflightGuard`] first and returns [`DreamResult::skipped_busy`]
//! when the stream is already being dreamed by another caller.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, OnceLock};

use tracing::info;

use crate::dream::DreamResult;

fn inflight() -> MutexGuard<'static, HashSet<String>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    // Poisoning is non-fatal: the set only holds stream ids and a panicking
    // holder's guard has already removed its entry (or never inserted one),
    // so recover the inner value instead of panicking (CLAUDE.md: no
    // unwrap/expect in production paths).
    INFLIGHT
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII token: the stream is marked busy until this is dropped.
#[derive(Debug)]
pub struct InflightGuard {
    stream: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        inflight().remove(&self.stream);
    }
}

/// Mark `stream` as dreaming. `None` when another caller already holds it.
pub fn try_acquire(stream: &str) -> Option<InflightGuard> {
    let mut set = inflight();
    if set.contains(stream) {
        info!(stream, "Dream already running on this stream, skipping");
        return None;
    }
    set.insert(stream.to_string());
    Some(InflightGuard {
        stream: stream.to_string(),
    })
}

/// `true` while some caller holds the guard for `stream`.
pub fn is_inflight(stream: &str) -> bool {
    inflight().contains(stream)
}

impl DreamResult {
    /// Zeroed result for a stream another dream entry point is processing.
    pub fn skipped_busy(stream: &str) -> Self {
        Self {
            stream: stream.to_string(),
            chunks_processed: 0,
            groups_found: 0,
            facts_merged: 0,
            contradictions_resolved: 0,
            cost_usd: 0.0,
            duration_ms: 0,
            cost_cap_reached: false,
            skipped_busy: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stream names are unique per test: the set is process-wide and the
    // test binary runs tests in parallel.

    #[test]
    fn acquire_marks_and_drop_releases() {
        let stream = "inflight-test-acquire-release";
        assert!(!is_inflight(stream));
        let guard = try_acquire(stream).expect("first acquire succeeds");
        assert!(is_inflight(stream));
        drop(guard);
        assert!(!is_inflight(stream));
    }

    #[test]
    fn double_acquire_is_refused_until_release() {
        let stream = "inflight-test-double";
        let first = try_acquire(stream).expect("first acquire succeeds");
        assert!(try_acquire(stream).is_none(), "second acquire must fail");
        drop(first);
        assert!(
            try_acquire(stream).is_some(),
            "acquire works again after release"
        );
    }

    #[test]
    fn streams_are_independent() {
        let _a = try_acquire("inflight-test-independent-a").expect("a");
        assert!(try_acquire("inflight-test-independent-b").is_some());
    }

    #[test]
    fn skipped_busy_result_is_zeroed_and_flagged() {
        let r = DreamResult::skipped_busy("s");
        assert_eq!(r.stream, "s");
        assert!(r.skipped_busy);
        assert_eq!(r.chunks_processed, 0);
        assert_eq!(r.facts_merged, 0);
        assert_eq!(r.cost_usd, 0.0);
        assert!(!r.cost_cap_reached);
    }
}
