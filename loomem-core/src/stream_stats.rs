//! Per-stream statistics aggregation (brief: stream statistics endpoint).
//!
//! Produces an inventory/health snapshot of a stream's memory store —
//! chunk counts, level breakdown, retrieval readiness, fact-type / attribution
//! / trust-tier distributions, rolling activity, and extraction quality.
//! This is complementary to [`crate::stats_aggregator`], which tracks
//! *retrieval-quality* metrics (hit rate, MRR, freshness) rather than the
//! store's shape.
//!
//! **Privacy invariant:** this module MUST NOT emit any chunk content — only
//! aggregate counts, timestamps, and distributions. No field here reads or
//! returns `Chunk.content`, `Chunk.metadata`, or
//! `ExtractionMeta::original_content`. The only strings ever returned are the
//! stream id and static labels.
//!
//! **Sourcing (grounded in live state, not the brief's assumptions):**
//! - Levels are L0/L1 only — the L2 tier was removed 2026-05-09, so there is
//!   no `l2_count`.
//! - Activity windows, `last_ingest_at`/`last_search_at`, and extraction
//!   quality are derived from the append-only event log (read-only, no hot-path
//!   change). They are populated only when `[event_log].enabled = true`.
//! - Consolidation events carry no `stream_id`, so `runs_total` / `last_at` are
//!   engine-global; only `chunks_awaiting_consolidation` is per-stream.
//! - `extraction_failures` has no per-stream / per-window source (the LLM
//!   failure tracker is process-global with a ~1h window), so it is surfaced
//!   verbatim as an engine-global block, clearly labelled.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::event_log::{EventEntry, MemoryEvent};
use crate::llm_failures::LlmFailureCounts;
use crate::storage::{Chunk, FactType, RocksDbStore};

const DAY_SECS: u64 = 86_400;

/// Inputs that the caller (HTTP/MCP handler) resolves from config and the clock
/// and injects, keeping this module free of ambient state (testable clock).
#[derive(Debug, Clone, Copy)]
pub struct ComputeOpts {
    /// Current unix time (seconds). Injected so window math is deterministic in
    /// tests (CLAUDE.md: no system clock without injection).
    pub now: u64,
    /// `[worker.consolidation].min_chunks_to_consolidate` — surfaced alongside
    /// the awaiting backlog so a consumer can judge whether it is "over".
    pub min_chunks_to_consolidate: u64,
    /// Whether the event log is enabled; when false the activity / extraction
    /// windows are unpopulated and `meta.event_log_enabled` says so.
    pub event_log_enabled: bool,
}

// ── Response types (hierarchical, per brief) ─────────────────────────────

