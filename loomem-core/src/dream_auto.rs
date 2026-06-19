//! Auto-trigger bookkeeping for background dream consolidation.
//!
//! [`dream_run`](crate::dream::dream_run) is on-demand (the `memory_dream` MCP
//! tool / `POST /v1/dream`). This module adds an opt-in, count-based automatic
//! trigger: the server records every newly-persisted chunk per stream and, once
//! a configurable threshold is reached (and a cooldown has elapsed), signals the
//! caller to fire one dream run.
//!
//! The type is deliberately pure (no I/O, no clock) — it takes `now` as a
//! parameter so the threshold/cooldown logic is deterministically unit-testable.
//! Firing the actual `dream_run` (which needs the LLM client, storage and config)
//! is the caller's job in `loomem-server`; this layer only answers "should it
//! fire now?". The private-stream restriction also lives in the caller, so this
//! struct stays free of stream-classification concerns.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-stream counter state.
#[derive(Default)]
struct StreamState {
    /// Chunks persisted since the last fire (or since startup).
    pending: usize,
    /// When the last automatic run was armed; `None` until the first fire.
    last_fire: Option<Instant>,
}

/// Tracks newly-persisted chunks per stream and decides when an automatic dream
/// run should fire. Thread-safe (interior `Mutex`); cheap O(1) per record.
pub struct DreamAutoTrigger {
    threshold: usize,
    cooldown: Duration,
    streams: Mutex<HashMap<String, StreamState>>,
}

impl DreamAutoTrigger {
    /// Build a trigger. `threshold == 0` disables auto-firing entirely
    /// (every [`record`](Self::record) returns `false`).
    pub fn new(threshold: usize, cooldown_secs: u64) -> Self {
        Self {
            threshold,
            cooldown: Duration::from_secs(cooldown_secs),
            streams: Mutex::new(HashMap::new()),
        }
    }

    /// Whether auto-triggering is active (threshold > 0).
    pub fn is_enabled(&self) -> bool {
        self.threshold > 0
    }

    /// Record `n` newly-persisted chunks for `stream` and report whether a dream
    /// run should fire now.
    ///
    /// Returns `true` exactly once per threshold crossing: when the pending
    /// count reaches the threshold *and* the cooldown since the previous fire
    /// has elapsed. On a `true` return the pending counter is reset and the
    /// cooldown clock is armed, so concurrent callers won't double-fire. If the
    /// threshold is reached but the cooldown is still active, the pending count
    /// is retained (not reset), so the next eligible call fires promptly.
    pub fn record(&self, stream: &str, n: usize, now: Instant) -> bool {
        if self.threshold == 0 {
            return false;
        }
        // Mutex poisoning is non-fatal here: the counter is best-effort
        // bookkeeping, never a correctness invariant, so recover the inner value
        // rather than panic (CLAUDE.md: no unwrap/expect in production paths).
        let mut streams = self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = streams.entry(stream.to_string()).or_default();
        st.pending = st.pending.saturating_add(n);
        if st.pending < self.threshold {
            return false;
        }
        if let Some(last) = st.last_fire {
            if now.duration_since(last) < self.cooldown {
                return false;
            }
        }
        st.pending = 0;
        st.last_fire = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_threshold_does_not_fire() {
        let t = DreamAutoTrigger::new(50, 900);
        let now = Instant::now();
        assert!(!t.record("s", 49, now));
    }

    #[test]
    fn reaching_threshold_fires_and_resets() {
        let t = DreamAutoTrigger::new(50, 900);
        let now = Instant::now();
        assert!(!t.record("s", 49, now));
        assert!(t.record("s", 1, now)); // 49 + 1 == 50 → fire
                                        // Counter reset: another 49 must not fire yet.
        assert!(!t.record("s", 49, now + Duration::from_secs(1000)));
    }

    #[test]
    fn single_record_over_threshold_fires() {
        let t = DreamAutoTrigger::new(50, 900);
        assert!(t.record("s", 120, Instant::now())); // bulk import in one go
    }

    #[test]
    fn cooldown_blocks_second_fire_then_allows_after_elapsed() {
        let t = DreamAutoTrigger::new(10, 900);
        let now = Instant::now();
        assert!(t.record("s", 10, now)); // first fire arms cooldown
                                         // Within cooldown: reaches threshold again but must not fire.
        assert!(!t.record("s", 10, now + Duration::from_secs(100)));
        // Pending was retained (not reset) during cooldown, so once the cooldown
        // elapses the next record fires immediately.
        assert!(t.record("s", 0, now + Duration::from_secs(901)));
    }

    #[test]
    fn threshold_zero_disables() {
        let t = DreamAutoTrigger::new(0, 900);
        assert!(!t.is_enabled());
        assert!(!t.record("s", 10_000, Instant::now()));
    }

    #[test]
    fn streams_are_independent() {
        let t = DreamAutoTrigger::new(50, 900);
        let now = Instant::now();
        assert!(!t.record("a", 40, now));
        assert!(!t.record("b", 40, now));
        assert!(t.record("a", 10, now)); // a hits 50
        assert!(!t.record("b", 5, now)); // b only at 45
    }
}
