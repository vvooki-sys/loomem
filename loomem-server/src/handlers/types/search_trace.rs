use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct TraceInfo {
    pub level: String,
    pub source: Option<String>,
    pub is_latest: bool,
    pub created_at: u64,
    pub memory_type: Option<String>,
    pub importance: f64,
    pub access_count: u32,
    pub version: u32,
    pub superseded_by: Option<String>,
    pub prompt_version: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TraceMetadata {
    pub total_results_before_topk: usize,
    pub dedup_removed: usize,
    pub search_latency_us: u64,
    pub query_complexity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationTelemetry>,
}

/// Request-local E1 measurements. Plan contents are represented only by a
/// cryptographic digest so debug responses cannot disclose derived facets.
#[derive(Debug, Serialize)]
pub struct OperationTelemetry {
    pub classify_plan_ms: f64,
    pub expansion_scan_rerank_ms: f64,
    pub bm25_ms: f64,
    /// Original embedding, vector lookup, and the primary hybrid fusion.
    pub embedding_vector_ms: f64,
    /// Lexical expanded-query and rare-lane candidate merges.
    pub merge_ms: f64,
    /// Existing score/rank pipeline; operation scan time is reported above.
    pub rerank_ms: f64,
    pub final_filter_ms: f64,
    pub bm25_leaf_queries: usize,
    pub embedding_calls: usize,
    pub extra_embeddings: usize,
    pub operation_bpe_calls: usize,
    pub scanned_candidates: usize,
    pub changed_admissions: usize,
    pub scanned_bytes: usize,
    /// Conservative upper bound for operation-owned temporary allocations.
    pub temporary_bytes: usize,
    pub fallback_to_e0: bool,
    pub operation_kind: &'static str,
    pub operation_plan_sha256: String,
}

/// Ordered, ID-only snapshots; no claim of ingest or per-channel coverage.
#[derive(Debug, Serialize)]
pub struct EvidenceTrace {
    pub schema_version: u8,
    pub coverage: &'static str,
    pub stages: Vec<EvidenceStage>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceStage {
    pub stage: &'static str,
    pub chunk_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(operation: Option<OperationTelemetry>) -> TraceMetadata {
        TraceMetadata {
            total_results_before_topk: 0,
            dedup_removed: 0,
            search_latency_us: 0,
            query_complexity: None,
            evidence: None,
            operation,
        }
    }

    #[test]
    fn absent_operation_preserves_legacy_serialized_shape() {
        let value = serde_json::to_value(trace(None)).unwrap();
        assert!(value.get("operation").is_none());
        assert_eq!(value.as_object().unwrap().len(), 4);
    }

    #[test]
    fn operation_trace_contains_digest_without_plaintext_plan() {
        let telemetry = OperationTelemetry {
            classify_plan_ms: 0.1,
            expansion_scan_rerank_ms: 0.2,
            bm25_ms: 1.0,
            embedding_vector_ms: 2.0,
            merge_ms: 0.1,
            rerank_ms: 0.3,
            final_filter_ms: 0.1,
            bm25_leaf_queries: 2,
            embedding_calls: 1,
            extra_embeddings: 0,
            operation_bpe_calls: 0,
            scanned_candidates: 60,
            changed_admissions: 2,
            scanned_bytes: 1024,
            temporary_bytes: 2048,
            fallback_to_e0: false,
            operation_kind: "PreferenceAdvice",
            operation_plan_sha256: "a".repeat(64),
        };
        let json = serde_json::to_string(&trace(Some(telemetry))).unwrap();
        assert!(json.contains("operation_plan_sha256"));
        assert!(!json.contains("facets"));
        assert!(!json.contains("added_terms"));
    }
}
