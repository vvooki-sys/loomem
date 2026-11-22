//! Persisted dream schedule (`meta:dream_schedule`) and the pure logic that
//! decides when the scheduler's periodic dream pass is due.
//!
//! The row overrides `[dream] schedule_enabled` / `schedule_interval_secs`
//! from `config.toml` at runtime (an admin edits it through
//! `PUT /v1/dream/schedule`). A `None` field inherits the config value.
//! `load`/`save` are the only functions that touch storage; everything else
//! is pure so the schedule arithmetic can be unit-tested without a store.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dream::DreamConfig;
use crate::storage::keys::DREAM_SCHEDULE_KEY;
use crate::storage::RocksDbStore;

/// Lower bound for a schedule interval (1 hour).
pub const MIN_INTERVAL_SECS: u64 = 3_600;
/// Upper bound for a schedule interval (30 days).
pub const MAX_INTERVAL_SECS: u64 = 2_592_000;

/// `true` when `secs` is an acceptable `interval_secs` (1 h ..= 30 d).
pub fn interval_in_bounds(secs: u64) -> bool {
    (MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs)
}

/// Runtime schedule state. Every field carries `#[serde(default)]` so rows
/// written by older binaries keep deserialising (CLAUDE.md hard rule 5).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DreamScheduleRow {
    /// `None` = not overridden, inherit `[dream] schedule_enabled`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `None` = inherit `[dream] schedule_interval_secs`.
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Epoch seconds the row was first anchored (base for the first run).
    #[serde(default)]
    pub anchor_at: Option<u64>,
    /// Epoch seconds the last scheduled or queued pass started.
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// Set by `POST /v1/dream/schedule/run`; cleared when the pass starts.
    #[serde(default)]
    pub run_requested_at: Option<u64>,
    /// Outcome of the last completed pass.
    #[serde(default)]
    pub last_result: Option<DreamPassSummary>,
}

/// Aggregate of one pass over all active streams.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DreamPassSummary {
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: u64,
    #[serde(default)]
    pub streams_total: usize,
    #[serde(default)]
    pub streams_failed: usize,
    #[serde(default)]
    pub chunks_processed: usize,
    #[serde(default)]
    pub facts_merged: usize,
    #[serde(default)]
    pub contradictions_resolved: usize,
    #[serde(default)]
    pub cost_usd: f64,
    /// Streams whose run hit the per-stream `cost_cap_usd_per_run`.
    #[serde(default)]
    pub cost_cap_reached: usize,
    /// `"schedule"` or `"manual"`.
    #[serde(default)]
    pub trigger: String,
}

/// Where the effective `enabled` / `interval_secs` pair comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleSource {
    Config,
    Override,
}

/// The schedule after merging the row with the config defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveSchedule {
    pub enabled: bool,
    pub interval_secs: u64,
    pub source: ScheduleSource,
}

/// Read the row; a missing row is the default (inherit config, never run).
pub fn load(store: &RocksDbStore) -> Result<DreamScheduleRow> {
    match store.get(DREAM_SCHEDULE_KEY)? {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode meta:dream_schedule"),
        None => Ok(DreamScheduleRow::default()),
    }
}

/// Persist the row as JSON in the default column family.
pub fn save(store: &RocksDbStore, row: &DreamScheduleRow) -> Result<()> {
    let bytes = serde_json::to_vec(row).context("encode meta:dream_schedule")?;
    store.put(DREAM_SCHEDULE_KEY, &bytes)
}

/// Merge the row's overrides with the config defaults.
pub fn effective(row: &DreamScheduleRow, cfg: &DreamConfig) -> EffectiveSchedule {
    let overridden = row.enabled.is_some() || row.interval_secs.is_some();
    EffectiveSchedule {
        enabled: row.enabled.unwrap_or(cfg.schedule_enabled),
        interval_secs: row.interval_secs.unwrap_or(cfg.schedule_interval_secs),
        source: if overridden {
            ScheduleSource::Override
        } else {
            ScheduleSource::Config
        },
    }
}

/// Epoch seconds of the next scheduled run: `base + interval`, where
/// `base = last_run_at.or(anchor_at)`. `None` when the schedule is disabled
/// or no base exists yet (the scheduler anchors the row on its first poll).
pub fn next_run_at(row: &DreamScheduleRow, eff: &EffectiveSchedule) -> Option<u64> {
    if !eff.enabled {
        return None;
    }
    let base = row.last_run_at.or(row.anchor_at)?;
    Some(base.saturating_add(eff.interval_secs))
}

