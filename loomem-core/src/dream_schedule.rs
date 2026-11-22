//! Persisted dream schedule (`meta:dream_schedule`) and the pure logic that
//! decides when the scheduler's periodic dream pass is due.
//!
//! The row overrides `[dream] schedule_enabled` / `schedule_interval_secs`
//! from `config.toml` at runtime (an admin edits it through
//! `PUT /v1/dream/schedule`). A `None` field inherits the config value.
//!
//! Storage access is confined to [`load`] and [`update`]; every writer
//! (HTTP handlers, the scheduler poll, pass bookkeeping) goes through
//! [`update`], which serialises read-modify-write cycles behind one
//! process-wide mutex. Everything else is pure so the schedule arithmetic
//! and the per-poll [`decide`] function can be unit-tested without a store.

use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dream::DreamConfig;
use crate::storage::keys::DREAM_SCHEDULE_KEY;
use crate::storage::RocksDbStore;

/// Lower bound for a schedule interval (1 hour).
pub const MIN_INTERVAL_SECS: u64 = 3_600;
/// Upper bound for a schedule interval (30 days).
pub const MAX_INTERVAL_SECS: u64 = 2_592_000;

/// `DreamPassSummary::status` values.
pub const STATUS_OK: &str = "ok";
/// The soft deadline stopped the pass before every stream was visited.
pub const STATUS_DEADLINE: &str = "deadline";
/// The pass could not run at all (e.g. the stream list could not be read).
pub const STATUS_FAILED: &str = "failed";

/// `DreamPassSummary::trigger` values.
pub const TRIGGER_SCHEDULE: &str = "schedule";
pub const TRIGGER_MANUAL: &str = "manual";

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
    /// Epoch seconds the schedule was (re-)anchored: first poll, or the moment
    /// the schedule went from disabled to enabled. Base for the next run
    /// together with `last_run_at` (the later of the two wins).
    #[serde(default)]
    pub anchor_at: Option<u64>,
    /// Epoch seconds the last scheduled or queued pass started.
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// Set by `POST /v1/dream/schedule/run`; cleared when the pass starts.
    #[serde(default)]
    pub run_requested_at: Option<u64>,
    /// Outcome of the last pass that started (ok, deadline or failed).
    #[serde(default)]
    pub last_result: Option<DreamPassSummary>,
    /// Effective `enabled` as seen by the last poll. Lets a disabled → enabled
    /// transition re-anchor instead of running a surprise catch-up pass.
    #[serde(default)]
    pub last_seen_enabled: Option<bool>,
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
    /// [`TRIGGER_SCHEDULE`] or [`TRIGGER_MANUAL`].
    #[serde(default)]
    pub trigger: String,
    /// [`STATUS_OK`], [`STATUS_DEADLINE`] or [`STATUS_FAILED`].
    #[serde(default)]
    pub status: String,
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

/// What one scheduler poll should do with the row (see [`decide`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Start a pass now with this trigger.
    Run { trigger: &'static str },
    /// Persist this row (anchor / transition bookkeeping) and do not run.
    Write(DreamScheduleRow),
    /// Nothing to do this poll.
    Idle,
}

// ── Storage ──────────────────────────────────────────────────────────────

/// Read the row; a missing row is the default (inherit config, never run).
pub fn load(store: &RocksDbStore) -> Result<DreamScheduleRow> {
    match store.get(DREAM_SCHEDULE_KEY)? {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode meta:dream_schedule"),
        None => Ok(DreamScheduleRow::default()),
    }
}

fn save(store: &RocksDbStore, row: &DreamScheduleRow) -> Result<()> {
    let bytes = serde_json::to_vec(row).context("encode meta:dream_schedule")?;
    store.put(DREAM_SCHEDULE_KEY, &bytes)
}

/// Process-wide writer lock: RocksDB puts are atomic per key, but the
/// handlers, the scheduler poll and the pass bookkeeping each do a
/// read-modify-write, and two of those interleaving would lose an update.
fn write_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Poisoning is non-fatal: the guard protects no data of its own, so
    // recover instead of panicking (CLAUDE.md: no unwrap in production paths).
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read-modify-write of the row under [`write_lock`]. The row is written
/// only when `mutate` changed it; the resulting row is returned either way.
pub fn update(
    store: &RocksDbStore,
    mutate: impl FnOnce(&mut DreamScheduleRow),
) -> Result<DreamScheduleRow> {
    let _guard = write_lock();
    let before = load(store)?;
    let mut row = before.clone();
    mutate(&mut row);
    if row != before {
        save(store, &row)?;
    }
    Ok(row)
}

