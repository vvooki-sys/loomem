use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::event_log::{EventEntry, MemoryEvent};
use crate::storage::{Chunk, RocksDbStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorConfig {
    pub enabled: bool,
    /// Minimum number of similar queries to flag as repeated (default 3).
    pub repeated_query_threshold: usize,
    /// Number of days before a fact is considered stale (default 14).
    pub stale_fact_days: u64,
    /// Maximum advisories to return per detection run (default 10).
    pub max_advisories: usize,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repeated_query_threshold: 3,
            stale_fact_days: 14,
            max_advisories: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisoryItem {
    pub id: String,
    pub advisory_type: AdvisoryType,
    pub message: String,
    pub suggested_action: Option<String>,
    pub affected_chunk_ids: Vec<String>,
    pub priority: AdvisoryPriority,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AdvisoryType {
    RepeatedQuery,
    SearchIgnored,
    StaleFact,
    Contradiction,
    GapFilled,
    HealthCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdvisoryPriority {
    Low,
    Medium,
    High,
}

// ---- ECA-08: Repeated queries detector ----

/// Normalize a query for similarity grouping: lowercase, trim, sort words.
fn normalize_query(q: &str) -> String {
    let lower = q.to_lowercase();
    lower.trim().to_string()
}

/// Check if two queries share >60% of words.
fn queries_similar(a: &str, b: &str) -> bool {
    let a_words: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let b_words: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if a_words.is_empty() || b_words.is_empty() {
        return false;
    }
    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    if union == 0 {
        return false;
    }
    (intersection as f64 / union as f64) > 0.6
}

/// Group queries by approximate similarity, return groups with 3+ members.
fn detect_repeated_queries(
    search_events: &[(u64, String)],
    threshold: usize,
) -> Vec<(String, usize)> {
    // Group by normalized form
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();

    for (_ts, query) in search_events {
        let norm = normalize_query(query);
        if norm.is_empty() {
            continue;
        }
        let mut found = false;
        for (rep, members) in &mut groups {
            if queries_similar(rep, &norm) {
                members.push(norm.clone());
                found = true;
                break;
            }
        }
        if !found {
            groups.push((norm.clone(), vec![norm]));
        }
    }

    groups
        .into_iter()
        .filter(|(_, members)| members.len() >= threshold)
        .map(|(rep, members)| (rep, members.len()))
        .collect()
}

// ---- ECA-09: Search-then-ignore detector ----

struct SearchEvent {
    timestamp: u64,
    query: String,
    top_score: f32,
    result_count: usize,
}

struct StoreEvent {
    timestamp: u64,
}

fn detect_search_ignore(
    searches: &[SearchEvent],
    stores: &[StoreEvent],
    feedback_chunk_ids: &std::collections::HashSet<String>,
) -> Vec<AdvisoryItem> {
    let now = chrono::Utc::now().timestamp() as u64;
    let mut advisories = Vec::new();

    // Search with top-1 score > 0.5 but no feedback → SearchIgnored
    let ignored_count = searches
        .iter()
        .filter(|s| s.top_score > 0.5 && s.result_count > 0)
        .count();
    // If >50% of good searches have no feedback at all, flag it
    let total_good = searches
        .iter()
        .filter(|s| s.top_score > 0.5 && s.result_count > 0)
        .count();
    if total_good > 3 && feedback_chunk_ids.is_empty() {
        advisories.push(AdvisoryItem {
            id: format!("adv-search-ignored-{}", now),
            advisory_type: AdvisoryType::SearchIgnored,
            message: format!(
                "{} searches returned good results (score > 0.5) but no feedback was recorded. Consider providing feedback to improve ranking.",
                ignored_count,
            ),
            suggested_action: Some("Use the /v1/stats/feedback endpoint to mark useful results".to_string()),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        });
    }

    // Store-after-miss: store within 120s of zero-result search → GapFilled
    let mut gap_filled_count = 0usize;
    for search in searches.iter().filter(|s| s.result_count == 0) {
        let has_store_after = stores
            .iter()
            .any(|st| st.timestamp > search.timestamp && st.timestamp - search.timestamp <= 120);
        if has_store_after {
            gap_filled_count += 1;
        }
    }
    if gap_filled_count > 0 {
        advisories.push(AdvisoryItem {
            id: format!("adv-gap-filled-{}", now),
            advisory_type: AdvisoryType::GapFilled,
            message: format!(
                "{} zero-result searches were followed by a store within 120s, suggesting knowledge gaps being filled reactively.",
                gap_filled_count,
            ),
            suggested_action: Some("Consider proactively storing knowledge in anticipated areas".to_string()),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Low,
            created_at: now,
        });
    }

    advisories
}

// ---- ECA-10: Stale facts detector ----

fn detect_stale_facts(store: &RocksDbStore, stream_id: &str, stale_days: u64) -> Vec<AdvisoryItem> {
    let now = chrono::Utc::now().timestamp() as u64;
    let stale_threshold = now.saturating_sub(stale_days * 86400);
    let mut advisories = Vec::new();

    // Collect all non-deleted chunks for this stream
    let mut chunks: Vec<Chunk> = Vec::new();
    for prefix in &[b"chunk:L0:" as &[u8], b"chunk:L1:"] {
        for (_, value) in store.prefix_scan(prefix) {
            if let Ok(chunk) = store.decode_chunk(&value) {
                if chunk.deleted_at.is_none() && chunk.stream == stream_id {
                    chunks.push(chunk);
                }
            }
        }
    }

    if chunks.is_empty() {
        return advisories;
    }

    // Group chunks by subject (from extraction_meta)
    let mut by_subject: HashMap<String, Vec<&Chunk>> = HashMap::new();
    for chunk in &chunks {
        if let Some(ref meta) = chunk.extraction_meta {
            if let Some(ref subject) = meta.subject {
                by_subject
                    .entry(subject.to_lowercase())
                    .or_default()
                    .push(chunk);
            }
        }
    }

    // Stale facts: chunk not updated in stale_days but entity has newer chunks
    let mut stale_ids: Vec<String> = Vec::new();
    for subject_chunks in by_subject.values() {
        if subject_chunks.len() < 2 {
            continue;
        }
        let newest_ts = subject_chunks
            .iter()
            .map(|c| c.timestamp)
            .max()
            .unwrap_or(0);
        for chunk in subject_chunks {
            let updated = chunk.updated_at.unwrap_or(chunk.timestamp);
            if updated < stale_threshold && newest_ts > updated {
                stale_ids.push(chunk.id.clone());
            }
        }
    }

    if !stale_ids.is_empty() {
        let count = stale_ids.len();
        advisories.push(AdvisoryItem {
            id: format!("adv-stale-{}", now),
            advisory_type: AdvisoryType::StaleFact,
            message: format!(
                "{} chunks have not been updated in {}+ days but their subject has newer information.",
                count, stale_days,
            ),
            suggested_action: Some("Review stale chunks and consider updating or archiving them".to_string()),
            affected_chunk_ids: stale_ids.into_iter().take(20).collect(),
            priority: AdvisoryPriority::Medium,
            created_at: now,
        });
    }

    // Contradiction detection: two chunks with same subject but divergent content
    for (subject, subject_chunks) in &by_subject {
        if subject_chunks.len() < 2 {
            continue;
        }
        // Only check latest chunks (is_latest = true)
        let latest: Vec<&&Chunk> = subject_chunks.iter().filter(|c| c.is_latest).collect();
        if latest.len() >= 2 {
            // Simple heuristic: if two latest chunks exist for same subject, flag
            let ids: Vec<String> = latest.iter().take(5).map(|c| c.id.clone()).collect();
            advisories.push(AdvisoryItem {
                id: format!("adv-contradiction-{}-{}", subject, now),
                advisory_type: AdvisoryType::Contradiction,
                message: format!(
                    "Multiple 'latest' chunks exist for subject '{}'. This may indicate a contradiction or duplicate.",
                    subject,
                ),
                suggested_action: Some("Review and reconcile these chunks, or use memory_dream to consolidate".to_string()),
                affected_chunk_ids: ids,
                priority: AdvisoryPriority::High,
                created_at: now,
            });
        }
    }

    advisories
}

// ---- Main detection function ----

/// Read event log JSONL files and return parsed search/store events for a stream.
fn read_events(events_dir: &Path, stream_id: &str) -> (Vec<SearchEvent>, Vec<StoreEvent>) {
    let mut searches = Vec::new();
    let mut stores_list = Vec::new();

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if events_dir.exists() {
        if let Ok(entries) = fs::read_dir(events_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    files.push(path);
                }
            }
        }
    }

    for file_path in &files {
        let file = match fs::File::open(file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            let entry: EventEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match &entry.event {
                MemoryEvent::Search {
                    stream_id: sid,
                    top_scores,
                    result_count,
                    query,
                    ..
                } if sid == stream_id => {
                    searches.push(SearchEvent {
                        timestamp: entry.timestamp,
                        query: query.clone(),
                        top_score: top_scores.first().copied().unwrap_or(0.0),
                        result_count: *result_count,
                    });
                }
                MemoryEvent::Store { stream_id: sid, .. } if sid == stream_id => {
                    stores_list.push(StoreEvent {
                        timestamp: entry.timestamp,
                    });
                }
                _ => {}
            }
        }
    }

    (searches, stores_list)
}

/// Run all pattern detectors and return advisories.
/// Stores results in RocksDB under `advisory:{stream_id}:{id}`.
pub fn detect_advisories(
    store: &RocksDbStore,
    events_dir: &Path,
    stream_id: &str,
) -> Result<Vec<AdvisoryItem>> {
    detect_advisories_with_config(store, events_dir, stream_id, 3, 14, 10)
}

/// Run all pattern detectors with configurable thresholds.
pub fn detect_advisories_with_config(
    store: &RocksDbStore,
    events_dir: &Path,
    stream_id: &str,
    repeated_query_threshold: usize,
    stale_fact_days: u64,
    max_advisories: usize,
) -> Result<Vec<AdvisoryItem>> {
    let mut all_advisories: Vec<AdvisoryItem> = Vec::new();
    let now = chrono::Utc::now().timestamp() as u64;

    // Read events
    let (searches, stores_list) = read_events(events_dir, stream_id);

    // ECA-08: Repeated queries
    let search_tuples: Vec<(u64, String)> = searches
        .iter()
        .map(|s| (s.timestamp, s.query.clone()))
        .collect();
    let repeated = detect_repeated_queries(&search_tuples, repeated_query_threshold);
    for (query, count) in &repeated {
        all_advisories.push(AdvisoryItem {
            id: format!("adv-repeated-{}", now),
            advisory_type: AdvisoryType::RepeatedQuery,
            message: format!(
                "Query '{}' has been searched {} times. Consider storing a pre-computed answer.",
                query, count,
            ),
            suggested_action: Some(
                "Store a dedicated chunk answering this frequently-asked query".to_string(),
            ),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        });
    }

    // ECA-09: Search-then-ignore
    let feedback_ids = get_feedback_chunk_ids(store, stream_id);
    let ignore_advisories = detect_search_ignore(&searches, &stores_list, &feedback_ids);
    all_advisories.extend(ignore_advisories);

    // ECA-10: Stale facts + contradictions
    let stale_advisories = detect_stale_facts(store, stream_id, stale_fact_days);
    all_advisories.extend(stale_advisories);

    // Sort by priority (High first)
    all_advisories.sort_by(|a, b| b.priority.cmp(&a.priority));
    all_advisories.truncate(max_advisories);

    // Store advisories in RocksDB
    for advisory in &all_advisories {
        let key = format!("advisory:{}:{}", stream_id, advisory.id);
        if let Ok(json) = serde_json::to_vec(advisory) {
            if let Err(e) = store.put(key.as_bytes(), &json) {
                warn!("Failed to store advisory {}: {}", advisory.id, e);
            }
        }
    }

    info!(
        "Advisory detection for stream {}: {} advisories generated",
        stream_id,
        all_advisories.len()
    );

    Ok(all_advisories)
}

/// Get cached advisories from RocksDB (lightweight, no event scanning).
pub fn get_cached_advisories(
    store: &RocksDbStore,
    stream_id: &str,
    limit: usize,
) -> Vec<AdvisoryItem> {
    let prefix = format!("advisory:{}:", stream_id);
    let mut advisories: Vec<AdvisoryItem> = Vec::new();

    for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
        if let Ok(advisory) = serde_json::from_slice::<AdvisoryItem>(&value) {
            advisories.push(advisory);
        }
        if advisories.len() >= limit * 2 {
            break; // scan cap
        }
    }

    advisories.sort_by(|a, b| b.priority.cmp(&a.priority));
    advisories.truncate(limit);
    advisories
}

/// Get all feedback chunk IDs for a stream (for search-ignore detection).
fn get_feedback_chunk_ids(
    store: &RocksDbStore,
    stream_id: &str,
) -> std::collections::HashSet<String> {
    let prefix = format!("feedback:{}:", stream_id);
    let mut ids = std::collections::HashSet::new();

    for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
        if let Ok(fb) = serde_json::from_slice::<crate::stats_aggregator::FeedbackRecord>(&value) {
            ids.insert(fb.chunk_id);
        }
    }

    ids
}