/// Due when a manual run was requested, or when the schedule is enabled and
/// `now` has reached `next_run_at`.
pub fn is_due(row: &DreamScheduleRow, eff: &EffectiveSchedule, now: u64) -> bool {
    if row.run_requested_at.is_some() {
        return true;
    }
    next_run_at(row, eff).is_some_and(|next| now >= next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RocksDbConfig;
    use tempfile::TempDir;

    fn cfg(enabled: bool, interval: u64) -> DreamConfig {
        DreamConfig {
            schedule_enabled: enabled,
            schedule_interval_secs: interval,
            ..DreamConfig::default()
        }
    }

    fn row(anchor: Option<u64>, last: Option<u64>) -> DreamScheduleRow {
        DreamScheduleRow {
            anchor_at: anchor,
            last_run_at: last,
            ..DreamScheduleRow::default()
        }
    }

    #[test]
    fn interval_bounds_are_inclusive() {
        assert!(!interval_in_bounds(MIN_INTERVAL_SECS - 1));
        assert!(interval_in_bounds(MIN_INTERVAL_SECS));
        assert!(interval_in_bounds(MAX_INTERVAL_SECS));
        assert!(!interval_in_bounds(MAX_INTERVAL_SECS + 1));
    }

    #[test]
    fn effective_inherits_config_when_row_is_empty() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 86_400));
        assert_eq!(
            eff,
            EffectiveSchedule {
                enabled: true,
                interval_secs: 86_400,
                source: ScheduleSource::Config,
            }
        );
    }

    #[test]
    fn effective_prefers_row_overrides() {
        let mut r = DreamScheduleRow::default();
        r.enabled = Some(false);
        r.interval_secs = Some(7_200);
        let eff = effective(&r, &cfg(true, 86_400));
        assert_eq!(
            eff,
            EffectiveSchedule {
                enabled: false,
                interval_secs: 7_200,
                source: ScheduleSource::Override,
            }
        );
    }

    #[test]
    fn effective_partial_override_counts_as_override() {
        let mut r = DreamScheduleRow::default();
        r.interval_secs = Some(3_600);
        let eff = effective(&r, &cfg(true, 86_400));
        assert!(eff.enabled);
        assert_eq!(eff.interval_secs, 3_600);
        assert_eq!(eff.source, ScheduleSource::Override);
    }

    #[test]
    fn next_run_at_none_without_base_or_when_disabled() {
        let enabled = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert_eq!(next_run_at(&row(None, None), &enabled), None);

        let disabled = effective(&DreamScheduleRow::default(), &cfg(false, 100));
        assert_eq!(next_run_at(&row(Some(10), None), &disabled), None);
    }

    #[test]
    fn next_run_at_uses_last_run_over_anchor() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert_eq!(next_run_at(&row(Some(10), None), &eff), Some(110));
        assert_eq!(next_run_at(&row(Some(10), Some(500)), &eff), Some(600));
    }

    #[test]
    fn is_due_follows_interval_and_manual_request() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert!(!is_due(&row(None, None), &eff, 1_000));
        assert!(!is_due(&row(Some(10), None), &eff, 109));
        assert!(is_due(&row(Some(10), None), &eff, 110));

        let disabled = effective(&DreamScheduleRow::default(), &cfg(false, 100));
        assert!(!is_due(&row(Some(10), None), &disabled, 10_000));

        let mut requested = row(None, None);
        requested.run_requested_at = Some(5);
        assert!(is_due(&requested, &disabled, 6));
    }

    #[test]
    fn load_missing_row_is_default_and_save_round_trips() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = RocksDbStore::open(
            tmp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "none".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )?;
        assert_eq!(load(&store)?, DreamScheduleRow::default());

        let row = DreamScheduleRow {
            enabled: Some(true),
            interval_secs: Some(43_200),
            anchor_at: Some(1),
            last_run_at: Some(2),
            run_requested_at: Some(3),
            last_result: Some(DreamPassSummary {
                started_at: 2,
                finished_at: 4,
                streams_total: 3,
                streams_failed: 1,
                chunks_processed: 20,
                facts_merged: 5,
                contradictions_resolved: 1,
                cost_usd: 0.01,
                cost_cap_reached: 0,
                trigger: "schedule".to_string(),
            }),
        };
        save(&store, &row)?;
        assert_eq!(load(&store)?, row);
        Ok(())
    }

    #[test]
    fn row_deserialises_from_empty_object() {
        let row: DreamScheduleRow = serde_json::from_str("{}").expect("all fields default");
        assert_eq!(row, DreamScheduleRow::default());
    }
}
