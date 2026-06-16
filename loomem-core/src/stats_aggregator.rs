use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::event_log::{EventEntry, MemoryEvent};
use crate::storage::RocksDbStore;

/// Aggregate metrics computed from event logs.
pub struct StatsAggregator;

/// Report returned after an aggregation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationReport {
    pub events_processed: usize,
    pub metrics_written: usize,
}

/// Summary of all latest metrics for a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSummary {
    pub search_count: u64,
    pub store_count: u64,
    pub mean_top1_score: f64,
    pub zero_result_rate: f64,
    pub hit_rate: f64,
    pub consolidation_count: u64,
    pub consolidation_cost_usd: f64,
    // ECA-06: recall quality
    pub mrr: f64,
    pub score_distribution: ScoreDistribution,
    pub freshness_curve: FreshnessCurve,
    // ECA-07: memory utilization
    pub recall_ratio: f64,
    pub dead_memory_pct: f64,
    pub hot_memory_ids: Vec<String>,
    pub regret_rate: f64,
    // ECA-27: association mechanism effectiveness
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mechanism_stats: Option<crate::associator::tracking::MechanismStats>,
}

impl Default for StatsSummary {
    fn default() -> Self {
        Self {
            search_count: 0,
            store_count: 0,
            mean_top1_score: 0.0,
            zero_result_rate: 0.0,
            hit_rate: 0.0,
            consolidation_count: 0,
            consolidation_cost_usd: 0.0,
            mrr: 0.0,
            score_distribution: ScoreDistribution::default(),
            freshness_curve: FreshnessCurve::default(),
            recall_ratio: 0.0,
            dead_memory_pct: 0.0,
            hot_memory_ids: Vec::new(),
            regret_rate: 0.0,
            mechanism_stats: None,
        }
    }
}

/// Score distribution buckets.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScoreDistribution {
    /// 0.0 - 0.3
    pub very_low: u64,
    /// 0.3 - 0.5
    pub low: u64,
    /// 0.5 - 0.65
    pub medium: u64,
    /// 0.65 - 0.8
    pub high: u64,
    /// 0.8 - 1.0
    pub very_high: u64,
}

/// Freshness curve — how old are the recalled chunks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FreshnessCurve {
    /// < 1 day old
    pub under_1d: u64,
    /// 1-7 days old
    pub d1_to_d7: u64,
    /// 7-30 days old
    pub d7_to_d30: u64,
    /// 30+ days old
    pub over_30d: u64,
}

/// Feedback record stored per chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub chunk_id: String,
    pub stream_id: String,
    pub useful: bool,
    pub rank_position: Option<usize>,
    pub timestamp: u64,
}

/// Internal accumulator for per-stream, per-hour metrics.
#[derive(Debug, Default)]
struct HourBucket {
    search_count: u64,
    store_count: u64,
    top1_scores: Vec<f32>,
    zero_result_count: u64,
    hit_count: u64,
    consolidation_count: u64,
    consolidation_cost_usd: f64,
    // Score distribution
    score_very_low: u64,
    score_low: u64,
    score_medium: u64,
    score_high: u64,
    score_very_high: u64,
}