// ---- ECA-14: Adaptive decay feedback loop ----

/// Compute a suggested decay threshold adjustment based on memory utilization.
///
/// - If dead_memory > 40% AND regret_rate < 5% -> suggest lowering threshold by 0.05
/// - If regret_rate > 10% -> suggest raising by 0.05
/// - Bounds: [0.2, 0.8], max 0.05/adjustment
/// - Returns None if no adjustment needed or oscillation detected (ECA-30 freeze).
pub fn compute_decay_adjustment(store: &RocksDbStore, stream_id: &str) -> Result<Option<f64>> {
    // ECA-30: Check for oscillation first — if oscillating, freeze (return None)
    if let Some(alert) = detect_oscillation(store, stream_id) {
        warn!(
            "Decay oscillation detected for stream {}: {} direction changes in {} days. \
             Freezing at midpoint {:.3}.",
            stream_id, alert.direction_changes, alert.window_days, alert.recommended_freeze_value
        );
        return Ok(None);
    }

    let summary = crate::stats_aggregator::StatsAggregator::get_summary(store, stream_id)?;

    let dead_pct = summary.dead_memory_pct;
    let regret = summary.regret_rate;

    let adjustment = if dead_pct > 0.40 && regret < 0.05 {
        // Too much dead memory, safe to decay more aggressively
        debug!(
            "Decay adjustment: dead_memory={:.1}% regret={:.1}% -> suggest -0.05",
            dead_pct * 100.0,
            regret * 100.0
        );
        Some(-0.05)
    } else if regret > 0.10 {
        // High regret rate, preserve more memories
        debug!(
            "Decay adjustment: dead_memory={:.1}% regret={:.1}% -> suggest +0.05",
            dead_pct * 100.0,
            regret * 100.0
        );
        Some(0.05)
    } else {
        None
    };

    // Record this adjustment in history for oscillation detection
    if let Some(adj) = adjustment {
        record_decay_adjustment(store, stream_id, adj);
    }

    Ok(adjustment)
}

