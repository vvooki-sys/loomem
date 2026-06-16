//! Negative ambient marker constructor — AC-5 / §10.6.1 + §10.6.2.
//!
//! Two responsibilities:
//!
//! 1. **`build_marker`** — given `(scope, NegativeReason)`, emit a structured
//!    `MarkerIfEmpty` payload (parser symmetric with positive `AmbientSnippet`).
//! 2. **`should_suppress_marker`** — decide whether the negative marker should
//!    be omitted entirely (returns `MarkerOutcome::Suppressed` → response
//!    payload becomes `{snippets: [], marker_if_empty: null}`).
//!
//! Per §10.6.2 there are exactly 3 suppress cases (plus the explicit
//! all-low-tier NOT-suppress edge documented in §10.6.2):
//!
//! - **Cold-start grace** — first 5 turns of a brand-new user (zero chunks
//!   in scope at all). Avoids "memory checked, nothing relevant" noise on a
//!   user's first interactions.
//! - **Explicit suppress** — request flag `suppress_negative_marker: true`
//!   (Cowork integration with planned per-host fallback).
//! - **Scope-mismatch noise** — hint queries pointed at the wrong scope per
//!   §3 inheritance. Fall-through cleaner than misleading "memory checked"
//!   framing on the wrong scope.
//!
//! Probe-4 (`cycles/CC-PROBE4-2026-05-07-empty-ambient.md`) confirmed that
//! a structured marker payload alone (sans the §10.5.4 `no_tool_calls`
//! directive) is sufficient on Haiku 4.5 to trigger correct
//! `memory_search` invocation. The marker carries load-bearing semantics —
//! its 50-token floor (enforced separately by `cache::truncate_to_budget`)
//! is non-negotiable.

use super::types::{MarkerIfEmpty, NegativeAmbientStatus, NegativeReason};

/// Suppress-case input. Each field encodes one of the 3 §10.6.2 conditions
/// (plus the contextual signals `chunk_count_in_scope` + `turn_index` that
/// drive the cold-start grace window).
#[derive(Debug, Clone, Copy)]
pub struct SuppressContext {
    /// `true` when the request explicitly opted out of the negative marker
    /// (Cowork-side suppress flag per §10.6.2 #2).
    pub explicit_suppress: bool,
    /// Per-user, per-scope chunk count. `0` triggers cold-start grace IFF
    /// `turn_index < COLD_START_GRACE_TURNS`.
    pub chunk_count_in_scope: u64,
    /// 0-indexed turn number in the current conversation (Cowork supplies).
    pub turn_index: u32,
    /// Per §10.6.2 #3: hint queries point at a scope different from the
    /// user's primary stream (heuristic; precise calibration deferred to
    /// `/103a-pre`).
    pub scope_mismatch: bool,
}

/// Cold-start grace window in turns (§10.6.2 #1, MVP value).
pub const COLD_START_GRACE_TURNS: u32 = 5;

/// Outcome of the marker decision logic.
#[derive(Debug, Clone, PartialEq)]
pub enum MarkerOutcome {
    /// Negative marker SHOULD be emitted with the supplied payload.
    Emit(MarkerIfEmpty),
    /// Negative marker SHOULD be omitted (`marker_if_empty: null`).
    Suppressed,
}

/// Decide whether the negative marker is suppressed for this request,
/// or which structured marker to emit.
///
/// `reason` is the retrieval-side reason (e.g. `BelowThreshold`,
/// `AllLowTier`); the suppress decision overrides reason in the 3 §10.6.2
/// cases. **`AllLowTier` is NEVER suppressed** per §10.6.2 explicit edge case.
#[must_use]
pub fn decide_marker(scope: &str, reason: NegativeReason, ctx: SuppressContext) -> MarkerOutcome {
    if reason == NegativeReason::AllLowTier {
        return MarkerOutcome::Emit(build_marker(scope, reason));
    }
    if ctx.explicit_suppress {
        return MarkerOutcome::Suppressed;
    }
    if is_cold_start(ctx.chunk_count_in_scope, ctx.turn_index) {
        return MarkerOutcome::Suppressed;
    }
    if ctx.scope_mismatch {
        return MarkerOutcome::Suppressed;
    }
    MarkerOutcome::Emit(build_marker(scope, reason))
}

