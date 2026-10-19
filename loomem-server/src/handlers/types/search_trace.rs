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
    #[serde(skip_serializing_if = "Option::is_none")]
    retrieval_v2: Option<RetrievalV2Telemetry>,
}

#[derive(Debug, Serialize)]
struct RetrievalV2Telemetry {
    schema_version: u8,
    policy: &'static str,
    operation_kind: &'static str,
    structural_plan_sha256: &'static str,
    primary_lane: RetrievalLaneTelemetry,
    date_lane: RetrievalLaneTelemetry,
    merge: RetrievalMergeTelemetry,
    resources: RetrievalResourceTelemetry,
    fallback: RetrievalFallbackTelemetry,
}

#[derive(Debug, Serialize)]
struct RetrievalLaneTelemetry {
    status: &'static str,
    elapsed_ms: f64,
    bm25_leaf_attempts: usize,
    candidate_count: usize,
    candidate_bytes: usize,
}

#[derive(Debug, Serialize)]
struct RetrievalMergeTelemetry {
    elapsed_ms: f64,
    input_candidates: usize,
    unique_candidates: usize,
    duplicate_candidates: usize,
    output_candidates: usize,
    actual_admissions: usize,
    actual_removals: usize,
    changed_prefix_positions: usize,
    protected_prefix_preserved: bool,
}

#[derive(Debug, Serialize)]
struct RetrievalResourceTelemetry {
    scanned_candidates: usize,
    scanned_bytes: usize,
    scan_temporary_bytes_upper_bound: usize,
    merge_temporary_bytes_upper_bound: usize,
    temporary_bytes_upper_bound: usize,
}

#[derive(Debug, Serialize)]
struct RetrievalFallbackTelemetry {
    used: bool,
    stage: &'static str,
    reason_code: &'static str,
}

impl TraceMetadata {
    pub(crate) fn new(
        total_results_before_topk: usize,
        dedup_removed: usize,
        search_latency_us: u64,
        query_complexity: Option<String>,
        evidence: Option<EvidenceTrace>,
        operation: Option<OperationTelemetry>,
    ) -> Self {
        Self {
            total_results_before_topk,
            dedup_removed,
            search_latency_us,
            query_complexity,
            evidence,
            operation,
            retrieval_v2: None,
        }
    }

    pub(crate) fn with_retrieval_v2(
        mut self,
        identity: (&'static str, &'static str),
        primary: (&'static str, f64, usize, usize, usize),
        date: (&'static str, f64, usize, usize, usize),
        merge: (f64, usize, usize, usize, usize, usize, usize, usize, bool),
        resources: (usize, usize, usize, usize, usize),
        fallback: (bool, &'static str, &'static str),
    ) -> Self {
        if let Some(operation) = self.operation.as_mut() {
            operation.bm25_leaf_queries = primary.2.saturating_add(date.2);
            operation.changed_admissions = merge.5;
            operation.scanned_candidates = resources.0;
            operation.scanned_bytes = resources.1;
            operation.temporary_bytes = resources.4;
            operation.fallback_to_e0 |= fallback.0;
        }
        self.retrieval_v2 = Some(RetrievalV2Telemetry {
            schema_version: 3,
            policy: "E2",
            operation_kind: identity.0,
            structural_plan_sha256: identity.1,
            primary_lane: RetrievalLaneTelemetry {
                status: primary.0,
                elapsed_ms: primary.1,
                bm25_leaf_attempts: primary.2,
                candidate_count: primary.3,
                candidate_bytes: primary.4,
            },
            date_lane: RetrievalLaneTelemetry {
                status: date.0,
                elapsed_ms: date.1,
                bm25_leaf_attempts: date.2,
                candidate_count: date.3,
                candidate_bytes: date.4,
            },
            merge: RetrievalMergeTelemetry {
                elapsed_ms: merge.0,
                input_candidates: merge.1,
                unique_candidates: merge.2,
                duplicate_candidates: merge.3,
                output_candidates: merge.4,
                actual_admissions: merge.5,
                actual_removals: merge.6,
                changed_prefix_positions: merge.7,
                protected_prefix_preserved: merge.8,
            },
            resources: RetrievalResourceTelemetry {
                scanned_candidates: resources.0,
                scanned_bytes: resources.1,
                scan_temporary_bytes_upper_bound: resources.2,
                merge_temporary_bytes_upper_bound: resources.3,
                temporary_bytes_upper_bound: resources.4,
            },
            fallback: RetrievalFallbackTelemetry {
                used: fallback.0,
                stage: fallback.1,
                reason_code: fallback.2,
            },
        });
        self
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_plan_sha256: Option<String>,
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
        TraceMetadata::new(0, 0, 0, None, None, operation)
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
            operation_plan_sha256: Some("a".repeat(64)),
        };
        let json = serde_json::to_string(&trace(Some(telemetry))).unwrap();
        assert!(json.contains("operation_plan_sha256"));
        assert!(!json.contains("facets"));
        assert!(!json.contains("added_terms"));
    }

    #[test]
    fn e2_operation_timings_omit_full_plan_digest() {
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
            operation_kind: "TemporalWindow",
            operation_plan_sha256: None,
        };
        let json = serde_json::to_string(&trace(Some(telemetry))).unwrap();
        assert!(json.contains("classify_plan_ms"));
        assert!(json.contains("expansion_scan_rerank_ms"));
        assert!(!json.contains("operation_plan_sha256"));
    }

    #[test]
    fn retrieval_v2_serializes_exact_public_structure() {
        let value = serde_json::to_value(trace(None).with_retrieval_v2(
            (
                "TemporalWindow",
                "5a6964dc8cf3cc0b17d4b24a11c63e0f826d1b2ae83c7924179059eed0b9caa4",
            ),
            ("completed", 1.0, 1, 60, 1024),
            ("completed", 0.5, 1, 60, 512),
            (0.1, 120, 80, 40, 60, 2, 2, 0, true),
            (60, 2048, 4096, 4096, 8192),
            (false, "none", "none"),
        ))
        .unwrap();
        let retrieval = value["retrieval_v2"].as_object().unwrap();
        assert_eq!(retrieval.len(), 9);
        assert_eq!(retrieval["schema_version"], 3);
        assert_eq!(retrieval["policy"], "E2");
        assert_eq!(retrieval["primary_lane"].as_object().unwrap().len(), 5);
        assert_eq!(retrieval["date_lane"].as_object().unwrap().len(), 5);
        assert_eq!(retrieval["merge"].as_object().unwrap().len(), 9);
        assert_eq!(retrieval["resources"].as_object().unwrap().len(), 5);
        assert_eq!(retrieval["fallback"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn retrieval_v2_has_no_public_membership_or_date_fingerprint() {
        let json = serde_json::to_string(&trace(None).with_retrieval_v2(
            (
                "Plain",
                "d78fccba08d3caf087d5c120d42cb834a3a04f60f09fa3992d4116f45e7f9dc0",
            ),
            ("completed", 0.0, 1, 1, 0),
            ("not_applicable", 0.0, 0, 0, 0),
            (0.0, 1, 1, 0, 1, 0, 0, 0, true),
            (0, 0, 0, 0, 0),
            (false, "none", "none"),
        ))
        .unwrap();
        for forbidden in [
            "ordered_ids",
            "prefix20_ids",
            "resolved_interval",
            "reference_time",
            "operation_plan_sha256",
            "facets",
            "added_terms",
        ] {
            assert!(!json.contains(forbidden), "leaked {forbidden}");
        }
    }
}