// ---- ECA-30: Advisor oscillation detector ----

/// Alert returned when decay threshold is oscillating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OscillationAlert {
    /// Number of direction changes detected in the window.
    pub direction_changes: usize,
    /// Recommended value to freeze the decay threshold at (midpoint of recent adjustments).
    pub recommended_freeze_value: f64,
    /// Window size in days that was analyzed.
    pub window_days: usize,
}

/// Record a decay adjustment for oscillation tracking.
/// Stores under `advisory:decay_history:{stream_id}:{timestamp}`.
fn record_decay_adjustment(store: &RocksDbStore, stream_id: &str, adjustment: f64) {
    let now = chrono::Utc::now().timestamp() as u64;
    let key = format!("advisory:decay_history:{}:{}", stream_id, now);
    if let Err(e) = store.put(key.as_bytes(), &adjustment.to_le_bytes()) {
        warn!("Failed to record decay adjustment: {}", e);
    }
}

/// Detect if the decay threshold is oscillating.
///
/// Reads decay adjustment history from RocksDB for the last 21 days.
/// If there are 3 or more direction changes (positive → negative or vice versa),
/// returns an OscillationAlert with a recommended freeze value (midpoint).
pub fn detect_oscillation(store: &RocksDbStore, stream_id: &str) -> Option<OscillationAlert> {
    let now = chrono::Utc::now().timestamp() as u64;
    let window_secs = 21 * 86400u64;
    let window_start = now.saturating_sub(window_secs);

    let prefix = format!("advisory:decay_history:{}:", stream_id);
    let mut adjustments: Vec<(u64, f64)> = Vec::new();

    for (key, value) in store.prefix_scan(prefix.as_bytes()) {
        let key_str = String::from_utf8_lossy(&key);
        if let Some(ts_str) = key_str.strip_prefix(&prefix) {
            if let Ok(ts) = ts_str.parse::<u64>() {
                if ts >= window_start {
                    let val = if value.len() == 8 {
                        f64::from_le_bytes(value[..8].try_into().unwrap_or([0u8; 8]))
                    } else {
                        0.0
                    };
                    adjustments.push((ts, val));
                }
            }
        }
    }

    // Need at least 3 adjustments to detect oscillation
    if adjustments.len() < 3 {
        return None;
    }

    // Sort by timestamp
    adjustments.sort_by_key(|(ts, _)| *ts);

    // Count direction changes
    let mut direction_changes = 0usize;
    for i in 1..adjustments.len() {
        let prev_sign = adjustments[i - 1].1 >= 0.0;
        let curr_sign = adjustments[i].1 >= 0.0;
        if prev_sign != curr_sign {
            direction_changes += 1;
        }
    }

    if direction_changes >= 3 {
        // Compute midpoint of all adjustments as recommended freeze value
        let sum: f64 = adjustments.iter().map(|(_, v)| v).sum();
        let midpoint = sum / adjustments.len() as f64;

        Some(OscillationAlert {
            direction_changes,
            recommended_freeze_value: midpoint,
            window_days: 21,
        })
    } else {
        None
    }
}