/// Construct the structured marker payload directly. Used by `decide_marker`
/// and exposed for tests / callers that have already passed the suppress gate.
#[must_use]
pub fn build_marker(scope: &str, reason: NegativeReason) -> MarkerIfEmpty {
    MarkerIfEmpty {
        ambient: NegativeAmbientStatus::NoRelevantContext,
        checked: true,
        scope: scope.to_string(),
        reason,
    }
}

/// `true` IFF the user has zero chunks in scope AND we're still inside the
/// cold-start grace window. First non-cold-start turn = `COLD_START_GRACE_TURNS`.
#[must_use]
pub fn is_cold_start(chunk_count_in_scope: u64, turn_index: u32) -> bool {
    chunk_count_in_scope == 0 && turn_index < COLD_START_GRACE_TURNS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(explicit: bool, chunks: u64, turn: u32, scope_mismatch: bool) -> SuppressContext {
        SuppressContext {
            explicit_suppress: explicit,
            chunk_count_in_scope: chunks,
            turn_index: turn,
            scope_mismatch,
        }
    }

    #[test]
    fn build_marker_shape_matches_10_6_1() {
        let m = build_marker("private:user_42", NegativeReason::BelowThreshold);
        assert_eq!(m.ambient, NegativeAmbientStatus::NoRelevantContext);
        assert!(m.checked);
        assert_eq!(m.scope, "private:user_42");
        assert_eq!(m.reason, NegativeReason::BelowThreshold);
    }

    #[test]
    fn explicit_suppress_returns_suppressed() {
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::BelowThreshold,
            ctx(true, 100, 50, false),
        );
        assert_eq!(outcome, MarkerOutcome::Suppressed);
    }

    #[test]
    fn cold_start_first_turn_zero_chunks_suppressed() {
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::ZeroChunks,
            ctx(false, 0, 0, false),
        );
        assert_eq!(outcome, MarkerOutcome::Suppressed);
    }

    #[test]
    fn cold_start_window_boundary_turn_5_emits_marker() {
        // Boundary: turn_index < COLD_START_GRACE_TURNS (=5). turn_index=5 is OUT.
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::ZeroChunks,
            ctx(false, 0, 5, false),
        );
        match outcome {
            MarkerOutcome::Emit(m) => assert_eq!(m.reason, NegativeReason::ZeroChunks),
            MarkerOutcome::Suppressed => panic!("turn 5 must NOT be cold-start"),
        }
    }

    #[test]
    fn cold_start_does_not_suppress_when_user_has_chunks() {
        // 1 chunk → not a brand-new user → cold-start grace doesn't apply.
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::BelowThreshold,
            ctx(false, 1, 0, false),
        );
        match outcome {
            MarkerOutcome::Emit(_) => {}
            MarkerOutcome::Suppressed => panic!("user with chunks must not be suppressed"),
        }
    }

    #[test]
    fn scope_mismatch_suppresses() {
        let outcome = decide_marker(
            "shared:stream_x",
            NegativeReason::BelowThreshold,
            ctx(false, 50, 10, true),
        );
        assert_eq!(outcome, MarkerOutcome::Suppressed);
    }

    #[test]
    fn all_low_tier_never_suppressed_even_under_cold_start() {
        // §10.6.2 explicit: AllLowTier fires regardless. Cold-start path
        // would normally suppress, but AllLowTier overrides.
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::AllLowTier,
            ctx(false, 0, 0, false),
        );
        match outcome {
            MarkerOutcome::Emit(m) => assert_eq!(m.reason, NegativeReason::AllLowTier),
            MarkerOutcome::Suppressed => panic!("AllLowTier must never be suppressed"),
        }
    }

    #[test]
    fn all_low_tier_never_suppressed_even_under_explicit_suppress() {
        // §10.6.2 edge case: AllLowTier overrides explicit suppress too —
        // the agent ALWAYS needs to know "memory checked, all hits weak."
        let outcome = decide_marker(
            "private:u1",
            NegativeReason::AllLowTier,
            ctx(true, 100, 50, false),
        );
        match outcome {
            MarkerOutcome::Emit(m) => assert_eq!(m.reason, NegativeReason::AllLowTier),
            MarkerOutcome::Suppressed => panic!("AllLowTier overrides explicit suppress"),
        }
    }

    #[test]
    fn is_cold_start_flips_at_chunk_count() {
        assert!(is_cold_start(0, 0));
        assert!(is_cold_start(0, 4));
        assert!(!is_cold_start(0, 5));
        assert!(!is_cold_start(1, 0));
    }
}
