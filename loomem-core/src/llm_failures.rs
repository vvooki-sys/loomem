//! Sliding-window LLM failure counters (/157 S3).
//!
//! Background workers (extraction, NER queue, embedding, dream/consolidation)
//! degrade gracefully on LLM errors — warn + continue — which made the
//! 2026-06-11 quota outage invisible until ingest was tested by hand.
//! This module gives those paths a process-wide, windowed failure counter
//! surfaced by `/v1/status` and MCP `memory_status` (`llm_failures_recent`),
//! mirroring the /150b drop-counter pattern (`event_log::emit_drop_count`).

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Window over which failures count as "recent" (brief /157 S3: ~1h).
pub const WINDOW: Duration = Duration::from_secs(3600);

/// Per-category cap — bounds memory during sustained failure storms.
const MAX_EVENTS_PER_KIND: usize = 10_000;

/// Which LLM-backed pipeline failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFailureKind {
    /// Knowledge extraction (`memory_extractor`).
    Extraction,
    /// Entity extraction / NER queue (`llm_ner` via `entity_extraction_queue`).
    Ner,
    /// Embedding API (`llm::embed` / `llm::embed_batch`).
    Embedding,
    /// Dream / consolidation chat calls (`dream`, `llm::compress`).
    Consolidation,
}

/// Snapshot of recent failure counts, serialized into status payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LlmFailureCounts {
    pub extraction: usize,
    pub ner: usize,
    pub embedding: usize,
    pub consolidation: usize,
    /// Length of the sliding window the counts cover, for self-describing JSON.
    pub window_secs: u64,
}

impl LlmFailureCounts {
    pub fn total(&self) -> usize {
        self.extraction + self.ner + self.embedding + self.consolidation
    }
}

#[derive(Default)]
struct Inner {
    extraction: VecDeque<Instant>,
    ner: VecDeque<Instant>,
    embedding: VecDeque<Instant>,
    consolidation: VecDeque<Instant>,
}

impl Inner {
    fn deque_mut(&mut self, kind: LlmFailureKind) -> &mut VecDeque<Instant> {
        match kind {
            LlmFailureKind::Extraction => &mut self.extraction,
            LlmFailureKind::Ner => &mut self.ner,
            LlmFailureKind::Embedding => &mut self.embedding,
            LlmFailureKind::Consolidation => &mut self.consolidation,
        }
    }
}

/// Windowed failure tracker. Use [`global`] for the process-wide instance.
#[derive(Default)]
pub struct LlmFailureTracker {
    inner: Mutex<Inner>,
}

fn prune(deque: &mut VecDeque<Instant>, now: Instant) {
    while let Some(front) = deque.front() {
        if now.saturating_duration_since(*front) > WINDOW {
            deque.pop_front();
        } else {
            break;
        }
    }
}

impl LlmFailureTracker {
    /// Record one failure of `kind` at the current instant.
    pub fn record(&self, kind: LlmFailureKind) {
        self.record_at(kind, Instant::now());
    }

    /// Counts within the window ending now.
    pub fn recent(&self) -> LlmFailureCounts {
        self.counts_at(Instant::now())
    }

    /// Poison recovery: the guarded data is plain counters, safe to reuse
    /// after a panicked holder (documented §2 rationale — no unwrap).
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Deterministic core of [`Self::record`] (tests pass synthetic instants).
    pub fn record_at(&self, kind: LlmFailureKind, now: Instant) {
        let mut inner = self.lock();
        let deque = inner.deque_mut(kind);
        prune(deque, now);
        if deque.len() >= MAX_EVENTS_PER_KIND {
            deque.pop_front();
        }
        deque.push_back(now);
    }

    /// Deterministic core of [`Self::recent`] (tests pass synthetic instants).
    pub fn counts_at(&self, now: Instant) -> LlmFailureCounts {
        let mut inner = self.lock();
        for kind in [
            LlmFailureKind::Extraction,
            LlmFailureKind::Ner,
            LlmFailureKind::Embedding,
            LlmFailureKind::Consolidation,
        ] {
            prune(inner.deque_mut(kind), now);
        }
        LlmFailureCounts {
            extraction: inner.extraction.len(),
            ner: inner.ner.len(),
            embedding: inner.embedding.len(),
            consolidation: inner.consolidation.len(),
            window_secs: WINDOW.as_secs(),
        }
    }
}

/// Process-wide tracker — workers record, status handlers read.
pub fn global() -> &'static LlmFailureTracker {
    static TRACKER: OnceLock<LlmFailureTracker> = OnceLock::new();
    TRACKER.get_or_init(LlmFailureTracker::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records inside the window are counted per category.
    #[test]
    fn records_count_per_category() {
        let t = LlmFailureTracker::default();
        let now = Instant::now();
        t.record_at(LlmFailureKind::Extraction, now);
        t.record_at(LlmFailureKind::Extraction, now);
        t.record_at(LlmFailureKind::Embedding, now);
        let counts = t.counts_at(now);
        assert_eq!(counts.extraction, 2);
        assert_eq!(counts.embedding, 1);
        assert_eq!(counts.ner, 0);
        assert_eq!(counts.consolidation, 0);
        assert_eq!(counts.total(), 3);
        assert_eq!(counts.window_secs, 3600);
    }

    /// Events older than the window are pruned from the counts.
    #[test]
    fn window_prunes_old_events() {
        let t = LlmFailureTracker::default();
        let t0 = Instant::now();
        t.record_at(LlmFailureKind::Ner, t0);
        let inside = t0 + WINDOW;
        assert_eq!(t.counts_at(inside).ner, 1, "boundary instant still counts");
        let outside = t0 + WINDOW + Duration::from_secs(1);
        assert_eq!(t.counts_at(outside).ner, 0, "past the window drops out");
    }

    /// The per-category cap bounds memory under failure storms.
    #[test]
    fn cap_bounds_event_storage() {
        let t = LlmFailureTracker::default();
        let now = Instant::now();
        for _ in 0..(MAX_EVENTS_PER_KIND + 50) {
            t.record_at(LlmFailureKind::Embedding, now);
        }
        assert_eq!(t.counts_at(now).embedding, MAX_EVENTS_PER_KIND);
    }
}