// ---- ECA-24: Recommendation tracking ----

/// Outcome record for an advisory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisoryOutcome {
    pub advisory_id: String,
    pub followed: bool,
    pub timestamp: u64,
}

/// Track that an advisory was acted upon (followed) or dismissed (ignored).
pub fn track_advisory_outcome(
    store: &RocksDbStore,
    advisory_id: &str,
    followed: bool,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp() as u64;
    let outcome = AdvisoryOutcome {
        advisory_id: advisory_id.to_string(),
        followed,
        timestamp: now,
    };
    let key = format!("advisory:outcome:{}", advisory_id);
    let value = serde_json::to_vec(&outcome)?;
    store.put(key.as_bytes(), &value)?;
    debug!(
        "Tracked advisory outcome: {} followed={}",
        advisory_id, followed
    );
    Ok(())
}

/// Advisory effectiveness stats per type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisoryEffectiveness {
    pub total_outcomes: usize,
    pub follow_rate: f64,
    pub by_type: Vec<TypeEffectiveness>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeEffectiveness {
    pub advisory_type: String,
    pub total: usize,
    pub followed: usize,
    pub follow_rate: f64,
}

/// Get advisory effectiveness stats.
pub fn get_advisory_effectiveness(
    store: &RocksDbStore,
    stream_id: &str,
) -> Result<AdvisoryEffectiveness> {
    let outcome_prefix = b"advisory:outcome:";
    let advisory_prefix = format!("advisory:{}:", stream_id);

    // First, build a map of advisory_id -> advisory_type
    let mut type_map: HashMap<String, String> = HashMap::new();
    for (_key, value) in store.prefix_scan(advisory_prefix.as_bytes()) {
        if let Ok(advisory) = serde_json::from_slice::<AdvisoryItem>(&value) {
            type_map.insert(advisory.id.clone(), format!("{:?}", advisory.advisory_type));
        }
    }

    // Now scan outcomes
    let mut total = 0usize;
    let mut total_followed = 0usize;
    let mut by_type_map: HashMap<String, (usize, usize)> = HashMap::new(); // type -> (total, followed)

    for (_key, value) in store.prefix_scan(outcome_prefix) {
        if let Ok(outcome) = serde_json::from_slice::<AdvisoryOutcome>(&value) {
            total += 1;
            if outcome.followed {
                total_followed += 1;
            }

            let atype = type_map
                .get(&outcome.advisory_id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());

            let entry = by_type_map.entry(atype).or_insert((0, 0));
            entry.0 += 1;
            if outcome.followed {
                entry.1 += 1;
            }
        }
    }

    let follow_rate = if total > 0 {
        total_followed as f64 / total as f64
    } else {
        0.0
    };

    let by_type: Vec<TypeEffectiveness> = by_type_map
        .into_iter()
        .map(|(atype, (t, f))| TypeEffectiveness {
            advisory_type: atype,
            total: t,
            followed: f,
            follow_rate: if t > 0 { f as f64 / t as f64 } else { 0.0 },
        })
        .collect();

    Ok(AdvisoryEffectiveness {
        total_outcomes: total,
        follow_rate,
        by_type,
    })
}

