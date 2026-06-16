//! Layer 1 ambient memory — request/response shapes.
//!
//! Implements §10.7 #1 endpoint contract + §10.5.3 plain-fact + §10.6.1 marker
//! per `cycles/cycle-103a-layer1-endpoint-brief.md` AC-1 / AC-4 / AC-5 / AC-11.
//!
//! Plain-fact constraint on `AmbientSnippet::text` (§10.5.3 + B-2 fix):
//! declarative, 3rd-person, no metawords ("according to memory", "I remember"),
//! no provenance citation. Provenance metadata lives in `AmbientDebug` and is
//! server-side-only — Cowork-side renderer MUST NOT inject `debug` into agent
//! context (B-1 fix).

use serde::{Deserialize, Serialize};

use crate::search::SignalBreakdown;

/// Confidence tier per §10.5.2 thresholds. `Conflict` is orthogonal — derived
/// from inter-chunk contradiction (≥2 chunks `c ≥ 0.65` with contradictory
/// predicates), not from a continuous score range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    High,
    Medium,
    Low,
    Conflict,
}

impl Tier {
    /// Map continuous internal score `c ∈ [0,1]` to (non-conflict) tier per
    /// §10.5.2 thresholds. Callers detect conflict separately.
    #[must_use]
    pub fn from_score(c: f32) -> Self {
        if c >= 0.75 {
            Self::High
        } else if c >= 0.45 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// AC-1 request shape for `POST /v1/ambient`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AmbientRequest {
    pub user_id: String,
    pub scope: String,
    #[serde(default)]
    pub recent_turns: Option<Vec<RecentTurn>>,
    #[serde(default)]
    pub hint: Option<String>,
    /// /103a-MVP AC-6 retrieval-side completion (added under cycle/103c):
    /// when set, restricts Tantivy BM25 + vector retrieval to chunks whose
    /// `Chunk::stream` equals this value. `scope` (above) handles auth
    /// validation; `stream` handles retrieval isolation in multi-tenant
    /// Loomem instances (e.g. LongMemEval-S `lme_<question_id>` per-question
    /// streams). Defaults to None → no stream filter; auth.memberships
    /// post-filter applies as before. Cross-stream leakage in multi-tenant
    /// retrieval was a known /103a-MVP gap surfaced by /103c pre-flight.
    #[serde(default)]
    pub stream: Option<String>,
    /// AC-10: bypass cache for this request (force fresh retrieval).
    #[serde(default)]
    pub refresh: bool,
    /// §10.6.2 #2 explicit suppress: when true, payload returns
    /// `{snippets: [], marker_if_empty: null}` even when retrieval is empty.
    #[serde(default)]
    pub suppress_negative_marker: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecentTurn {
    pub role: String,
    pub content: String,
}

/// One ambient snippet — declarative plain-fact text + tier + score.
///
/// Per §10.5.3: `text` MUST be declarative ("User's playlist is named 'Summer
/// Vibes'."), 3rd-person, with no provenance citation. The synthesizer
/// enforces this; the type itself doesn't validate (validation lives in
/// `synthesis::render_text`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmbientSnippet {
    pub text: String,
    pub tier: Tier,
    pub score: f32,
}

/// AC-5 / §10.6.1 negative ambient marker. Shape mirrors positive snippets so
/// the agent's parser is uniform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarkerIfEmpty {
    pub ambient: NegativeAmbientStatus,
    pub checked: bool,
    pub scope: String,
    pub reason: NegativeReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NegativeAmbientStatus {
    NoRelevantContext,
}

/// 7 reason values per §10.6.1 enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NegativeReason {
    BelowThreshold,
    ZeroChunks,
    ColdStartGrace,
    ScopeEmpty,
    AllLowTier,
    DegradedRetrieval,
    TimeoutPartial,
}

/// AC-1 response shape. `Serialize`-only — the server emits responses; the
/// client side has no need to round-trip-deserialize within loomem-core
/// (tests build responses via struct literals). Per §10.7 #11, `debug` is
/// server-side-only telemetry that Cowork MUST NOT inject into the agent's
/// system prompt or any agent-visible context (B-1 fix).
#[derive(Debug, Clone, Serialize)]
pub struct AmbientResponse {
    pub snippets: Vec<AmbientSnippet>,
    pub marker_if_empty: Option<MarkerIfEmpty>,
    pub debug: AmbientDebug,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AmbientDebug {
    pub trace_ids: Vec<String>,
    pub signal_breakdown_per_snippet: Vec<SignalBreakdown>,
    pub latency_ms: AmbientLatencyMs,
    /// Set when AC-12 graceful-degradation path fires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct AmbientLatencyMs {
    pub retrieval: u32,
    pub synthesis: u32,
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_from_score_thresholds_match_spec_10_5_2() {
        assert_eq!(Tier::from_score(0.95), Tier::High);
        assert_eq!(Tier::from_score(0.75), Tier::High);
        assert_eq!(Tier::from_score(0.7499), Tier::Medium);
        assert_eq!(Tier::from_score(0.45), Tier::Medium);
        assert_eq!(Tier::from_score(0.4499), Tier::Low);
        assert_eq!(Tier::from_score(0.0), Tier::Low);
    }

    #[test]
    fn tier_from_score_does_not_emit_conflict() {
        // Conflict is orthogonal, derived from inter-chunk contradiction not score.
        for raw in [0.0, 0.45, 0.75, 1.0] {
            assert_ne!(Tier::from_score(raw), Tier::Conflict);
        }
    }

    #[test]
    fn ambient_snippet_round_trip_serde() {
        let s = AmbientSnippet {
            text: "User's playlist is named 'Summer Vibes'.".to_string(),
            tier: Tier::High,
            score: 0.91,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"tier\":\"high\""));
        assert!(json.contains("\"text\":\"User's playlist is named 'Summer Vibes'.\""));
        let back: AmbientSnippet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn marker_serializes_with_snake_case_reason() {
        let m = MarkerIfEmpty {
            ambient: NegativeAmbientStatus::NoRelevantContext,
            checked: true,
            scope: "private:user_42".to_string(),
            reason: NegativeReason::BelowThreshold,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["ambient"], "no_relevant_context");
        assert_eq!(json["reason"], "below_threshold");
        assert_eq!(json["checked"], true);
    }

    #[test]
    fn all_seven_reason_values_serialize_distinctly() {
        let all = [
            NegativeReason::BelowThreshold,
            NegativeReason::ZeroChunks,
            NegativeReason::ColdStartGrace,
            NegativeReason::ScopeEmpty,
            NegativeReason::AllLowTier,
            NegativeReason::DegradedRetrieval,
            NegativeReason::TimeoutPartial,
        ];
        let serialized: Vec<String> = all
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        assert_eq!(serialized.len(), 7);
        let unique: std::collections::HashSet<_> = serialized.iter().collect();
        assert_eq!(
            unique.len(),
            7,
            "all reason values must serialize distinctly"
        );
    }

    #[test]
    fn ambient_request_minimal_deserializes_with_defaults() {
        let json = r#"{"user_id": "u1", "scope": "private:u1"}"#;
        let req: AmbientRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.user_id, "u1");
        assert_eq!(req.scope, "private:u1");
        assert!(req.recent_turns.is_none());
        assert!(req.hint.is_none());
        assert!(!req.refresh);
        assert!(!req.suppress_negative_marker);
    }

    #[test]
    fn ambient_response_omits_debug_error_when_none() {
        let resp = AmbientResponse {
            snippets: vec![],
            marker_if_empty: None,
            debug: AmbientDebug::default(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("\"error\""),
            "debug.error must be omitted when None"
        );
    }
}
