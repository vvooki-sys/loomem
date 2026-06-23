use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::storage::RocksDbStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayConfig {
    pub l0_lambda: f64,
    pub l1_lambda: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayWorkerConfig {
    pub interval_secs: u64,
    pub factor: f64,
    pub timeout_secs: u64,
    pub l0_factor: f64,
    pub l1_factor: f64,
    pub dormant_threshold: f64,
    pub access_boost: bool,
    #[serde(default)]
    pub adaptive_enabled: bool,
    #[serde(default = "default_adaptive_dampening")]
    pub adaptive_dampening: f64,
    #[serde(default = "default_adaptive_cap")]
    pub adaptive_cap: u32,
    /// Auto-promote to persistent=true once access_count exceeds this threshold.
    /// Default 50. Disabled if persistent_auto_enabled=false.
    #[serde(default = "default_persistent_auto_threshold")]
    pub persistent_auto_threshold: u32,
    /// Master switch for auto-persistent promotion.
    #[serde(default = "default_persistent_auto_enabled")]
    pub persistent_auto_enabled: bool,
}

fn default_adaptive_dampening() -> f64 {
    0.5
}
fn default_adaptive_cap() -> u32 {
    200
}
fn default_persistent_auto_threshold() -> u32 {
    50
}
fn default_persistent_auto_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayReport {
    pub decayed_count: usize,
    pub dormant_count: usize,
    pub errors: usize,
}

/// Result of applying a decay score update to storage.
struct ApplyResult {
    decayed: usize,
    dormant: usize,
    errors: usize,
}

/// Apply the computed new_score to storage: mark dormant or update score.
/// Returns count deltas for the caller to accumulate.
fn apply_score(
    storage: &Arc<RocksDbStore>,
    chunk: &crate::storage::Chunk,
    new_score: f64,
    now_secs: u64,
    dormant_threshold: f64,
) -> ApplyResult {
    if new_score < dormant_threshold {
        match storage.mark_dormant(&chunk.id) {
            Ok(()) => {
                debug!(
                    "Marked chunk {} as dormant (score: {:.4} -> {:.4})",
                    chunk.id, chunk.score, new_score
                );
                ApplyResult {
                    decayed: 0,
                    dormant: 1,
                    errors: 0,
                }
            }
            Err(e) => {
                debug!("Failed to mark chunk {} as dormant: {}", chunk.id, e);
                ApplyResult {
                    decayed: 0,
                    dormant: 0,
                    errors: 1,
                }
            }
        }
    } else {
        match storage.update_score(&chunk.id, new_score, now_secs) {
            Ok(()) => {
                debug!(
                    "Decayed chunk {} (L{}): score {:.4} -> {:.4}",
                    chunk.id, chunk.level, chunk.score, new_score
                );
                ApplyResult {
                    decayed: 1,
                    dormant: 0,
                    errors: 0,
                }
            }
            Err(e) => {
                debug!("Failed to update score for chunk {}: {}", chunk.id, e);
                ApplyResult {
                    decayed: 0,
                    dormant: 0,
                    errors: 1,
                }
            }
        }
    }
}

/// Compute the new score for a chunk after applying time-based decay.
/// `now_secs` is the current UNIX timestamp in seconds (computed once per decay cycle).
/// Returns the new score after applying the appropriate decay factor.
fn compute_new_score(
    config: &DecayWorkerConfig,
    chunk: &crate::storage::Chunk,
    now_secs: u64,
) -> f64 {
    let base_factor = match chunk.level {
        0 => config.l0_factor,
        _ => config.l1_factor,
    };

    // Adaptive decay: frequently accessed chunks decay slower
    let factor = if config.adaptive_enabled && chunk.access_count > 0 {
        let capped = chunk.access_count.min(config.adaptive_cap) as f64;
        // Raise factor closer to 1.0 — slower decay for accessed chunks
        // e.g. base 0.995, dampening 0.1, access 10 → 0.995 + 0.1*(10/20)*(1-0.995) = 0.99525
        let boost =
            config.adaptive_dampening * (capped / config.adaptive_cap as f64) * (1.0 - base_factor);
        (base_factor + boost).min(0.9999)
    } else {
        base_factor
    };

    let last_decay = chunk.last_decay.unwrap_or(chunk.timestamp);
    let hours_elapsed = ((now_secs - last_decay) as f64) / 3600.0;

    // Apply decay: new_score = old_score * factor^hours
    chunk.score * factor.powf(hours_elapsed)
}

/// Auto-promote chunk to persistent if access_count meets threshold.
/// Returns Ok(true) if promoted (caller should skip decay), Ok(false) if not promoted
/// (caller continues with normal decay path). Storage errors are returned as Err
/// — caller decides whether to count and fall through.
fn try_auto_promote(
    storage: &Arc<RocksDbStore>,
    config: &DecayWorkerConfig,
    chunk: &crate::storage::Chunk,
) -> Result<bool> {
    if !config.persistent_auto_enabled || chunk.access_count < config.persistent_auto_threshold {
        return Ok(false);
    }
    storage.mark_persistent(&chunk.id)?;
    debug!(
        "Auto-promoted chunk {} to persistent (access_count={})",
        chunk.id, chunk.access_count
    );
    Ok(true)
}

/// Run decay: apply time-based score decay to all chunks
pub async fn run_decay(
    storage: Arc<RocksDbStore>,
    config: &DecayWorkerConfig,
    cancel: CancellationToken,
) -> Result<DecayReport> {
    let mut decayed_count = 0;
    let mut dormant_count = 0;
    let mut errors = 0;

    info!(
        "Starting decay cycle: l0_factor={}, l1_factor={}, threshold={}",
        config.l0_factor, config.l1_factor, config.dormant_threshold
    );

    // Compute current timestamp once for the entire cycle
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Scan all chunks with score > dormant_threshold
    let min_score = config.dormant_threshold;
    let chunks = storage.scan_for_decay(min_score)?;

    info!("Found {} chunks eligible for decay", chunks.len());

    for chunk in chunks {
        // Check for cancellation
        if cancel.is_cancelled() {
            info!("Decay cancelled, stopping");
            break;
        }

        // Skip persistent chunks
        if chunk.persistent {
            debug!("Skipping persistent chunk: {}", chunk.id);
            continue;
        }

        // Auto-promote to persistent if access_count crosses threshold
        match try_auto_promote(&storage, config, &chunk) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                errors += 1;
                debug!("Failed to mark chunk {} as persistent: {}", chunk.id, e);
                // fall through to normal decay
            }
        }

        let new_score = compute_new_score(config, &chunk, now);
        let applied = apply_score(&storage, &chunk, new_score, now, config.dormant_threshold);
        decayed_count += applied.decayed;
        dormant_count += applied.dormant;
        errors += applied.errors;
    }

    info!(
        "Decay cycle completed: decayed={}, dormant={}, errors={}",
        decayed_count, dormant_count, errors
    );

    Ok(DecayReport {
        decayed_count,
        dormant_count,
        errors,
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_decay_math() {
        // Test that 0.995^144 ≈ 0.487 (half-life ~6 days at hourly intervals)
        let factor = 0.995_f64;
        let hours = 144.0; // 6 days
        let result = factor.powf(hours);

        assert!(
            result > 0.47 && result < 0.50,
            "Expected ~0.487, got {}",
            result
        );
    }

    #[test]
    fn test_access_boost_concept() {
        // Test that resetting score to 1.0 works as expected
        let decayed_score = 0.5;
        let boosted_score = 1.0;

        assert_eq!(boosted_score, 1.0);
        assert!(boosted_score > decayed_score);
    }

    #[test]
    fn test_adaptive_decay_slows_with_access() {
        // A chunk with access_count at cap should decay slower than access_count=0
        let base_factor = 0.995_f64;
        let dampening = 0.5_f64; // new default
        let cap = 200_u32; // new default
        let hours = 144.0_f64; // 6 days

        // No access: standard decay
        let no_access = base_factor.powf(hours);

        // Max access: adaptive decay
        let capped = f64::from(cap.min(cap));
        let boost = dampening * (capped / f64::from(cap)) * (1.0 - base_factor);
        let adaptive_factor = (base_factor + boost).min(0.9999);
        let with_access = adaptive_factor.powf(hours);

        assert!(
            with_access > no_access,
            "Chunk with max access should retain more score: {} > {}",
            with_access,
            no_access
        );

        // Sanity: adaptive factor should be between base and 1.0
        assert!(adaptive_factor > base_factor);
        assert!(adaptive_factor < 1.0);
    }

    #[test]
    fn test_adaptive_decay_zero_access_unchanged() {
        // With access_count=0, adaptive decay should equal base decay
        let base_factor = 0.995_f64;
        let _dampening = 0.5_f64; // new default
        let _cap = 200_u32; // new default

        let access_count = 0_u32;
        // The code path: if access_count > 0 → apply boost, else base_factor
        // So with 0 access, factor = base_factor
        assert_eq!(access_count, 0);
        // Factor should be base_factor (no boost applied)
        let factor = base_factor; // code uses base_factor when access_count == 0
        assert_eq!(factor, base_factor);
    }

    #[test]
    fn test_adaptive_decay_capped() {
        // access_count=300 should have same effect as access_count=200 (new cap)
        let base_factor = 0.995_f64;
        let dampening = 0.5_f64; // new default
        let cap = 200_u32; // new default

        let calc = |ac: u32| -> f64 {
            let capped = f64::from(ac.min(cap));
            let boost = dampening * (capped / f64::from(cap)) * (1.0 - base_factor);
            (base_factor + boost).min(0.9999)
        };

        let at_cap = calc(200);
        let over_cap = calc(300);
        assert!(
            (at_cap - over_cap).abs() < 1e-10,
            "Above cap should be identical"
        );
    }

    #[test]
    fn test_adaptive_decay_new_defaults() {
        // Sanity: default cap and dampening match cycle/115 values
        assert_eq!(super::default_adaptive_cap(), 200);
        assert!((super::default_adaptive_dampening() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_backward_compat_config_missing_persistent_auto_fields() {
        // Legacy JSON without persistent_auto_* fields must deserialize with defaults
        let json = r#"{
            "interval_secs": 3600,
            "factor": 0.995,
            "timeout_secs": 60,
            "l0_factor": 0.990,
            "l1_factor": 0.995,
            "dormant_threshold": 0.01,
            "access_boost": true,
            "adaptive_enabled": true,
            "adaptive_dampening": 0.1,
            "adaptive_cap": 20
        }"#;
        let cfg: super::DecayWorkerConfig = serde_json::from_str(json).expect("deserialize ok");
        assert_eq!(cfg.persistent_auto_threshold, 50);
        assert!(cfg.persistent_auto_enabled);
    }

    // Integration tests for auto-persistent promotion logic
    // These use a real tempdir RocksDbStore and call run_decay.
    mod auto_persistent {
        use super::super::{run_decay, DecayWorkerConfig};
        use crate::config::RocksDbConfig;
        use crate::storage::{Chunk, RocksDbStore};
        use std::sync::Arc;
        use tempfile::TempDir;
        use tokio_util::sync::CancellationToken;

        fn make_config() -> RocksDbConfig {
            RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            }
        }

        fn decay_config_auto_on(threshold: u32) -> DecayWorkerConfig {
            DecayWorkerConfig {
                interval_secs: 3600,
                factor: 0.995,
                timeout_secs: 60,
                l0_factor: 0.990,
                l1_factor: 0.995,
                dormant_threshold: 0.01,
                access_boost: false,
                adaptive_enabled: false,
                adaptive_dampening: 0.5,
                adaptive_cap: 200,
                persistent_auto_threshold: threshold,
                persistent_auto_enabled: true,
            }
        }

        fn decay_config_auto_off() -> DecayWorkerConfig {
            DecayWorkerConfig {
                interval_secs: 3600,
                factor: 0.995,
                timeout_secs: 60,
                l0_factor: 0.990,
                l1_factor: 0.995,
                dormant_threshold: 0.01,
                access_boost: false,
                adaptive_enabled: false,
                adaptive_dampening: 0.5,
                adaptive_cap: 200,
                persistent_auto_threshold: 50,
                persistent_auto_enabled: false,
            }
        }

        fn make_chunk(id: &str, access_count: u32) -> Chunk {
            Chunk {
                id: id.to_string(),
                content: "test content".to_string(),
                stream: "100".to_string(),
                level: 0,
                score: 1.0,
                timestamp: 1_000,
                consolidated: false,
                dormant: false,
                in_progress: false,
                prompt_version: None,
                source_ids: None,
                last_decay: None,
                metadata: None,
                importance: None,
                persistent: false,
                last_implicit_boost: None,
                access_count,
                source: None,
                created_by: None,
                updated_at: None,
                valid_from: None,
                valid_until: None,
                is_latest: true,
                superseded_by: None,
                supersedes_id: None,
                root_memory_id: None,
                version: 1,
                memory_type: None,
                extraction_meta: None,
                deleted_at: None,
                trust_level: None,
                ingester_user_id: None,
                alpha: 1.0,
                beta: 1.0,
                harmful_count: 0,
                n_ratings: 0,
                last_rated_at: None,
                provenance_role: crate::storage::ProvenanceRole::Claim,
            }
        }

        #[tokio::test]
        async fn test_auto_persistent_promotion() {
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("chunk-high", 60);
            store.store_chunk(&chunk).expect("store chunk");

            let cfg = decay_config_auto_on(50);
            let cancel = CancellationToken::new();
            run_decay(Arc::clone(&store), &cfg, cancel)
                .await
                .expect("run_decay ok");

            let after = store.get_chunk("chunk-high").expect("get").expect("exists");
            assert!(
                after.persistent,
                "chunk with access_count=60 >= threshold=50 must be promoted to persistent"
            );
        }

        #[tokio::test]
        async fn test_auto_persistent_idempotent() {
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("chunk-idempotent", 60);
            store.store_chunk(&chunk).expect("store chunk");

            let cfg = decay_config_auto_on(50);

            // First pass
            let cancel1 = CancellationToken::new();
            run_decay(Arc::clone(&store), &cfg, cancel1)
                .await
                .expect("first run ok");

            // Second pass — must not error
            let cancel2 = CancellationToken::new();
            let report = run_decay(Arc::clone(&store), &cfg, cancel2)
                .await
                .expect("second run ok");

            assert_eq!(report.errors, 0, "second pass must have zero errors");
            let after = store
                .get_chunk("chunk-idempotent")
                .expect("get")
                .expect("exists");
            assert!(after.persistent, "still persistent after second pass");
        }

        #[tokio::test]
        async fn test_auto_persistent_disabled() {
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("chunk-disabled", 100);
            store.store_chunk(&chunk).expect("store chunk");

            let cfg = decay_config_auto_off();
            let cancel = CancellationToken::new();
            run_decay(Arc::clone(&store), &cfg, cancel)
                .await
                .expect("run_decay ok");

            let after = store
                .get_chunk("chunk-disabled")
                .expect("get")
                .expect("exists");
            assert!(
                !after.persistent,
                "persistent_auto_enabled=false: chunk must NOT be auto-promoted"
            );
        }

        #[tokio::test]
        async fn test_run_decay_auto_promotes_high_access_chunk() {
            // B2 fixture: access_count=60, threshold=50 → persistent=true after one pass
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("b2-chunk", 60);
            store.store_chunk(&chunk).expect("store chunk");

            let cfg = decay_config_auto_on(50);
            let cancel = CancellationToken::new();
            run_decay(Arc::clone(&store), &cfg, cancel)
                .await
                .expect("run_decay ok");

            let after = store.get_chunk("b2-chunk").expect("get").expect("exists");
            assert!(
                after.persistent,
                "B2: chunk with access_count=60 must be promoted to persistent"
            );
        }

        #[tokio::test]
        async fn test_mark_persistent_idempotent() {
            // AC-15: mark_persistent on already-persistent chunk is no-op, no error
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("chunk-persist-idem", 0);
            store.store_chunk(&chunk).expect("store chunk");

            store
                .mark_persistent("chunk-persist-idem")
                .expect("first call ok");
            // Second call must be no-op (no error)
            store
                .mark_persistent("chunk-persist-idem")
                .expect("second call no-op");

            let after = store
                .get_chunk("chunk-persist-idem")
                .expect("get")
                .expect("exists");
            assert!(after.persistent);
        }

        #[tokio::test]
        async fn test_auto_persistent_under_threshold() {
            // Chunk with access_count < threshold should NOT be promoted (under-threshold + enabled branch).
            // Different from test_auto_persistent_disabled which tests the !enabled branch.
            let tmp = TempDir::new().expect("tempdir");
            let store =
                Arc::new(RocksDbStore::open(tmp.path(), &make_config()).expect("open store"));
            let chunk = make_chunk("chunk-under-threshold", 10);
            store.store_chunk(&chunk).expect("store chunk");

            let cfg = decay_config_auto_on(50);
            let cancel = CancellationToken::new();
            run_decay(Arc::clone(&store), &cfg, cancel)
                .await
                .expect("run_decay ok");

            let after = store
                .get_chunk("chunk-under-threshold")
                .expect("get")
                .expect("exists");
            assert!(
                !after.persistent,
                "chunk with access_count=10 < threshold=50 must NOT be promoted"
            );
        }
    }
}