// ---- ECA-25: Advisor learning — weight adjustment ----

/// A weight adjustment recommendation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightAdjustment {
    pub advisory_type: String,
    pub current_follow_rate: f64,
    pub action: String,
}

/// Adjust advisory priority weights based on follow rates.
/// Low follow_rate (<20%) -> deprioritize. High follow rate (>50%) -> promote.
pub fn adjust_advisory_weights(
    store: &RocksDbStore,
    stream_id: &str,
) -> Result<Vec<WeightAdjustment>> {
    let effectiveness = get_advisory_effectiveness(store, stream_id)?;
    let mut adjustments = Vec::new();

    for type_eff in &effectiveness.by_type {
        if type_eff.total < 3 {
            // Not enough data to make a judgment
            continue;
        }

        if type_eff.follow_rate < 0.2 {
            adjustments.push(WeightAdjustment {
                advisory_type: type_eff.advisory_type.clone(),
                current_follow_rate: type_eff.follow_rate,
                action: "deprioritize".to_string(),
            });
        } else if type_eff.follow_rate > 0.5 {
            adjustments.push(WeightAdjustment {
                advisory_type: type_eff.advisory_type.clone(),
                current_follow_rate: type_eff.follow_rate,
                action: "promote".to_string(),
            });
        }
    }

    // Store adjustments in RocksDB for reference
    if !adjustments.is_empty() {
        let key = format!("advisory:weights:{}", stream_id);
        let value = serde_json::to_vec(&adjustments)?;
        store.put(key.as_bytes(), &value)?;
        info!(
            "Advisory weight adjustments for stream {}: {} changes",
            stream_id,
            adjustments.len()
        );
    }

    Ok(adjustments)
}