/// Full per-stream statistics snapshot. Every field is a count, timestamp, or
/// distribution — never chunk content.
#[derive(Debug, Clone, Serialize, Default)]
pub struct StreamStats {
    pub stream_id: String,
    pub health: HealthStats,
    pub retrieval: RetrievalStats,
    pub consolidation: ConsolidationStats,
    pub distribution: DistributionStats,
    pub activity: ActivityStats,
    pub extraction: ExtractionStats,
    pub meta: StatsMeta,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct HealthStats {
    /// Live chunks: `is_latest && deleted_at.is_none()`.
    pub memory_count: u64,
    /// Tombstoned chunks: `deleted_at.is_some()`.
    pub deleted_count: u64,
    /// Superseded-by-newer chunks: `!is_latest && deleted_at.is_none()`.
    pub superseded_count: u64,
    pub l0_count: u64,
    pub l1_count: u64,
    /// Oldest / newest live-chunk `timestamp` (unix secs).
    pub oldest_chunk_at: Option<u64>,
    pub newest_chunk_at: Option<u64>,
    /// Most recent Store / Search event for the stream (unix secs), from the
    /// event log. `None` when the log is disabled or has no such event.
    pub last_ingest_at: Option<u64>,
    pub last_search_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RetrievalStats {
    /// Live chunks that already carry a vector.
    pub embedded_count: u64,
    /// Live chunks still awaiting a vector (BM25-only until this reaches 0).
    pub embeddings_pending: u64,
    /// Docs for this stream in the BM25 (Tantivy) index. Filled by the caller
    /// (async index handle); `None` when unavailable.
    pub tantivy_indexed_count: Option<u64>,
    /// Chunks that failed to decode during this scan (corrupt / undecryptable).
    /// Scan-global: a failed decode cannot be attributed to a stream.
    pub undecodable_count: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConsolidationStats {
    /// Live L0 chunks not yet consolidated (`level == 0 && !consolidated`).
    pub chunks_awaiting_consolidation: u64,
    /// The configured threshold, for context on the backlog above.
    pub min_chunks_to_consolidate: u64,
    /// Engine-global consolidation-run count over the trailing 30d (matching
    /// the activity windows). Global because consolidation events carry no
    /// stream id.
    pub runs_total_global: u64,
    /// Engine-global timestamp of the most recent consolidation event within
    /// the trailing 30d.
    pub last_at_global: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DistributionStats {
    pub fact_types: FactTypeCounts,
    pub attribution: AttributionCounts,
    pub trust_tier: TrustTierCounts,
}

/// Fact-type histogram over live chunks. `unclassified` = live chunks with no
/// `extraction_meta`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct FactTypeCounts {
    pub preference_or_decision: u64,
    pub project_state: u64,
    pub fact: u64,
    pub event: u64,
    pub experience: u64,
    pub unclassified: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AttributionCounts {
    pub user_authored: u64,
    pub assistant_authored: u64,
    pub unattributed: u64,
}

/// Trust-tier histogram. `None`/unknown `trust_level` counts as `a1`
/// (`derive_trust_level` treats legacy/unknown as user-generated), so
/// `a1 + a2 + b == memory_count`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TrustTierCounts {
    pub a1: u64,
    pub a2: u64,
    pub b: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ActivityStats {
    pub ingests: WindowCounts,
    pub searches: WindowCounts,
}

/// Rolling event counts over the trailing 24h / 7d / 30d. The windows nest, so
/// `last_24h <= last_7d <= last_30d`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct WindowCounts {
    pub last_24h: u64,
    pub last_7d: u64,
    pub last_30d: u64,
}

impl WindowCounts {
    /// Record one event `age_secs` old across every window it falls into.
    fn bump(&mut self, age_secs: u64) {
        if age_secs <= 30 * DAY_SECS {
            self.last_30d += 1;
            if age_secs <= 7 * DAY_SECS {
                self.last_7d += 1;
                if age_secs <= DAY_SECS {
                    self.last_24h += 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ExtractionStats {
    pub avg_facts_per_ingest_24h: f64,
    pub avg_facts_per_ingest_7d: f64,
    pub empty_extractions_24h: u64,
    pub empty_extractions_7d: u64,
    /// Engine-global LLM failure counts over the tracker's window (~1h,
    /// in-memory, not per-stream). The only faithful source for extraction
    /// failures; labelled global rather than fabricated per-stream.
    pub llm_failures_recent_global: LlmFailureCounts,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StatsMeta {
    /// `opts.now` echoed back — the instant this snapshot describes.
    pub generated_at: u64,
    /// When false, `activity` and the per-stream `extraction` windows are 0
    /// because the event log is off.
    pub event_log_enabled: bool,
    /// Chunk rows scanned to build this snapshot (cost visibility).
    pub scanned_rows: u64,
}

/// Admin all-streams response: one entry per stream plus a `_total` aggregate.
#[derive(Debug, Clone, Serialize)]
pub struct AllStreamStats {
    pub streams: HashMap<String, StreamStats>,
    #[serde(rename = "_total")]
    pub total: StreamStats,
}

// ── Chunk-scan accumulator ───────────────────────────────────────────────

/// Per-stream tallies gathered from one pass over the chunk store.
#[derive(Debug, Default, Clone)]
struct ChunkAcc {
    memory_count: u64,
    deleted_count: u64,
    superseded_count: u64,
    l0_count: u64,
    l1_count: u64,
    embedded_count: u64,
    awaiting_consolidation: u64,
    oldest_chunk_at: Option<u64>,
    newest_chunk_at: Option<u64>,
    fact_types: FactTypeCounts,
    attribution: AttributionCounts,
    trust_tier: TrustTierCounts,
}

impl ChunkAcc {
    fn merge(&mut self, o: &ChunkAcc) {
        self.memory_count += o.memory_count;
        self.deleted_count += o.deleted_count;
        self.superseded_count += o.superseded_count;
        self.l0_count += o.l0_count;
        self.l1_count += o.l1_count;
        self.embedded_count += o.embedded_count;
        self.awaiting_consolidation += o.awaiting_consolidation;
        self.oldest_chunk_at = min_opt(self.oldest_chunk_at, o.oldest_chunk_at);
        self.newest_chunk_at = max_opt(self.newest_chunk_at, o.newest_chunk_at);
        merge_fact_types(&mut self.fact_types, &o.fact_types);
        merge_attribution(&mut self.attribution, &o.attribution);
        merge_trust(&mut self.trust_tier, &o.trust_tier);
    }
}

/// Fold one chunk into its stream accumulator. Pure (no storage), so the
/// distribution/health logic is unit-testable without a RocksDB instance.
/// `has_embedding` is resolved by the caller (a cheap column-family probe).
fn accumulate_chunk(acc: &mut ChunkAcc, chunk: &Chunk, has_embedding: bool) {
    let live = chunk.is_latest && chunk.deleted_at.is_none();
    if chunk.deleted_at.is_some() {
        acc.deleted_count += 1;
        return;
    }
    if !chunk.is_latest {
        acc.superseded_count += 1;
        return;
    }
    // Live chunk from here on.
    debug_assert!(live);
    acc.memory_count += 1;
    if chunk.level <= 0 {
        acc.l0_count += 1;
        if !chunk.consolidated {
            acc.awaiting_consolidation += 1;
        }
    } else {
        acc.l1_count += 1;
    }
    if has_embedding {
        acc.embedded_count += 1;
    }
    acc.oldest_chunk_at = min_opt(acc.oldest_chunk_at, Some(chunk.timestamp));
    acc.newest_chunk_at = max_opt(acc.newest_chunk_at, Some(chunk.timestamp));
    bump_fact_type(&mut acc.fact_types, chunk);
    bump_attribution(&mut acc.attribution, chunk);
    bump_trust(&mut acc.trust_tier, chunk);
}

fn bump_fact_type(c: &mut FactTypeCounts, chunk: &Chunk) {
    match chunk.extraction_meta.as_ref().map(|m| &m.fact_type) {
        Some(FactType::PreferenceOrDecision) => c.preference_or_decision += 1,
        Some(FactType::ProjectState) => c.project_state += 1,
        Some(FactType::Fact) => c.fact += 1,
        Some(FactType::Event) => c.event += 1,
        Some(FactType::Experience) => c.experience += 1,
        None => c.unclassified += 1,
    }
}

fn bump_attribution(c: &mut AttributionCounts, chunk: &Chunk) {
    let who = chunk
        .extraction_meta
        .as_ref()
        .and_then(|m| m.attributed_to.as_deref())
        .map(str::to_ascii_lowercase);
    match who.as_deref() {
        Some("user") => c.user_authored += 1,
        Some("assistant") => c.assistant_authored += 1,
        _ => c.unattributed += 1,
    }
}

fn bump_trust(c: &mut TrustTierCounts, chunk: &Chunk) {
    // None/unknown → a1 (legacy = user-generated), keeping the tiers summing
    // to memory_count.
    match chunk.trust_level.as_deref() {
        Some("a2") => c.a2 += 1,
        Some("b") => c.b += 1,
        _ => c.a1 += 1,
    }
}

fn merge_fact_types(a: &mut FactTypeCounts, o: &FactTypeCounts) {
    a.preference_or_decision += o.preference_or_decision;
    a.project_state += o.project_state;
    a.fact += o.fact;
    a.event += o.event;
    a.experience += o.experience;
    a.unclassified += o.unclassified;
}

fn merge_attribution(a: &mut AttributionCounts, o: &AttributionCounts) {
    a.user_authored += o.user_authored;
    a.assistant_authored += o.assistant_authored;
    a.unattributed += o.unattributed;
}

fn merge_trust(a: &mut TrustTierCounts, o: &TrustTierCounts) {
    a.a1 += o.a1;
    a.a2 += o.a2;
    a.b += o.b;
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Result of one chunk-store pass: per-stream tallies + scan-wide counters that
/// cannot be attributed to a stream.
struct ChunkScan {
    per_stream: HashMap<String, ChunkAcc>,
    undecodable: u64,
    scanned_rows: u64,
}

/// Single pass over `chunk:L0:` + `chunk:L1:`, bucketed by stream. When
/// `only` is `Some`, non-matching streams are skipped (single-stream path);
/// when `None`, every stream is bucketed (admin all-streams path) — either way
/// the store is scanned exactly once.
fn scan_chunks(store: &RocksDbStore, only: Option<&str>) -> ChunkScan {
    let mut per_stream: HashMap<String, ChunkAcc> = HashMap::new();
    let mut undecodable = 0u64;
    let mut scanned_rows = 0u64;
    for level in ["chunk:L0:", "chunk:L1:"] {
        for (_key, value) in store.prefix_scan(level.as_bytes()) {
            scanned_rows += 1;
            let chunk = match store.decode_chunk(&value) {
                Ok(c) => c,
                Err(_) => {
                    undecodable += 1;
                    continue;
                }
            };
            if let Some(target) = only {
                if chunk.stream != target {
                    continue;
                }
            }
            let has_emb = store.has_embedding(&chunk.id).unwrap_or(false);
            let acc = per_stream.entry(chunk.stream.clone()).or_default();
            accumulate_chunk(acc, &chunk, has_emb);
        }
    }
    ChunkScan {
        per_stream,
        undecodable,
        scanned_rows,
    }
}

// ── Event-log scan (activity + extraction + global consolidation) ────────

/// Per-stream rolling activity + extraction tallies from the event log.
#[derive(Debug, Default, Clone)]
struct EventAcc {
    ingests: WindowCounts,
    searches: WindowCounts,
    last_ingest_at: Option<u64>,
    last_search_at: Option<u64>,
    facts_24h: u64,
    ingests_24h: u64,
    empty_24h: u64,
    facts_7d: u64,
    ingests_7d: u64,
    empty_7d: u64,
}

impl EventAcc {
    fn merge(&mut self, o: &EventAcc) {
        self.ingests.last_24h += o.ingests.last_24h;
        self.ingests.last_7d += o.ingests.last_7d;
        self.ingests.last_30d += o.ingests.last_30d;
        self.searches.last_24h += o.searches.last_24h;
        self.searches.last_7d += o.searches.last_7d;
        self.searches.last_30d += o.searches.last_30d;
        self.last_ingest_at = max_opt(self.last_ingest_at, o.last_ingest_at);
        self.last_search_at = max_opt(self.last_search_at, o.last_search_at);
        self.facts_24h += o.facts_24h;
        self.ingests_24h += o.ingests_24h;
        self.empty_24h += o.empty_24h;
        self.facts_7d += o.facts_7d;
        self.ingests_7d += o.ingests_7d;
        self.empty_7d += o.empty_7d;
    }

    fn record_ingest(&mut self, age: u64, chunk_count: u64) {
        self.ingests.bump(age);
        if age <= 7 * DAY_SECS {
            self.facts_7d += chunk_count;
            self.ingests_7d += 1;
            if chunk_count == 0 {
                self.empty_7d += 1;
            }
            if age <= DAY_SECS {
                self.facts_24h += chunk_count;
                self.ingests_24h += 1;
                if chunk_count == 0 {
                    self.empty_24h += 1;
                }
            }
        }
    }
}

/// Engine-global consolidation tallies (events carry no stream id).
#[derive(Debug, Default, Clone)]
struct GlobalConsolidation {
    runs_total: u64,
    last_at: Option<u64>,
}

struct EventScan {
    per_stream: HashMap<String, EventAcc>,
    consolidation: GlobalConsolidation,
}

/// Read every retained `*.jsonl` in `events_dir` once, bucketing Store/Search
/// events by stream within the 30d window and counting consolidation events
/// globally. Read-only — never touches the hot write path. Unparseable lines
/// and files are skipped rather than failing the whole snapshot.
fn scan_events(events_dir: &Path, now: u64, only: Option<&str>) -> EventScan {
    let mut per_stream: HashMap<String, EventAcc> = HashMap::new();
    let mut consolidation = GlobalConsolidation::default();
    let Ok(dir) = std::fs::read_dir(events_dir) else {
        return EventScan {
            per_stream,
            consolidation,
        };
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl") != Some(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(e) = serde_json::from_str::<EventEntry>(line) {
                fold_event(&mut per_stream, &mut consolidation, &e, now, only);
            }
        }
    }
    EventScan {
        per_stream,
        consolidation,
    }
}

fn fold_event(
    per_stream: &mut HashMap<String, EventAcc>,
    consolidation: &mut GlobalConsolidation,
    entry: &EventEntry,
    now: u64,
    only: Option<&str>,
) {
    // Drop future-dated events (clock skew or a corrupt log line): otherwise
    // `saturating_sub` gives them age 0, counting them in every window and
    // letting them set `last_*` to a timestamp that has not happened yet.
    if entry.timestamp > now {
        return;
    }
    let age = now.saturating_sub(entry.timestamp);
    match &entry.event {
        MemoryEvent::Store {
            stream_id,
            chunk_count,
            ..
        } if only.is_none_or(|f| f == stream_id) => {
            let acc = per_stream.entry(stream_id.clone()).or_default();
            acc.record_ingest(age, u64::try_from(*chunk_count).unwrap_or(u64::MAX));
            acc.last_ingest_at = max_opt(acc.last_ingest_at, Some(entry.timestamp));
        }
        MemoryEvent::Search { stream_id, .. } if only.is_none_or(|f| f == stream_id) => {
            let acc = per_stream.entry(stream_id.clone()).or_default();
            acc.searches.bump(age);
            acc.last_search_at = max_opt(acc.last_search_at, Some(entry.timestamp));
        }
        // Bound consolidation to the same 30d horizon as the activity windows,
        // so `runs_total`/`last_at` never include runs from older rotated logs
        // that fall outside the reported window.
        MemoryEvent::Consolidation { .. } if age <= 30 * DAY_SECS => {
            consolidation.runs_total += 1;
            consolidation.last_at = max_opt(consolidation.last_at, Some(entry.timestamp));
        }
        _ => {}
    }
}

// ── Assembly ─────────────────────────────────────────────────────────────

fn avg(sum: u64, n: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    // truncation intentional: integer counts widened to f64 for a mean; any
    // precision loss past 2^53 is irrelevant to a facts-per-ingest average.
    let (sum, n) = (sum as f64, n as f64);
    sum / n
}

/// Combine a stream's chunk + event tallies with engine-global figures into the
/// public snapshot.
fn assemble(
    stream_id: String,
    chunk: &ChunkAcc,
    event: &EventAcc,
    consolidation: &GlobalConsolidation,
    undecodable: u64,
    scanned_rows: u64,
    opts: &ComputeOpts,
) -> StreamStats {
    StreamStats {
        stream_id,
        health: HealthStats {
            memory_count: chunk.memory_count,
            deleted_count: chunk.deleted_count,
            superseded_count: chunk.superseded_count,
            l0_count: chunk.l0_count,
            l1_count: chunk.l1_count,
            oldest_chunk_at: chunk.oldest_chunk_at,
            newest_chunk_at: chunk.newest_chunk_at,
            last_ingest_at: event.last_ingest_at,
            last_search_at: event.last_search_at,
        },
        retrieval: RetrievalStats {
            embedded_count: chunk.embedded_count,
            embeddings_pending: chunk.memory_count.saturating_sub(chunk.embedded_count),
            tantivy_indexed_count: None,
            undecodable_count: undecodable,
        },
        consolidation: ConsolidationStats {
            chunks_awaiting_consolidation: chunk.awaiting_consolidation,
            min_chunks_to_consolidate: opts.min_chunks_to_consolidate,
            runs_total_global: consolidation.runs_total,
            last_at_global: consolidation.last_at,
        },
        distribution: DistributionStats {
            fact_types: chunk.fact_types.clone(),
            attribution: chunk.attribution.clone(),
            trust_tier: chunk.trust_tier.clone(),
        },
        activity: ActivityStats {
            ingests: event.ingests.clone(),
            searches: event.searches.clone(),
        },
        extraction: ExtractionStats {
            avg_facts_per_ingest_24h: avg(event.facts_24h, event.ingests_24h),
            avg_facts_per_ingest_7d: avg(event.facts_7d, event.ingests_7d),
            empty_extractions_24h: event.empty_24h,
            empty_extractions_7d: event.empty_7d,
            llm_failures_recent_global: crate::llm_failures::global().recent(),
        },
        meta: StatsMeta {
            generated_at: opts.now,
            event_log_enabled: opts.event_log_enabled,
            scanned_rows,
        },
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Statistics for a single stream. One chunk-store pass + one event-log pass,
/// both filtered to `stream_id`.
pub fn compute_stream(
    store: &RocksDbStore,
    events_dir: &Path,
    opts: &ComputeOpts,
    stream_id: &str,
) -> Result<StreamStats> {
    let scan = scan_chunks(store, Some(stream_id));
    let events = if opts.event_log_enabled {
        scan_events(events_dir, opts.now, Some(stream_id))
    } else {
        EventScan {
            per_stream: HashMap::new(),
            consolidation: GlobalConsolidation::default(),
        }
    };
    let chunk = scan.per_stream.get(stream_id).cloned().unwrap_or_default();
    let event = events
        .per_stream
        .get(stream_id)
        .cloned()
        .unwrap_or_default();
    Ok(assemble(
        stream_id.to_string(),
        &chunk,
        &event,
        &events.consolidation,
        scan.undecodable,
        scan.scanned_rows,
        opts,
    ))
}

/// Statistics for every stream plus a `_total` aggregate. One chunk-store pass
/// and one event-log pass regardless of stream count.
pub fn compute_all(
    store: &RocksDbStore,
    events_dir: &Path,
    opts: &ComputeOpts,
) -> Result<AllStreamStats> {
    let scan = scan_chunks(store, None);
    let events = if opts.event_log_enabled {
        scan_events(events_dir, opts.now, None)
    } else {
        EventScan {
            per_stream: HashMap::new(),
            consolidation: GlobalConsolidation::default(),
        }
    };

    let ids: std::collections::BTreeSet<String> = scan
        .per_stream
        .keys()
        .chain(events.per_stream.keys())
        .cloned()
        .collect();

    let mut streams = HashMap::new();
    let mut total_chunk = ChunkAcc::default();
    let mut total_event = EventAcc::default();
    for id in ids {
        let chunk = scan.per_stream.get(&id).cloned().unwrap_or_default();
        let event = events.per_stream.get(&id).cloned().unwrap_or_default();
        total_chunk.merge(&chunk);
        total_event.merge(&event);
        streams.insert(
            id.clone(),
            assemble(
                id,
                &chunk,
                &event,
                &events.consolidation,
                scan.undecodable,
                scan.scanned_rows,
                opts,
            ),
        );
    }

    let total = assemble(
        "_total".to_string(),
        &total_chunk,
        &total_event,
        &events.consolidation,
        scan.undecodable,
        scan.scanned_rows,
        opts,
    );
    Ok(AllStreamStats { streams, total })
}

/// Render a [`StreamStats`] as compact, section-grouped text for the MCP
/// `memory_stats` tool. Emits only numbers, timestamps, and static labels —
/// never chunk content (privacy invariant).
pub fn render_text(s: &StreamStats) -> String {
    let h = &s.health;
    let r = &s.retrieval;
    let c = &s.consolidation;
    let ft = &s.distribution.fact_types;
    let at = &s.distribution.attribution;
    let tt = &s.distribution.trust_tier;
    let ing = &s.activity.ingests;
    let sea = &s.activity.searches;
    let ex = &s.extraction;
    let lf = &ex.llm_failures_recent_global;
    let ts = |v: Option<u64>| v.map_or_else(|| "n/a".to_string(), |t| t.to_string());
    let bm25 = r
        .tantivy_indexed_count
        .map_or_else(|| "n/a".to_string(), |v| v.to_string());

    format!(
        "Loomem Stats — stream {stream}\n\
         [health] live={live} deleted={deleted} superseded={sup} L0={l0} L1={l1} oldest={oldest} newest={newest} last_ingest={li} last_search={ls}\n\
         [retrieval] embedded={emb} pending={pend} bm25_indexed={bm25} undecodable={undec}\n\
         [consolidation] awaiting_L0={awaiting} threshold={thr} runs_total(global)={runs} last(global)={clast}\n\
         [fact_types] preference_or_decision={p} project_state={ps} fact={f} event={ev} experience={xp} unclassified={un}\n\
         [attribution] user={u} assistant={a} unattributed={ua}\n\
         [trust] a1={a1} a2={a2} b={b}\n\
         [activity] ingests 24h/7d/30d={i1}/{i7}/{i30} searches 24h/7d/30d={s1}/{s7}/{s30}\n\
         [extraction] avg_facts/ingest 24h/7d={af1:.2}/{af7:.2} empty 24h/7d={e1}/{e7} llm_failures(global,~{win}m) extraction={xf} ner={nf} embedding={ef} consolidation={cf} empty_extractions={ee}\n\
         [meta] event_log_enabled={elog} scanned_rows={rows} generated_at={gen}",
        stream = s.stream_id,
        live = h.memory_count, deleted = h.deleted_count, sup = h.superseded_count,
        l0 = h.l0_count, l1 = h.l1_count,
        oldest = ts(h.oldest_chunk_at), newest = ts(h.newest_chunk_at),
        li = ts(h.last_ingest_at), ls = ts(h.last_search_at),
        emb = r.embedded_count, pend = r.embeddings_pending, undec = r.undecodable_count,
        awaiting = c.chunks_awaiting_consolidation, thr = c.min_chunks_to_consolidate,
        runs = c.runs_total_global, clast = ts(c.last_at_global),
        p = ft.preference_or_decision, ps = ft.project_state, f = ft.fact, ev = ft.event, xp = ft.experience, un = ft.unclassified,
        u = at.user_authored, a = at.assistant_authored, ua = at.unattributed,
        a1 = tt.a1, a2 = tt.a2, b = tt.b,
        i1 = ing.last_24h, i7 = ing.last_7d, i30 = ing.last_30d,
        s1 = sea.last_24h, s7 = sea.last_7d, s30 = sea.last_30d,
        af1 = ex.avg_facts_per_ingest_24h, af7 = ex.avg_facts_per_ingest_7d,
        e1 = ex.empty_extractions_24h, e7 = ex.empty_extractions_7d,
        win = lf.window_secs / 60,
        xf = lf.extraction, nf = lf.ner, ef = lf.embedding, cf = lf.consolidation, ee = lf.extraction_empty,
        elog = s.meta.event_log_enabled, rows = s.meta.scanned_rows, gen = s.meta.generated_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ExtractionMeta, ProvenanceRole, RocksDbConfig};
    use std::io::Write;
    use tempfile::TempDir;

    fn base_chunk(id: &str, stream: &str, level: i32) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: "x".to_string(),
            stream: stream.to_string(),
            level,
            score: 1.0,
            timestamp: 1000,
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
            access_count: 0,
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
            provenance_role: ProvenanceRole::Claim,
        }
    }

    fn meta_with(fact: FactType, attributed: Option<&str>) -> ExtractionMeta {
        ExtractionMeta {
            fact_type: fact,
            subject: None,
            event_date: None,
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: 1.0,
            extracted_from: None,
            extraction_model: None,
            original_content: None,
            topic: None,
            attributed_to: attributed.map(str::to_string),
        }
    }

    // ── health section ──
    #[test]
    fn health_counts_live_deleted_superseded_and_levels() {
        let mut acc = ChunkAcc::default();
        let mut live0 = base_chunk("a", "s", 0);
        live0.timestamp = 500;
        accumulate_chunk(&mut acc, &live0, true);
        let mut live1 = base_chunk("b", "s", 1);
        live1.timestamp = 900;
        accumulate_chunk(&mut acc, &live1, false);
        let mut deleted = base_chunk("c", "s", 0);
        deleted.deleted_at = Some(123);
        accumulate_chunk(&mut acc, &deleted, false);
        let mut superseded = base_chunk("d", "s", 0);
        superseded.is_latest = false;
        accumulate_chunk(&mut acc, &superseded, false);

        assert_eq!(acc.memory_count, 2);
        assert_eq!(acc.deleted_count, 1);
        assert_eq!(acc.superseded_count, 1);
        assert_eq!(acc.l0_count, 1);
        assert_eq!(acc.l1_count, 1);
        assert_eq!(acc.oldest_chunk_at, Some(500));
        assert_eq!(acc.newest_chunk_at, Some(900));
    }

    // ── retrieval section ──
    #[test]
    fn retrieval_embedded_and_pending_from_scan() {
        let dir = TempDir::new().unwrap();
        let store = RocksDbStore::open(
            dir.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap();
        store.store_chunk(&base_chunk("a", "s", 0)).unwrap();
        store.store_chunk(&base_chunk("b", "s", 0)).unwrap();
        store.store_embedding("a", vec![0.1; 8]).unwrap();

        let opts = ComputeOpts {
            now: 10_000,
            min_chunks_to_consolidate: 3,
            event_log_enabled: false,
        };
        let stats = compute_stream(&store, dir.path(), &opts, "s").unwrap();
        assert_eq!(stats.retrieval.embedded_count, 1);
        assert_eq!(stats.retrieval.embeddings_pending, 1);
        assert_eq!(stats.health.memory_count, 2);
    }

    // ── consolidation section ──
    #[test]
    fn consolidation_awaiting_counts_unconsolidated_l0_only() {
        let mut acc = ChunkAcc::default();
        accumulate_chunk(&mut acc, &base_chunk("a", "s", 0), false); // L0, !consolidated
        let mut done = base_chunk("b", "s", 0);
        done.consolidated = true;
        accumulate_chunk(&mut acc, &done, false); // L0, consolidated → not awaiting
        accumulate_chunk(&mut acc, &base_chunk("c", "s", 1), false); // L1 → not awaiting
        assert_eq!(acc.awaiting_consolidation, 1);
    }

    // ── distribution section ──
    #[test]
    fn distribution_fact_type_attribution_trust() {
        let mut acc = ChunkAcc::default();
        let mut a = base_chunk("a", "s", 0);
        a.extraction_meta = Some(meta_with(FactType::Fact, Some("user")));
        a.trust_level = Some("a1".to_string());
        accumulate_chunk(&mut acc, &a, false);
        let mut b = base_chunk("b", "s", 0);
        b.extraction_meta = Some(meta_with(FactType::Event, Some("assistant")));
        b.trust_level = Some("a2".to_string());
        accumulate_chunk(&mut acc, &b, false);
        let c = base_chunk("c", "s", 0); // no meta, no trust → unclassified/unattributed/a1
        accumulate_chunk(&mut acc, &c, false);

        assert_eq!(acc.fact_types.fact, 1);
        assert_eq!(acc.fact_types.event, 1);
        assert_eq!(acc.fact_types.unclassified, 1);
        assert_eq!(acc.attribution.user_authored, 1);
        assert_eq!(acc.attribution.assistant_authored, 1);
        assert_eq!(acc.attribution.unattributed, 1);
        assert_eq!(acc.trust_tier.a1, 2); // explicit a1 + None default
        assert_eq!(acc.trust_tier.a2, 1);
        // Tiers sum to the live count.
        assert_eq!(
            acc.trust_tier.a1 + acc.trust_tier.a2 + acc.trust_tier.b,
            acc.memory_count
        );
    }

    // ── activity + extraction sections ──
    fn write_events(dir: &Path, lines: &[&str]) {
        let mut f = std::fs::File::create(dir.join("events.jsonl")).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn activity_windows_and_extraction_from_event_log() {
        let dir = TempDir::new().unwrap();
        let now = 100 * DAY_SECS;
        // Two ingests (one empty), one search, all within 24h; one old ingest
        // (10d ago → only in 30d window); one consolidation (global).
        let recent = now - 100;
        let old = now - 10 * DAY_SECS;
        write_events(
            dir.path(),
            &[
                &format!(
                    r#"{{"timestamp":{recent},"event":{{"type":"store","content_len":10,"chunk_count":3,"stream_id":"s","source":"api"}}}}"#
                ),
                &format!(
                    r#"{{"timestamp":{recent},"event":{{"type":"store","content_len":0,"chunk_count":0,"stream_id":"s","source":"api"}}}}"#
                ),
                &format!(
                    r#"{{"timestamp":{recent},"event":{{"type":"search","query":"q","stream_id":"s","top_scores":[0.9],"latency_ms":5,"result_count":1}}}}"#
                ),
                &format!(
                    r#"{{"timestamp":{old},"event":{{"type":"store","content_len":10,"chunk_count":2,"stream_id":"s","source":"api"}}}}"#
                ),
                &format!(
                    r#"{{"timestamp":{recent},"event":{{"type":"consolidation","input_count":5,"output_count":1,"dropped_ids":[],"cost_usd":0.01}}}}"#
                ),
            ],
        );

        let scan = scan_events(dir.path(), now, Some("s"));
        let acc = scan.per_stream.get("s").unwrap();
        assert_eq!(acc.ingests.last_24h, 2);
        assert_eq!(acc.ingests.last_7d, 2);
        assert_eq!(acc.ingests.last_30d, 3, "10d-old ingest lands in 30d only");
        assert_eq!(acc.searches.last_24h, 1);
        assert_eq!(acc.last_ingest_at, Some(recent));
        assert_eq!(acc.empty_24h, 1);
        assert_eq!(acc.facts_24h, 3);
        assert_eq!(acc.ingests_24h, 2);
        // avg over 24h = 3 facts / 2 ingests = 1.5
        assert!((avg(acc.facts_24h, acc.ingests_24h) - 1.5).abs() < 1e-9);
        assert_eq!(scan.consolidation.runs_total, 1);
        assert_eq!(scan.consolidation.last_at, Some(recent));
    }

    #[test]
    fn event_scan_filters_by_stream() {
        let dir = TempDir::new().unwrap();
        let now = 100 * DAY_SECS;
        write_events(
            dir.path(),
            &[
                &format!(
                    r#"{{"timestamp":{now},"event":{{"type":"store","content_len":10,"chunk_count":1,"stream_id":"s1","source":"api"}}}}"#
                ),
                &format!(
                    r#"{{"timestamp":{now},"event":{{"type":"store","content_len":10,"chunk_count":1,"stream_id":"s2","source":"api"}}}}"#
                ),
            ],
        );
        let scan = scan_events(dir.path(), now, Some("s1"));
        assert!(scan.per_stream.contains_key("s1"));
        assert!(!scan.per_stream.contains_key("s2"));
    }

    #[test]
    fn future_events_dropped_and_old_consolidation_excluded() {
        let dir = TempDir::new().unwrap();
        let now = 100 * DAY_SECS;
        let future = now + 1000;
        let old = now - 40 * DAY_SECS;
        let recent = now - 100;
        write_events(
            dir.path(),
            &[
                // future-dated ingest → dropped entirely (no window, no last_*)
                &format!(
                    r#"{{"timestamp":{future},"event":{{"type":"store","content_len":10,"chunk_count":9,"stream_id":"s","source":"api"}}}}"#
                ),
                // consolidation 40d ago → outside the 30d horizon, excluded
                &format!(
                    r#"{{"timestamp":{old},"event":{{"type":"consolidation","input_count":5,"output_count":1,"dropped_ids":[],"cost_usd":0.0}}}}"#
                ),
                // consolidation just now → counted
                &format!(
                    r#"{{"timestamp":{recent},"event":{{"type":"consolidation","input_count":5,"output_count":1,"dropped_ids":[],"cost_usd":0.0}}}}"#
                ),
            ],
        );
        let scan = scan_events(dir.path(), now, None);
        assert!(
            scan.per_stream
                .get("s")
                .is_none_or(|a| a.ingests.last_30d == 0 && a.last_ingest_at.is_none()),
            "future ingest must not appear in any window or last_ingest_at"
        );
        assert_eq!(
            scan.consolidation.runs_total, 1,
            "only the recent run counts"
        );
        assert_eq!(scan.consolidation.last_at, Some(recent));
    }

    // ── text rendering (MCP memory_stats) ──
    #[test]
    fn render_text_has_sections_and_numbers() {
        let mut s = StreamStats {
            stream_id: "s1".to_string(),
            ..Default::default()
        };
        s.health.memory_count = 7;
        s.retrieval.tantivy_indexed_count = Some(5);
        let text = render_text(&s);
        assert!(text.contains("stream s1"));
        assert!(text.contains("[health]") && text.contains("live=7"));
        assert!(text.contains("[retrieval]") && text.contains("bm25_indexed=5"));
        // None timestamps render as n/a, not a panic.
        assert!(text.contains("oldest=n/a"));
    }

    // ── all-streams aggregation + _total ──
    #[test]
    fn compute_all_buckets_streams_and_totals() {
        let dir = TempDir::new().unwrap();
        let store = RocksDbStore::open(
            dir.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap();
        store.store_chunk(&base_chunk("a", "s1", 0)).unwrap();
        store.store_chunk(&base_chunk("b", "s2", 0)).unwrap();
        store.store_chunk(&base_chunk("c", "s2", 1)).unwrap();

        let opts = ComputeOpts {
            now: 10_000,
            min_chunks_to_consolidate: 3,
            event_log_enabled: false,
        };
        let all = compute_all(&store, dir.path(), &opts).unwrap();
        assert_eq!(all.streams.get("s1").unwrap().health.memory_count, 1);
        assert_eq!(all.streams.get("s2").unwrap().health.memory_count, 2);
        assert_eq!(all.total.health.memory_count, 3);
        assert_eq!(all.total.health.l1_count, 1);
    }
}