impl StatsAggregator {
    /// Run aggregation on event log files, compute metrics, store in RocksDB.
    pub fn aggregate(store: &RocksDbStore, events_dir: &Path) -> Result<AggregationReport> {
        let mut events_processed = 0usize;
        let mut metrics_written = 0usize;

        // Collect all .jsonl files
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if events_dir.exists() {
            for entry in fs::read_dir(events_dir)
                .with_context(|| format!("Failed to read events dir: {}", events_dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    files.push(path);
                }
            }
        }

        if files.is_empty() {
            info!("No event log files found in {}", events_dir.display());
            return Ok(AggregationReport {
                events_processed: 0,
                metrics_written: 0,
            });
        }

        // key: (stream_id, hour_bucket)
        let mut buckets: HashMap<(String, u64), HourBucket> = HashMap::new();

        for file_path in &files {
            let file = match fs::File::open(file_path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to open event file {}: {}", file_path.display(), e);
                    continue;
                }
            };
            let reader = BufReader::new(file);

            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        warn!("Failed to read line from {}: {}", file_path.display(), e);
                        continue;
                    }
                };

                if line.trim().is_empty() {
                    continue;
                }

                let entry: EventEntry = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("Failed to parse event entry: {}", e);
                        continue;
                    }
                };

                events_processed += 1;
                let hour = entry.timestamp / 3600;

                match &entry.event {
                    MemoryEvent::Search {
                        stream_id,
                        top_scores,
                        result_count,
                        ..
                    } => {
                        let bucket = buckets.entry((stream_id.clone(), hour)).or_default();
                        bucket.search_count += 1;

                        if *result_count == 0 {
                            bucket.zero_result_count += 1;
                        }

                        if let Some(&top1) = top_scores.first() {
                            bucket.top1_scores.push(top1);
                            if top1 > 0.65 {
                                bucket.hit_count += 1;
                            }
                            // Score distribution
                            if top1 < 0.3 {
                                bucket.score_very_low += 1;
                            } else if top1 < 0.5 {
                                bucket.score_low += 1;
                            } else if top1 < 0.65 {
                                bucket.score_medium += 1;
                            } else if top1 < 0.8 {
                                bucket.score_high += 1;
                            } else {
                                bucket.score_very_high += 1;
                            }
                        }
                    }
                    MemoryEvent::Store { stream_id, .. } => {
                        let bucket = buckets.entry((stream_id.clone(), hour)).or_default();
                        bucket.store_count += 1;
                    }
                    MemoryEvent::Consolidation {
                        input_count,
                        output_count: _,
                        cost_usd,
                        ..
                    } => {
                        // Consolidation events don't carry stream_id — attribute to "_global"
                        let bucket = buckets.entry(("_global".to_string(), hour)).or_default();
                        bucket.consolidation_count += 1;
                        bucket.consolidation_cost_usd += cost_usd;
                        let _ = input_count; // avoid unused warning
                    }
                    MemoryEvent::CostEvent { .. }
                    | MemoryEvent::Association { .. }
                    | MemoryEvent::DreamCycle { .. } => {
                        // Not aggregated in basic metrics yet
                    }
                }
            }
        }

        // Write aggregated metrics to RocksDB
        for ((stream_id, hour), bucket) in &buckets {
            let prefix = format!("stats:{}:", stream_id);

            // search_count
            Self::write_metric(
                store,
                &prefix,
                "search_count",
                *hour,
                bucket.search_count as f64,
            )?;
            metrics_written += 1;

            // store_count
            Self::write_metric(
                store,
                &prefix,
                "store_count",
                *hour,
                bucket.store_count as f64,
            )?;
            metrics_written += 1;

            // mean_top1_score
            let mean_top1 = if bucket.top1_scores.is_empty() {
                0.0
            } else {
                bucket.top1_scores.iter().sum::<f32>() as f64 / bucket.top1_scores.len() as f64
            };
            Self::write_metric(store, &prefix, "mean_top1_score", *hour, mean_top1)?;
            metrics_written += 1;

            // zero_result_rate
            let zrr = if bucket.search_count == 0 {
                0.0
            } else {
                bucket.zero_result_count as f64 / bucket.search_count as f64
            };
            Self::write_metric(store, &prefix, "zero_result_rate", *hour, zrr)?;
            metrics_written += 1;

            // hit_rate
            let hr = if bucket.search_count == 0 {
                0.0
            } else {
                bucket.hit_count as f64 / bucket.search_count as f64
            };
            Self::write_metric(store, &prefix, "hit_rate", *hour, hr)?;
            metrics_written += 1;

            // consolidation_count
            Self::write_metric(
                store,
                &prefix,
                "consolidation_count",
                *hour,
                bucket.consolidation_count as f64,
            )?;
            metrics_written += 1;

            // consolidation_cost_usd
            Self::write_metric(
                store,
                &prefix,
                "consolidation_cost_usd",
                *hour,
                bucket.consolidation_cost_usd,
            )?;
            metrics_written += 1;

            // Score distribution (store as JSON blob for the hour)
            let dist = ScoreDistribution {
                very_low: bucket.score_very_low,
                low: bucket.score_low,
                medium: bucket.score_medium,
                high: bucket.score_high,
                very_high: bucket.score_very_high,
            };
            let dist_key = format!("{}score_distribution:{}", prefix, hour);
            let dist_json = serde_json::to_vec(&dist)?;
            store.put(dist_key.as_bytes(), &dist_json)?;
            metrics_written += 1;
        }

        info!(
            "Stats aggregation complete: {} events processed, {} metrics written",
            events_processed, metrics_written
        );

        Ok(AggregationReport {
            events_processed,
            metrics_written,
        })
    }

    /// Get the latest metric value for a stream.
    pub fn get_metric(store: &RocksDbStore, stream_id: &str, metric: &str) -> Result<Option<f64>> {
        let prefix = format!("stats:{}:{}:", stream_id, metric);
        let mut latest: Option<(u64, f64)> = None;

        for (key, value) in store.prefix_scan(prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(hour_str) = key_str.strip_prefix(&prefix) {
                if let Ok(hour) = hour_str.parse::<u64>() {
                    let val = Self::parse_metric_value(&value);
                    match latest {
                        Some((h, _)) if hour > h => {
                            latest = Some((hour, val));
                        }
                        None => {
                            latest = Some((hour, val));
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(latest.map(|(_, v)| v))
    }

    /// Get metric time series for a stream (last N hours).
    pub fn get_trend(
        store: &RocksDbStore,
        stream_id: &str,
        metric: &str,
        hours: usize,
    ) -> Result<Vec<(u64, f64)>> {
        let prefix = format!("stats:{}:{}:", stream_id, metric);
        let now_hour = chrono::Utc::now().timestamp() as u64 / 3600;
        let min_hour = now_hour.saturating_sub(hours as u64);

        let mut series: Vec<(u64, f64)> = Vec::new();

        for (key, value) in store.prefix_scan(prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(hour_str) = key_str.strip_prefix(&prefix) {
                if let Ok(hour) = hour_str.parse::<u64>() {
                    if hour >= min_hour {
                        let val = Self::parse_metric_value(&value);
                        series.push((hour * 3600, val)); // convert back to timestamp
                    }
                }
            }
        }

        series.sort_by_key(|(ts, _)| *ts);
        Ok(series)
    }

    /// Get summary for a stream (all latest metrics).
    pub fn get_summary(store: &RocksDbStore, stream_id: &str) -> Result<StatsSummary> {
        let search_count = Self::get_cumulative(store, stream_id, "search_count")?;
        let mean_top1_score = Self::get_metric(store, stream_id, "mean_top1_score")?.unwrap_or(0.0);
        let zero_result_rate =
            Self::get_metric(store, stream_id, "zero_result_rate")?.unwrap_or(0.0);
        let hit_rate = Self::get_metric(store, stream_id, "hit_rate")?.unwrap_or(0.0);
        let consolidation_count = Self::get_cumulative(store, stream_id, "consolidation_count")?;
        let consolidation_cost_usd =
            Self::get_cumulative_f64(store, stream_id, "consolidation_cost_usd")?;

        // MRR from feedback
        let mrr = Self::get_metric(store, stream_id, "mrr")?.unwrap_or(0.0);

        // Aggregate score distribution from latest bucket
        let score_distribution = Self::get_latest_score_distribution(store, stream_id)?;

        // Freshness curve from latest bucket
        let freshness_curve = Self::get_latest_freshness_curve(store, stream_id)?;

        // Scan chunks once: used for store_count (restart-safe — lives in RocksDB, not in
        // event logs) and for utilization metrics (avoids a second full scan).
        let chunks = Self::get_stream_chunks(store, stream_id)?;
        let store_count = chunks.len() as u64;
        let utilization = Self::compute_utilization_from_chunks(&chunks);

        // ECA-27: mechanism effectiveness
        let mechanism_stats =
            crate::associator::tracking::get_mechanism_effectiveness(store, stream_id).ok();

        Ok(StatsSummary {
            search_count: search_count as u64,
            store_count,
            mean_top1_score,
            zero_result_rate,
            hit_rate,
            consolidation_count: consolidation_count as u64,
            consolidation_cost_usd,
            mrr,
            score_distribution,
            freshness_curve,
            recall_ratio: utilization.recall_ratio,
            dead_memory_pct: utilization.dead_memory_pct,
            hot_memory_ids: utilization.hot_memory_ids,
            regret_rate: 0.0, // placeholder — requires dropped_ids tracking
            mechanism_stats,
        })
    }

    /// Store feedback for a chunk (useful/not useful).
    pub fn store_feedback(store: &RocksDbStore, feedback: &FeedbackRecord) -> Result<()> {
        let key = format!(
            "feedback:{}:{}:{}",
            feedback.stream_id, feedback.chunk_id, feedback.timestamp
        );
        let value = serde_json::to_vec(feedback)?;
        store.put(key.as_bytes(), &value)?;

        // Update MRR if rank_position is provided
        if feedback.useful {
            if let Some(pos) = feedback.rank_position {
                Self::update_mrr(store, &feedback.stream_id, pos)?;
            }
        }

        Ok(())
    }

    /// Compute freshness curve from search events — uses chunk timestamps relative to query time.
    pub fn compute_freshness_for_stream(
        store: &RocksDbStore,
        stream_id: &str,
    ) -> Result<FreshnessCurve> {
        let now = chrono::Utc::now().timestamp() as u64;
        let mut curve = FreshnessCurve::default();

        // Scan chunks for this stream and bucket by age
        let chunks = Self::get_stream_chunks(store, stream_id)?;
        for chunk in &chunks {
            if chunk.access_count > 0 {
                let age_secs = now.saturating_sub(chunk.timestamp);
                let age_days = age_secs / 86400;
                if age_days < 1 {
                    curve.under_1d += 1;
                } else if age_days < 7 {
                    curve.d1_to_d7 += 1;
                } else if age_days < 30 {
                    curve.d7_to_d30 += 1;
                } else {
                    curve.over_30d += 1;
                }
            }
        }

        // Store the latest freshness curve
        let key = format!("stats:{}:freshness_curve:latest", stream_id);
        let val = serde_json::to_vec(&curve)?;
        store.put(key.as_bytes(), &val)?;

        Ok(curve)
    }

    // ---- internal helpers ----

    fn write_metric(
        store: &RocksDbStore,
        prefix: &str,
        metric: &str,
        hour: u64,
        value: f64,
    ) -> Result<()> {
        let key = format!("{}{}:{}", prefix, metric, hour);
        let val = value.to_le_bytes();
        store.put(key.as_bytes(), &val)?;
        Ok(())
    }

    fn parse_metric_value(bytes: &[u8]) -> f64 {
        if bytes.len() == 8 {
            f64::from_le_bytes(bytes.try_into().unwrap_or([0u8; 8]))
        } else {
            // Fallback: try parsing as string
            String::from_utf8_lossy(bytes).parse::<f64>().unwrap_or(0.0)
        }
    }

    /// Get cumulative (sum of all hour buckets) for a metric.
    fn get_cumulative(store: &RocksDbStore, stream_id: &str, metric: &str) -> Result<f64> {
        let prefix = format!("stats:{}:{}:", stream_id, metric);
        let mut total = 0.0;
        for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
            total += Self::parse_metric_value(&value);
        }
        Ok(total)
    }

    fn get_cumulative_f64(store: &RocksDbStore, stream_id: &str, metric: &str) -> Result<f64> {
        Self::get_cumulative(store, stream_id, metric)
    }

    fn get_latest_score_distribution(
        store: &RocksDbStore,
        stream_id: &str,
    ) -> Result<ScoreDistribution> {
        let prefix = format!("stats:{}:score_distribution:", stream_id);
        let mut latest: Option<(u64, ScoreDistribution)> = None;

        for (key, value) in store.prefix_scan(prefix.as_bytes()) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(hour_str) = key_str.strip_prefix(&prefix) {
                if hour_str == "latest" {
                    continue; // skip freshness_curve:latest key pattern
                }
                if let Ok(hour) = hour_str.parse::<u64>() {
                    if let Ok(dist) = serde_json::from_slice::<ScoreDistribution>(&value) {
                        match latest {
                            Some((h, _)) if hour > h => {
                                latest = Some((hour, dist));
                            }
                            None => {
                                latest = Some((hour, dist));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(latest.map(|(_, d)| d).unwrap_or_default())
    }

    fn get_latest_freshness_curve(store: &RocksDbStore, stream_id: &str) -> Result<FreshnessCurve> {
        let key = format!("stats:{}:freshness_curve:latest", stream_id);
        match store.get(key.as_bytes())? {
            Some(bytes) => {
                let curve: FreshnessCurve = serde_json::from_slice(&bytes).unwrap_or_default();
                Ok(curve)
            }
            None => Ok(FreshnessCurve::default()),
        }
    }

    /// Update MRR (Mean Reciprocal Rank) incrementally.
    fn update_mrr(store: &RocksDbStore, stream_id: &str, rank_position: usize) -> Result<()> {
        let count_key = format!("stats:{}:mrr_count", stream_id);
        let sum_key = format!("stats:{}:mrr_sum", stream_id);

        let count: u64 = store
            .get(count_key.as_bytes())?
            .map(|b| Self::parse_metric_value(&b) as u64)
            .unwrap_or(0);
        let sum: f64 = store
            .get(sum_key.as_bytes())?
            .map(|b| Self::parse_metric_value(&b))
            .unwrap_or(0.0);

        let rr = 1.0 / (rank_position as f64 + 1.0); // 0-indexed rank
        let new_count = count + 1;
        let new_sum = sum + rr;
        let new_mrr = new_sum / new_count as f64;

        store.put(count_key.as_bytes(), &new_count.to_le_bytes())?;
        store.put(sum_key.as_bytes(), &new_sum.to_le_bytes())?;

        // Write current MRR to the standard metric key
        let now_hour = chrono::Utc::now().timestamp() as u64 / 3600;
        let mrr_key = format!("stats:{}:mrr:{}", stream_id, now_hour);
        store.put(mrr_key.as_bytes(), &new_mrr.to_le_bytes())?;

        Ok(())
    }

    /// Get chunks for a specific stream (used for utilization metrics).
    fn get_stream_chunks(
        store: &RocksDbStore,
        stream_id: &str,
    ) -> Result<Vec<crate::storage::Chunk>> {
        let mut chunks = Vec::new();
        for prefix in &[b"chunk:L0:" as &[u8], b"chunk:L1:"] {
            for (_, value) in store.prefix_scan(prefix) {
                match store.decode_chunk(&value) {
                    Ok(chunk) if chunk.deleted_at.is_none() && chunk.stream == stream_id => {
                        chunks.push(chunk);
                    }
                    _ => {}
                }
            }
        }
        Ok(chunks)
    }

    /// ECA-07: Compute memory utilization metrics from an already-loaded chunk slice.
    fn compute_utilization_from_chunks(chunks: &[crate::storage::Chunk]) -> UtilizationMetrics {
        let total = chunks.len();

        if total == 0 {
            return UtilizationMetrics {
                recall_ratio: 0.0,
                dead_memory_pct: 0.0,
                hot_memory_ids: Vec::new(),
            };
        }

        let now = chrono::Utc::now().timestamp() as u64;
        let thirty_days_secs = 30 * 86400u64;

        let mut recalled_count = 0usize;
        let mut dead_count = 0usize;
        let mut access_counts: Vec<(String, u32)> = Vec::new();

        for chunk in chunks {
            if chunk.access_count > 0 {
                recalled_count += 1;
            }

            // "Dead" = never accessed and older than 30 days
            let age = now.saturating_sub(chunk.timestamp);
            if chunk.access_count == 0 && age > thirty_days_secs {
                dead_count += 1;
            }

            access_counts.push((chunk.id.clone(), chunk.access_count));
        }

        // Hot memory: top 10% by access count
        access_counts.sort_by_key(|b| std::cmp::Reverse(b.1));
        let top_10_pct = (total as f64 * 0.1).ceil() as usize;
        let hot_memory_ids: Vec<String> = access_counts
            .iter()
            .take(top_10_pct.max(1))
            .filter(|(_, c)| *c > 0)
            .map(|(id, _)| id.clone())
            .collect();

        UtilizationMetrics {
            recall_ratio: recalled_count as f64 / total as f64,
            dead_memory_pct: dead_count as f64 / total as f64,
            hot_memory_ids,
        }
    }

    /// ECA-07: Compute memory utilization metrics by scanning RocksDB directly.
    #[allow(dead_code)] // ECA-07 hook; invoked via future stats route not yet wired
    fn compute_utilization(store: &RocksDbStore, stream_id: &str) -> Result<UtilizationMetrics> {
        let chunks = Self::get_stream_chunks(store, stream_id)?;
        Ok(Self::compute_utilization_from_chunks(&chunks))
    }
}

/// Internal struct for ECA-07 utilization metrics.
struct UtilizationMetrics {
    recall_ratio: f64,
    dead_memory_pct: f64,
    hot_memory_ids: Vec<String>,
}

// ---- ECA-26: Per-stream memory profile ----

/// A per-stream memory profile summarizing usage patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProfile {
    pub dominant_query_types: Vec<(String, usize)>,
    pub avg_query_frequency: f64,
    pub hot_topics: Vec<(String, usize)>,
    pub memory_density: f64,
    pub total_chunks: usize,
    pub total_embeddings: usize,
}

impl MemoryProfile {
    /// Compute a memory profile for a stream.
    pub fn compute(store: &RocksDbStore, events_dir: &Path, stream_id: &str) -> Result<Self> {
        let now = chrono::Utc::now().timestamp() as u64;
        let seven_days = 7 * 86400u64;
        let window_start = now.saturating_sub(seven_days);

        // Count chunks and embeddings
        let chunks = StatsAggregator::get_stream_chunks(store, stream_id)?;
        let total_chunks = chunks.len();

        let mut total_embeddings = 0usize;
        for chunk in &chunks {
            if store.get_embedding(&chunk.id).ok().flatten().is_some() {
                total_embeddings += 1;
            }
        }

        // Memory density: chunks per day over last 7 days
        let recent_chunks = chunks
            .iter()
            .filter(|c| c.timestamp >= window_start)
            .count();
        let memory_density = recent_chunks as f64 / 7.0;

        // Hot topics: most common subjects from extraction_meta
        let mut topic_counts: HashMap<String, usize> = HashMap::new();
        for chunk in &chunks {
            if let Some(ref meta) = chunk.extraction_meta {
                if let Some(ref subject) = meta.subject {
                    *topic_counts.entry(subject.to_lowercase()).or_insert(0) += 1;
                }
            }
        }
        let mut hot_topics: Vec<(String, usize)> = topic_counts.into_iter().collect();
        hot_topics.sort_by_key(|b| std::cmp::Reverse(b.1));
        hot_topics.truncate(10);

        // Query type classification from event logs
        let mut query_type_counts: HashMap<String, usize> = HashMap::new();
        let mut search_count_7d = 0usize;

        // Read event logs for search queries
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

                if let MemoryEvent::Search {
                    stream_id: ref sid,
                    ref query,
                    ..
                } = entry.event
                {
                    if sid == stream_id && entry.timestamp >= window_start {
                        search_count_7d += 1;
                        // Simple classification based on query length/keywords
                        let complexity = classify_query_simple(query);
                        *query_type_counts.entry(complexity).or_insert(0) += 1;
                    }
                }
            }
        }

        let avg_query_frequency = search_count_7d as f64 / 7.0;

        let mut dominant_query_types: Vec<(String, usize)> =
            query_type_counts.into_iter().collect();
        dominant_query_types.sort_by_key(|b| std::cmp::Reverse(b.1));

        Ok(MemoryProfile {
            dominant_query_types,
            avg_query_frequency,
            hot_topics,
            memory_density,
            total_chunks,
            total_embeddings,
        })
    }
}

/// Simple query complexity classifier for profile stats.
fn classify_query_simple(query: &str) -> String {
    let lower = query.to_lowercase();
    let word_count = query.split_whitespace().count();

    if word_count <= 3 {
        "simple".to_string()
    } else if word_count <= 8 {
        "medium".to_string()
    } else if lower.contains("how") || lower.contains("why") || lower.contains("compare") {
        "complex".to_string()
    } else {
        "medium".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_summary_serialization() {
        let summary = StatsSummary::default();
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: StatsSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.search_count, 0);
        assert_eq!(parsed.store_count, 0);
    }

    #[test]
    fn test_score_distribution_default() {
        let dist = ScoreDistribution::default();
        assert_eq!(dist.very_low, 0);
        assert_eq!(dist.very_high, 0);
    }

    #[test]
    fn test_freshness_curve_default() {
        let curve = FreshnessCurve::default();
        assert_eq!(curve.under_1d, 0);
        assert_eq!(curve.over_30d, 0);
    }

    #[test]
    fn test_feedback_record_serialization() {
        let fb = FeedbackRecord {
            chunk_id: "abc123".into(),
            stream_id: "100".into(),
            useful: true,
            rank_position: Some(0),
            timestamp: 1234567890,
        };
        let json = serde_json::to_string(&fb).unwrap();
        let parsed: FeedbackRecord = serde_json::from_str(&json).unwrap();
        assert!(parsed.useful);
        assert_eq!(parsed.rank_position, Some(0));
    }
}
