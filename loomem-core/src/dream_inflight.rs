//! Per-stream mutual exclusion for every dream entry point.
//!
//! `memory_dream`, `POST /v1/dream`, the count-based auto-trigger and the
//! scheduled pass all end in [`crate::dream::dream_run`]. Two of them on the
//! same stream at once would merge the same subjects twice, so `dream_run`
//! takes an [`InflightGuard`] first and returns [`DreamResult::skipped_busy`]
//! when the stream is already being dreamed by another caller.
//!
//! The set is process-wide but keyed by `(store identity, stream)`: in the
//! hosted engine several scopes, each with its own `RocksDbStore`, share one
//! process and may well use the same stream name without sharing any data.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, OnceLock};

use tracing::info;

use crate::dream::DreamResult;
use crate::storage::RocksDbStore;

/// `(store identity, stream)`. The identity is the store's address: stable
/// for as long as the store lives, and a guard never outlives the `&store`
/// it was taken with.
type Key = (usize, String);

fn key(store: &RocksDbStore, stream: &str) -> Key {
    (std::ptr::from_ref(store).addr(), stream.to_string())
}

fn inflight() -> MutexGuard<'static, HashSet<Key>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();
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
    key: Key,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        inflight().remove(&self.key);
    }
}

/// Mark `stream` of `store` as dreaming. `None` when another caller already
/// holds it.
pub fn try_acquire(store: &RocksDbStore, stream: &str) -> Option<InflightGuard> {
    let key = key(store, stream);
    let mut set = inflight();
    if set.contains(&key) {
        info!(stream, "Dream already running on this stream, skipping");
        return None;
    }
    set.insert(key.clone());
    Some(InflightGuard { key })
}

/// `true` while some caller holds the guard for `stream` of `store`.
pub fn is_inflight(store: &RocksDbStore, stream: &str) -> bool {
    inflight().contains(&key(store, stream))
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
    use crate::storage::RocksDbConfig;
    use tempfile::TempDir;

    // Each test opens its own store, so keys never collide across the
    // parallel tests of this binary even when stream names repeat.
    fn open_store(tmp: &TempDir) -> RocksDbStore {
        RocksDbStore::open(
            tmp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "none".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .expect("open tempdir store")
    }

    #[test]
    fn acquire_marks_and_drop_releases() {
        let tmp = TempDir::new().expect("tempdir");
        let store = open_store(&tmp);
        assert!(!is_inflight(&store, "s"));
        let guard = try_acquire(&store, "s").expect("first acquire succeeds");
        assert!(is_inflight(&store, "s"));
        drop(guard);
        assert!(!is_inflight(&store, "s"));
    }

    #[test]
    fn double_acquire_is_refused_until_release() {
        let tmp = TempDir::new().expect("tempdir");
        let store = open_store(&tmp);
        let first = try_acquire(&store, "s").expect("first acquire succeeds");
        assert!(
            try_acquire(&store, "s").is_none(),
            "second acquire must fail"
        );
        drop(first);
        assert!(
            try_acquire(&store, "s").is_some(),
            "acquire works again after release"
        );
    }

    #[test]
    fn streams_are_independent() {
        let tmp = TempDir::new().expect("tempdir");
        let store = open_store(&tmp);
        let _a = try_acquire(&store, "a").expect("a");
        assert!(try_acquire(&store, "b").is_some());
    }

    /// Hosted: two scopes with their own stores may share a stream name;
    /// one dreaming must not block the other.
    #[test]
    fn same_stream_on_different_stores_is_independent() {
        let (tmp_a, tmp_b) = (
            TempDir::new().expect("tempdir a"),
            TempDir::new().expect("tempdir b"),
        );
        let (store_a, store_b) = (open_store(&tmp_a), open_store(&tmp_b));
        let _a = try_acquire(&store_a, "scope-stream").expect("store a acquires");
        assert!(
            try_acquire(&store_b, "scope-stream").is_some(),
            "store b must not be blocked by store a"
        );
        assert!(is_inflight(&store_a, "scope-stream"));
        assert!(
            try_acquire(&store_a, "scope-stream").is_none(),
            "store a itself is still exclusive"
        );
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
