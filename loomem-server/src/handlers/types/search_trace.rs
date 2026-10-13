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
