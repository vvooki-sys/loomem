//! Query taxonomy types (cycle/85).
//!
//! 5 query types per architecture brief §4 (summarization explicitly excluded
//! from MVP per arch §4.1). Tier weights per
//! arch §5.3 table — placeholder decimals, finalize w /87 eval.

use serde::Serialize;

/// Five query types per memory-routing architecture §4.
///
/// Detection priority (first match wins): DocumentLookup > Relational >
/// Temporal > Recent > Factual. Fallback default: Factual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    Factual,
    Temporal,
    Relational,
    Recent,
    DocumentLookup,
}

/// Per-signal weights, one row per QueryType, normalized to sum ≈ 1.0.
///
/// Five channels per arch §5.1 signal table (`valid_time` removed in /114
/// Phase 2; `doc_abstract` removed with the file registry in cycle/005).
/// Values consumed by `/86 RRF fusion` to weight per-channel rankings.
/// Decimals are placeholder tier values (H≈0.35, M≈0.20, L≈0.08, —=0)
/// renormalized so each row sums to 1.0; final values produced by `/87`
/// ablation eval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WeightVector {
    pub dense: f32,
    pub lexical: f32,
    pub entity_match: f32,
    pub graph_edge: f32,
    pub recency: f32,
}

/// Tier constants per arch §5.3 (placeholder values, /87 will replace).
const TIER_H: f32 = 0.35;
const TIER_M: f32 = 0.20;
const TIER_L: f32 = 0.08;
const TIER_ZERO: f32 = 0.00;

impl WeightVector {
    /// Static lookup of placeholder tier values per query type, normalized
    /// so every row sums to exactly 1.0 (within floating-point tolerance).
    ///
    /// The relative ordering H > M > L > 0 is what matters at this stage;
    /// `/87` replaces these with measured decimals derived from per-signal
    /// ablation on LongMemEval-S.
    #[must_use]
    pub fn for_type(query_type: QueryType) -> Self {
        let raw = match query_type {
            // factual: dense=H, lexical=H, entity=M, graph=L, recency=L
            QueryType::Factual => [TIER_H, TIER_H, TIER_M, TIER_L, TIER_L],
            // temporal: dense=M, lexical=L, entity=L, graph=L, recency=M
            // Recency carries temporal signal (50.5% top-1 change rate per /111 AC6 ablation).
            QueryType::Temporal => [TIER_M, TIER_L, TIER_L, TIER_L, TIER_M],
            // relational: dense=M, lexical=L, entity=H, graph=H, recency=L
            QueryType::Relational => [TIER_M, TIER_L, TIER_H, TIER_H, TIER_L],
            // recent: dense=L, lexical=L, entity=L, graph=0, recency=H
            QueryType::Recent => [TIER_L, TIER_L, TIER_L, TIER_ZERO, TIER_H],
            // document_lookup: dense=H, lexical=H, entity=L, graph=0, recency=L
            // (the dedicated doc_abstract channel was removed with the file
            //  registry in cycle/005; lookups now lean on dense + lexical)
            QueryType::DocumentLookup => [TIER_H, TIER_H, TIER_L, TIER_ZERO, TIER_L],
        };
        normalize(raw)
    }
}

fn normalize(raw: [f32; 5]) -> WeightVector {
    let sum: f32 = raw.iter().sum();
    debug_assert!(sum > 0.0, "weight row must have non-zero sum");
    WeightVector {
        dense: raw[0] / sum,
        lexical: raw[1] / sum,
        entity_match: raw[2] / sum,
        graph_edge: raw[3] / sum,
        recency: raw[4] / sum,
    }
}

/// Surface-level features detected by the parser, returned alongside the
/// chosen `QueryType`. `/86 RRF fusion` consumes these (entities for
/// `entity_match` signal, temporal_markers for `valid_time_match` signal)
/// independently of which type "won" the priority dispatch.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ParsedFeatures {
    pub entities: Vec<String>,
    pub temporal_markers: Vec<String>,
    pub doc_lookup_verbs: Vec<String>,
    pub language_hint: Option<String>,
}

/// Output of `query_classifier::classify`.
#[derive(Debug, Clone, Serialize)]
pub struct ClassifiedQuery {
    pub query_type: QueryType,
    pub weights: WeightVector,
    pub features: ParsedFeatures,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights_sum(w: &WeightVector) -> f32 {
        w.dense + w.lexical + w.entity_match + w.graph_edge + w.recency
    }

    #[test]
    fn test_weights_sum_to_one_per_type() {
        for qt in [
            QueryType::Factual,
            QueryType::Temporal,
            QueryType::Relational,
            QueryType::Recent,
            QueryType::DocumentLookup,
        ] {
            let w = WeightVector::for_type(qt);
            let sum = weights_sum(&w);
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "weights for {qt:?} sum to {sum} (expected ≈1.0)"
            );
        }
    }

    #[test]
    fn test_factual_weights_dense_and_lexical_dominant() {
        let w = WeightVector::for_type(QueryType::Factual);
        // dense and lexical share the top tier (H), entity_match is medium.
        assert!(w.dense > w.entity_match);
        assert!(w.lexical > w.entity_match);
        assert!(
            (w.dense - w.lexical).abs() < 1e-5,
            "dense and lexical both H"
        );
    }

    #[test]
    fn test_relational_weights_entity_and_graph_dominant() {
        let w = WeightVector::for_type(QueryType::Relational);
        assert!(w.entity_match > w.dense);
        assert!(w.graph_edge > w.dense);
        assert!((w.entity_match - w.graph_edge).abs() < 1e-5, "both H");
    }

    #[test]
    fn test_recent_weights_recency_dominant() {
        let w = WeightVector::for_type(QueryType::Recent);
        assert!(w.recency > w.dense);
        assert!(w.recency > w.lexical);
        assert!(w.recency > w.entity_match);
        assert_eq!(w.graph_edge, 0.0, "recent: graph_edge tier is —");
    }

    #[test]
    fn test_document_lookup_weights_dense_and_lexical_dominant() {
        let w = WeightVector::for_type(QueryType::DocumentLookup);
        assert!(w.dense > w.entity_match);
        assert!(w.lexical > w.entity_match);
        assert!(w.dense > w.recency);
        assert_eq!(w.graph_edge, 0.0, "document_lookup: graph_edge tier is —");
    }

    #[test]
    fn test_query_type_serialize_snake_case() {
        let json = serde_json::to_string(&QueryType::DocumentLookup).expect("serialize");
        assert_eq!(json, "\"document_lookup\"");
    }
}