// ── Pure schedule logic ──────────────────────────────────────────────────

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

/// Base of the next run: the later of `last_run_at` and `anchor_at`.
fn base(row: &DreamScheduleRow) -> Option<u64> {
    match (row.last_run_at, row.anchor_at) {
        (Some(run), Some(anchor)) => Some(run.max(anchor)),
        (run, anchor) => run.or(anchor),
    }
}

/// Epoch seconds of the next scheduled run: `base + interval`. `None` when
/// the schedule is disabled or no base exists yet (the scheduler anchors the
/// row on its first poll).
pub fn next_run_at(row: &DreamScheduleRow, eff: &EffectiveSchedule) -> Option<u64> {
    if !eff.enabled {
        return None;
    }
    base(row).map(|b| b.saturating_add(eff.interval_secs))
}

/// Due when a manual run was requested, or when the schedule is enabled and
/// `now` has reached `next_run_at`.
pub fn is_due(row: &DreamScheduleRow, eff: &EffectiveSchedule, now: u64) -> bool {
    if row.run_requested_at.is_some() {
        return true;
    }
    next_run_at(row, eff).is_some_and(|next| now >= next)
}

/// One scheduler poll, as a pure function of the row. In order:
/// 1. a pending manual request runs regardless of the schedule;
/// 2. a disabled schedule only records `last_seen_enabled = false` (once);
/// 3. an enabled schedule that was last seen disabled, or that has no base
///    yet, is anchored at `now` — no catch-up run;
/// 4. a row that never recorded `last_seen_enabled = true` records it;
/// 5. otherwise run when due.
pub fn decide(row: &DreamScheduleRow, eff: &EffectiveSchedule, now: u64) -> Decision {
    if row.run_requested_at.is_some() {
        return Decision::Run {
            trigger: TRIGGER_MANUAL,
        };
    }
    if !eff.enabled {
        return if row.last_seen_enabled == Some(false) {
            Decision::Idle
        } else {
            Decision::Write(DreamScheduleRow {
                last_seen_enabled: Some(false),
                ..row.clone()
            })
        };
    }
    let re_enabled = row.last_seen_enabled == Some(false);
    if re_enabled || base(row).is_none() {
        return Decision::Write(DreamScheduleRow {
            anchor_at: Some(now),
            last_seen_enabled: Some(true),
            ..row.clone()
        });
    }
    if row.last_seen_enabled != Some(true) {
        return Decision::Write(DreamScheduleRow {
            last_seen_enabled: Some(true),
            ..row.clone()
        });
    }
    if is_due(row, eff, now) {
        Decision::Run {
            trigger: TRIGGER_SCHEDULE,
        }
    } else {
        Decision::Idle
    }
}

/// One scheduler poll against storage: [`decide`] inside [`update`], so a
/// `Write` decision is applied atomically with the read it was based on.
pub fn poll(store: &RocksDbStore, cfg: &DreamConfig, now: u64) -> Result<Decision> {
    let mut decision = Decision::Idle;
    update(store, |row| {
        let eff = effective(row, cfg);
        decision = decide(row, &eff, now);
        if let Decision::Write(next) = &decision {
            *row = next.clone();
        }
    })?;
    Ok(decision)
}

