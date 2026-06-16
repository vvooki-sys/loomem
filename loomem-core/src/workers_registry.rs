use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::scheduler::WorkerConfig;

pub struct WorkerStats {
    pub paused: AtomicBool,
    pub last_run_at: AtomicU64,           // epoch seconds, 0 = never run
    pub last_success_at: AtomicU64,       // epoch seconds, 0 = never succeeded
    pub items_processed_total: AtomicU64, // monotonic counter
    pub interval_secs: u64,               // immutable in v1
}

impl WorkerStats {
    fn new(interval_secs: u64) -> Self {
        Self {
            paused: AtomicBool::new(false),
            last_run_at: AtomicU64::new(0),
            last_success_at: AtomicU64::new(0),
            items_processed_total: AtomicU64::new(0),
            interval_secs,
        }
    }
}

pub type WorkerRegistry = Arc<HashMap<&'static str, WorkerStats>>;

pub const KNOWN_WORKERS: &[&str] = &[
    "consolidation",
    "decay",
    "compaction",
    "backup",
    "clustering",
    "purge",
    "stats",
];

/// Intervals for workers whose config does not live directly in `WorkerConfig`.
/// `retention_interval_secs` is for purge, `stats_interval_secs` is fixed.
pub fn build_registry(
    config: &WorkerConfig,
    retention_interval_secs: u64,
    stats_interval_secs: u64,
) -> WorkerRegistry {
    let mut map: HashMap<&'static str, WorkerStats> = HashMap::new();
    map.insert(
        "consolidation",
        WorkerStats::new(config.consolidation.interval_secs),
    );
    map.insert("decay", WorkerStats::new(config.decay_worker.interval_secs));
    map.insert(
        "compaction",
        WorkerStats::new(config.compaction.interval_secs),
    );
    map.insert("backup", WorkerStats::new(config.backup.interval_secs));
    map.insert(
        "clustering",
        WorkerStats::new(config.clustering.interval_secs),
    );
    map.insert("purge", WorkerStats::new(retention_interval_secs));
    map.insert("stats", WorkerStats::new(stats_interval_secs));
    Arc::new(map)
}

/// Returns the current Unix epoch seconds.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Checks whether the named worker is paused.
/// Returns `false` if the name is not in the registry (unknown workers run by default).
pub fn is_worker_paused(registry: &WorkerRegistry, name: &str) -> bool {
    registry
        .get(name)
        .is_some_and(|w| w.paused.load(Ordering::SeqCst))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> WorkerConfig {
        WorkerConfig::default()
    }

    #[test]
    fn build_registry_has_all_known_workers() {
        let cfg = test_config();
        let reg = build_registry(&cfg, 86400, 3600);
        for name in KNOWN_WORKERS {
            assert!(reg.get(*name).is_some(), "missing worker: {}", name);
        }
        assert_eq!(reg.len(), KNOWN_WORKERS.len());
    }

    #[test]
    fn build_registry_defaults_all_fields_zero_or_false() {
        let cfg = test_config();
        let reg = build_registry(&cfg, 86400, 3600);
        for name in KNOWN_WORKERS {
            let w = reg.get(*name).unwrap();
            assert!(
                !w.paused.load(Ordering::SeqCst),
                "worker {name} should default paused=false"
            );
            assert_eq!(
                w.last_run_at.load(Ordering::SeqCst),
                0,
                "worker {name} last_run_at should default to 0"
            );
            assert_eq!(
                w.last_success_at.load(Ordering::SeqCst),
                0,
                "worker {name} last_success_at should default to 0"
            );
            assert_eq!(
                w.items_processed_total.load(Ordering::SeqCst),
                0,
                "worker {name} items_processed_total should default to 0"
            );
        }
    }

    #[test]
    fn build_registry_interval_matches_config() {
        let cfg = test_config();
        let reg = build_registry(&cfg, 86400, 3600);

        assert_eq!(
            reg.get("consolidation").unwrap().interval_secs,
            cfg.consolidation.interval_secs
        );
        assert_eq!(
            reg.get("decay").unwrap().interval_secs,
            cfg.decay_worker.interval_secs
        );
        assert_eq!(
            reg.get("compaction").unwrap().interval_secs,
            cfg.compaction.interval_secs
        );
        assert_eq!(
            reg.get("backup").unwrap().interval_secs,
            cfg.backup.interval_secs
        );
        assert_eq!(
            reg.get("clustering").unwrap().interval_secs,
            cfg.clustering.interval_secs
        );
        assert_eq!(reg.get("purge").unwrap().interval_secs, 86400);
        assert_eq!(reg.get("stats").unwrap().interval_secs, 3600);
    }

    #[test]
    fn is_worker_paused_returns_correct_state() {
        let cfg = test_config();
        let reg = build_registry(&cfg, 86400, 3600);

        assert!(!is_worker_paused(&reg, "consolidation"));

        reg.get("consolidation")
            .unwrap()
            .paused
            .store(true, Ordering::SeqCst);
        assert!(is_worker_paused(&reg, "consolidation"));

        // unknown name returns false (not paused)
        assert!(!is_worker_paused(&reg, "unknown_worker"));
    }
}