/// Generate actionable advisories from current stats (always returns at least one if data exists).
pub fn generate_actionable_advisories(
    store: &RocksDbStore,
    stream_id: &str,
) -> Result<Vec<AdvisoryItem>> {
    let mut items = Vec::new();
    let now = chrono::Utc::now().timestamp() as u64;

    let summary = crate::stats_aggregator::StatsAggregator::get_summary(store, stream_id)?;

    // If dead_memory > 30%, suggest dream consolidation
    if summary.dead_memory_pct > 0.30 {
        items.push(AdvisoryItem {
            id: format!("adv-actionable-dead-memory-{}", now),
            advisory_type: AdvisoryType::HealthCheck,
            message: format!(
                "{:.0}% of stored memories have never been accessed and are over 30 days old.",
                summary.dead_memory_pct * 100.0,
            ),
            suggested_action: Some(
                "Run POST /v1/dream to consolidate stale memories and free up space".to_string(),
            ),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        });
    }

    // If recall_ratio < 0.3, suggest more specific queries
    if summary.recall_ratio < 0.3 && summary.store_count > 0 {
        items.push(AdvisoryItem {
            id: format!("adv-actionable-low-recall-{}", now),
            advisory_type: AdvisoryType::HealthCheck,
            message: format!(
                "Only {:.0}% of stored memories have ever been recalled.",
                summary.recall_ratio * 100.0,
            ),
            suggested_action: Some(
                "Try more specific search queries or use namespace filters to improve recall"
                    .to_string(),
            ),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Low,
            created_at: now,
        });
    }

    // If zero_result_rate > 20%, suggest reviewing stored content
    if summary.zero_result_rate > 0.20 && summary.search_count > 5 {
        items.push(AdvisoryItem {
            id: format!("adv-actionable-zero-results-{}", now),
            advisory_type: AdvisoryType::HealthCheck,
            message: format!(
                "{:.0}% of searches returned zero results.",
                summary.zero_result_rate * 100.0,
            ),
            suggested_action: Some(
                "Review your stored content to ensure it covers the topics you search for, or adjust query phrasing".to_string(),
            ),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: now,
        });
    }

    // If no search or store activity, suggest enabling embeddings / vector search
    if summary.search_count == 0 && summary.store_count == 0 {
        // No data at all — nothing actionable
    } else if summary.mean_top1_score == 0.0 && summary.search_count > 0 {
        items.push(AdvisoryItem {
            id: format!("adv-actionable-no-embeddings-{}", now),
            advisory_type: AdvisoryType::HealthCheck,
            message: "Searches are returning zero-score results, which may indicate embeddings are missing or vector search is disabled.".to_string(),
            suggested_action: Some(
                "Enable vector_enabled in config and run POST /v1/embed-missing to generate embeddings".to_string(),
            ),
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::High,
            created_at: now,
        });
    }

    // Always return at least a "memory health OK" if nothing else but data exists
    if items.is_empty() && (summary.store_count > 0 || summary.search_count > 0) {
        items.push(AdvisoryItem {
            id: format!("adv-actionable-health-ok-{}", now),
            advisory_type: AdvisoryType::HealthCheck,
            message: format!(
                "Memory health OK: {:.0}% recall ratio, {:.0}% dead memory, {:.0}% zero-result rate.",
                summary.recall_ratio * 100.0,
                summary.dead_memory_pct * 100.0,
                summary.zero_result_rate * 100.0,
            ),
            suggested_action: None,
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Low,
            created_at: now,
        });
    }

    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_query() {
        assert_eq!(normalize_query("  Hello World  "), "hello world");
        assert_eq!(normalize_query("TEST"), "test");
    }

    #[test]
    fn test_queries_similar() {
        assert!(queries_similar("what is rust", "what is rust programming"));
        assert!(!queries_similar("hello world", "goodbye moon"));
        assert!(queries_similar(
            "rust programming language",
            "rust programming language features"
        ));
    }

    #[test]
    fn test_detect_repeated_queries() {
        let events = vec![
            (1, "what is rust".to_string()),
            (2, "what is rust".to_string()),
            (3, "what is rust programming".to_string()),
            (4, "something else".to_string()),
        ];
        let repeated = detect_repeated_queries(&events, 3);
        assert_eq!(repeated.len(), 1);
        assert!(repeated[0].1 >= 3);
    }

    #[test]
    fn test_advisory_serialization() {
        let item = AdvisoryItem {
            id: "test-1".into(),
            advisory_type: AdvisoryType::RepeatedQuery,
            message: "test".into(),
            suggested_action: None,
            affected_chunk_ids: vec![],
            priority: AdvisoryPriority::Medium,
            created_at: 123,
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: AdvisoryItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-1");
    }

    #[test]
    fn test_priority_ordering() {
        assert!(AdvisoryPriority::High > AdvisoryPriority::Medium);
        assert!(AdvisoryPriority::Medium > AdvisoryPriority::Low);
    }
}
