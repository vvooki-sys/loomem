pub mod clustering;
pub mod dream;
pub mod graph_walk;
pub mod sap;
pub mod serendipity;
pub mod temporal;
pub mod tracking;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::storage::{Chunk, RocksDbStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssociatorConfig {
    pub enabled: bool,
    /// Number of clusters. 0 = auto (sqrt(n/2)).
    pub k_clusters: usize,
    /// Maximum number of clusters (cap for auto K).
    pub max_clusters: usize,
    /// Maximum K-means iterations.
    pub max_iterations: usize,
    /// Minimum Sₑ score to surface an association.
    pub min_serendipity: f64,
    /// Maximum associations per search response.
    pub max_associations: usize,
}

impl Default for AssociatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            k_clusters: 0,
            max_clusters: 50,
            max_iterations: 100,
            min_serendipity: 0.3,
            max_associations: 3,
        }
    }
}

// ---- ECA-29: Association noise cap / circuit breaker ----

/// Health status of the association pipeline for a stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AssociationHealth {
    /// Association pipeline is operating normally.
    Healthy,
    /// Mean Sₑ is below threshold for some days but not yet at the disable window.
    Degraded { mean_se: f64, days: usize },
    /// Associations disabled due to sustained low quality.
    Disabled { reason: String },
}

/// Check if associations should be disabled for a stream.
///
/// Scans `stats:{stream_id}:mean_se_score:` for the last 7 days (168 hours).
/// If all hourly mean Sₑ values are below 0.35 for 7 consecutive days, returns Disabled.
/// If some are below threshold, returns Degraded with the count.
/// Otherwise returns Healthy.
pub fn check_association_health(store: &RocksDbStore, stream_id: &str) -> AssociationHealth {
    let now_hour = chrono::Utc::now().timestamp() as u64 / 3600;
    let hours_in_7_days: u64 = 168;
    let min_hour = now_hour.saturating_sub(hours_in_7_days);
    let threshold = 0.35;

    let prefix = format!("stats:{}:mean_se_score:", stream_id);
    let mut values_in_window: Vec<f64> = Vec::new();

    for (key, value) in store.prefix_scan(prefix.as_bytes()) {
        let key_str = String::from_utf8_lossy(&key);
        if let Some(hour_str) = key_str.strip_prefix(&prefix) {
            if let Ok(hour) = hour_str.parse::<u64>() {
                if hour >= min_hour && hour <= now_hour {
                    let val = if value.len() == 8 {
                        f64::from_le_bytes(value[..8].try_into().unwrap_or([0u8; 8]))
                    } else {
                        String::from_utf8_lossy(&value)
                            .parse::<f64>()
                            .unwrap_or(0.0)
                    };
                    values_in_window.push(val);
                }
            }
        }
    }

    // No data means we can't judge — assume healthy
    if values_in_window.is_empty() {
        return AssociationHealth::Healthy;
    }

    let below_count = values_in_window.iter().filter(|&&v| v < threshold).count();
    let total = values_in_window.len();

    // All values below threshold for the full 7-day window
    if below_count == total && total >= 24 {
        // At least 24 hours of data, all below threshold
        let mean_se = values_in_window.iter().sum::<f64>() / total as f64;
        let days = total / 24;
        if days >= 7 {
            return AssociationHealth::Disabled {
                reason: format!(
                    "Mean Se score below {:.2} for {} consecutive days (mean={:.3})",
                    threshold, days, mean_se
                ),
            };
        }
        return AssociationHealth::Degraded { mean_se, days };
    }

    // Partial degradation
    if below_count > total / 2 {
        let mean_se = values_in_window.iter().sum::<f64>() / total as f64;
        let days = below_count / 24;
        return AssociationHealth::Degraded {
            mean_se,
            days: days.max(1),
        };
    }

    AssociationHealth::Healthy
}

/// Record the mean Sₑ score for the current hour.
/// Called after association computation to track quality over time.
pub fn record_mean_se_score(store: &RocksDbStore, stream_id: &str, mean_se: f64) {
    let now_hour = chrono::Utc::now().timestamp() as u64 / 3600;
    let key = format!("stats:{}:mean_se_score:{}", stream_id, now_hour);
    if let Err(e) = store.put(key.as_bytes(), &mean_se.to_le_bytes()) {
        warn!("Failed to record mean Se score: {}", e);
    }
}

/// Check if associations should proceed for a stream. Returns true if healthy or degraded.
/// Returns false (with warning log) if disabled.
pub fn should_run_associations(store: &RocksDbStore, stream_id: &str) -> bool {
    match check_association_health(store, stream_id) {
        AssociationHealth::Healthy => true,
        AssociationHealth::Degraded { mean_se, days } => {
            warn!(
                "Association quality degraded for stream {}: mean_se={:.3}, days={}",
                stream_id, mean_se, days
            );
            true
        }
        AssociationHealth::Disabled { reason } => {
            warn!("Associations DISABLED for stream {}: {}", stream_id, reason);
            false
        }
    }
}

/// Check if a chunk is eligible for association.
/// Filters out:
/// - Deleted chunks
/// - Non-latest (superseded) chunks
/// - Meta/operational/system chunks
/// - Raw transcripts and toolResult artifacts
/// - Chunks with very short content (<10 chars)
pub fn is_associable(chunk: &Chunk) -> bool {
    // Must be latest and not deleted
    if !chunk.is_latest || chunk.deleted_at.is_some() {
        return false;
    }

    // Filter by source — exclude operational/meta sources
    if let Some(ref source) = chunk.source {
        let s = source.agent.to_lowercase();
        if s.contains("raw-transcript")
            || s.contains("openclaw")
            || s.contains("system")
            || s.contains("toolresult")
            || s.contains("tool_result")
            || s.contains("meta")
            || s.contains("debug")
        {
            return false;
        }
    }

    // Filter by content — exclude operational artifacts
    let content_lower = chunk.content.to_lowercase();
    if content_lower.starts_with("toolresult")
        || content_lower.starts_with("tool_result")
        || content_lower.starts_with("{\"tool")
        || content_lower.starts_with("error:")
        || content_lower.starts_with("system:")
    {
        return false;
    }

    // Must have meaningful content
    if chunk.content.len() < 10 {
        return false;
    }

    true
}

/// Check if a chunk belongs to the given stream and is associable.
pub fn is_associable_in_stream(chunk: &Chunk, stream_id: &str) -> bool {
    chunk.stream == stream_id && is_associable(chunk)
}