/// Apply an admin override (`PUT /v1/dream/schedule`). A disabled → enabled
/// transition re-anchors at `now` so `next_run_at` is `now + interval`
/// rather than a time in the past; a row without any base is anchored too.
pub fn apply_override(
    row: &mut DreamScheduleRow,
    cfg: &DreamConfig,
    enabled: bool,
    interval_secs: u64,
    now: u64,
) {
    let was_enabled = effective(row, cfg).enabled;
    row.enabled = Some(enabled);
    row.interval_secs = Some(interval_secs);
    if enabled && (!was_enabled || base(row).is_none()) {
        row.anchor_at = Some(now);
    }
    row.last_seen_enabled = Some(enabled);
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

    fn seen(row: DreamScheduleRow, seen: Option<bool>) -> DreamScheduleRow {
        DreamScheduleRow {
            last_seen_enabled: seen,
            ..row
        }
    }

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
        let r = DreamScheduleRow {
            enabled: Some(false),
            interval_secs: Some(7_200),
            ..DreamScheduleRow::default()
        };
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
        let r = DreamScheduleRow {
            interval_secs: Some(3_600),
            ..DreamScheduleRow::default()
        };
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
    fn next_run_at_uses_the_later_of_anchor_and_last_run() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert_eq!(next_run_at(&row(Some(10), None), &eff), Some(110));
        assert_eq!(next_run_at(&row(None, Some(500)), &eff), Some(600));
        assert_eq!(next_run_at(&row(Some(10), Some(500)), &eff), Some(600));
        // Re-anchored after the last run: the anchor wins.
        assert_eq!(next_run_at(&row(Some(900), Some(500)), &eff), Some(1_000));
    }

    #[test]
    fn is_due_follows_interval_and_manual_request() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert!(!is_due(&row(None, None), &eff, 1_000));
        assert!(!is_due(&row(Some(10), None), &eff, 109));
        assert!(is_due(&row(Some(10), None), &eff, 110));

        let disabled = effective(&DreamScheduleRow::default(), &cfg(false, 100));
        assert!(!is_due(&row(Some(10), None), &disabled, 10_000));

        let requested = DreamScheduleRow {
            run_requested_at: Some(5),
            ..row(None, None)
        };
        assert!(is_due(&requested, &disabled, 6));
    }

    // ── decide ──

    #[test]
    fn decide_manual_request_runs_even_when_disabled() {
        let disabled = effective(&DreamScheduleRow::default(), &cfg(false, 100));
        let requested = DreamScheduleRow {
            run_requested_at: Some(5),
            ..row(None, None)
        };
        assert_eq!(
            decide(&requested, &disabled, 6),
            Decision::Run {
                trigger: TRIGGER_MANUAL
            }
        );
    }

    #[test]
    fn decide_disabled_records_last_seen_false_once() {
        let disabled = effective(&DreamScheduleRow::default(), &cfg(false, 100));
        let fresh = row(Some(10), None);
        assert_eq!(
            decide(&fresh, &disabled, 10_000),
            Decision::Write(seen(fresh.clone(), Some(false)))
        );
        assert_eq!(
            decide(&seen(fresh.clone(), Some(false)), &disabled, 10_000),
            Decision::Idle
        );
        // Previously enabled and overdue: still no run, just the transition.
        assert_eq!(
            decide(&seen(fresh.clone(), Some(true)), &disabled, 10_000),
            Decision::Write(seen(fresh, Some(false)))
        );
    }

    #[test]
    fn decide_first_poll_anchors_without_running() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        assert_eq!(
            decide(&row(None, None), &eff, 42),
            Decision::Write(seen(row(Some(42), None), Some(true)))
        );
    }

    #[test]
    fn decide_re_enable_re_anchors_instead_of_catching_up() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        // Last run long ago, schedule was disabled meanwhile, now enabled and
        // far overdue: re-anchor, no run.
        let stale = seen(row(Some(10), Some(20)), Some(false));
        assert_eq!(
            decide(&stale, &eff, 5_000),
            Decision::Write(DreamScheduleRow {
                anchor_at: Some(5_000),
                last_seen_enabled: Some(true),
                ..stale.clone()
            })
        );
        // Next poll: anchored at 5_000, interval 100 → not due until 5_100.
        let anchored = DreamScheduleRow {
            anchor_at: Some(5_000),
            last_seen_enabled: Some(true),
            ..stale
        };
        assert_eq!(decide(&anchored, &eff, 5_050), Decision::Idle);
        assert_eq!(
            decide(&anchored, &eff, 5_100),
            Decision::Run {
                trigger: TRIGGER_SCHEDULE
            }
        );
    }

    #[test]
    fn decide_legacy_row_records_last_seen_true_before_running() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        // A row written before `last_seen_enabled` existed: has a base, no
        // transition info. Record the flag; the following poll may run.
        let legacy = row(Some(10), None);
        assert_eq!(
            decide(&legacy, &eff, 10_000),
            Decision::Write(seen(legacy.clone(), Some(true)))
        );
        assert_eq!(
            decide(&seen(legacy, Some(true)), &eff, 10_000),
            Decision::Run {
                trigger: TRIGGER_SCHEDULE
            }
        );
    }

    #[test]
    fn decide_enabled_and_not_due_is_idle() {
        let eff = effective(&DreamScheduleRow::default(), &cfg(true, 100));
        let steady = seen(row(Some(10), Some(500)), Some(true));
        assert_eq!(decide(&steady, &eff, 599), Decision::Idle);
        assert_eq!(
            decide(&steady, &eff, 600),
            Decision::Run {
                trigger: TRIGGER_SCHEDULE
            }
        );
    }

    // ── apply_override ──

    #[test]
    fn apply_override_anchors_on_enable_and_records_disable() {
        let c = cfg(false, 86_400);
        let mut r = seen(row(Some(10), Some(20)), Some(false));
        apply_override(&mut r, &c, true, 7_200, 1_000);
        assert_eq!(r.enabled, Some(true));
        assert_eq!(r.interval_secs, Some(7_200));
        assert_eq!(r.anchor_at, Some(1_000), "disabled → enabled re-anchors");
        assert_eq!(r.last_seen_enabled, Some(true));
        assert_eq!(next_run_at(&r, &effective(&r, &c)), Some(8_200));

        // Enabled → enabled with a new interval keeps the base.
        apply_override(&mut r, &c, true, 3_600, 2_000);
        assert_eq!(r.anchor_at, Some(1_000));
        assert_eq!(next_run_at(&r, &effective(&r, &c)), Some(4_600));

        // Enabled → disabled records the transition, keeps the base.
        apply_override(&mut r, &c, false, 3_600, 3_000);
        assert_eq!(r.enabled, Some(false));
        assert_eq!(r.anchor_at, Some(1_000));
        assert_eq!(r.last_seen_enabled, Some(false));

        // No base at all: anchored on enable.
        let mut empty = DreamScheduleRow::default();
        apply_override(&mut empty, &cfg(true, 86_400), true, 86_400, 77);
        assert_eq!(empty.anchor_at, Some(77));
    }

    // ── storage ──

    #[test]
    fn load_missing_row_is_default_and_update_round_trips() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = open_store(&tmp);
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
                trigger: TRIGGER_SCHEDULE.to_string(),
                status: STATUS_OK.to_string(),
            }),
            last_seen_enabled: Some(true),
        };
        let written = update(&store, |r| *r = row.clone())?;
        assert_eq!(written, row);
        assert_eq!(load(&store)?, row);

        // A no-op mutation returns the stored row unchanged.
        assert_eq!(update(&store, |_| {})?, row);
        Ok(())
    }

    #[test]
    fn poll_applies_write_decisions_and_reports_runs() -> Result<()> {
        let tmp = TempDir::new()?;
        let store = open_store(&tmp);
        let c = cfg(true, 100);

        // First poll anchors.
        assert!(matches!(poll(&store, &c, 50)?, Decision::Write(_)));
        let row = load(&store)?;
        assert_eq!(row.anchor_at, Some(50));
        assert_eq!(row.last_seen_enabled, Some(true));

        // Not due yet, then due.
        assert_eq!(poll(&store, &c, 149)?, Decision::Idle);
        assert_eq!(
            poll(&store, &c, 150)?,
            Decision::Run {
                trigger: TRIGGER_SCHEDULE
            }
        );
        // A Run decision writes nothing: the pass marks the row itself.
        assert_eq!(load(&store)?, row);
        Ok(())
    }

    #[test]
    fn row_deserialises_from_empty_object_and_legacy_shape() {
        let row: DreamScheduleRow = serde_json::from_str("{}").expect("all fields default");
        assert_eq!(row, DreamScheduleRow::default());

        // A row written before `last_seen_enabled` / `status` existed.
        let legacy = r#"{"anchor_at": 5, "last_result": {"started_at": 1, "trigger": "schedule"}}"#;
        let row: DreamScheduleRow = serde_json::from_str(legacy).expect("legacy row parses");
        assert_eq!(row.anchor_at, Some(5));
        assert_eq!(row.last_seen_enabled, None);
        let result = row.last_result.expect("summary present");
        assert_eq!(result.status, "");
        assert_eq!(result.trigger, "schedule");
    }
}
