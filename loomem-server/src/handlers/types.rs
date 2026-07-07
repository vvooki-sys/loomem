use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Date filter types
#[derive(Debug, Clone)]
pub enum DateFilter {
    Range(i64, i64), // from..to as unix timestamps
}

// Association types (ECA-21)
#[derive(Debug, Deserialize)]
pub struct AssociateRequest {
    pub query: String,
    pub stream_id: Option<String>,
    pub mechanisms: Option<Vec<String>>, // "graph", "temporal", "adjacent"
    pub count: Option<usize>,            // max 10
    pub hops: Option<usize>,             // for graph walk
}

#[derive(Debug, Serialize)]
pub struct AssociateResponse {
    pub associations: Vec<Association>,
    pub took_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct Association {
    pub content: String,
    pub score: f64,
    pub source_mechanism: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

// Request/Response types
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct StoreRequest {
    pub content: String,
    pub user_id: Option<String>,
    pub app_id: Option<String>,
    pub level: Option<i32>,
    pub stream_id: Option<String>,
    pub stream: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub persistent: Option<bool>,
    pub importance: Option<f64>,
    pub source: Option<String>,
    pub source_agent: Option<String>,
    pub source_session: Option<String>,
    pub source_channel: Option<String>,
    pub valid_from: Option<u64>,
    pub valid_until: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct StoreResponse {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SearchRequest {
    pub query: String,
    pub user_id: Option<String>,
    pub top_k: Option<usize>,
    pub stream: Option<String>,
    pub streams: Option<Vec<String>>,
    pub entity: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub valid_at: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
    pub filters: Option<serde_json::Value>,
    #[serde(default)]
    pub include_superseded: bool,
    #[serde(default)]
    pub trace: bool,
    pub fact_type: Option<String>,
    pub subject: Option<String>,
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub include_associations: bool,
    pub source_agent: Option<String>,
    pub exclude_source_agents: Option<Vec<String>>,
    pub scope: Option<crate::handlers::scope::ScopeParam>,
    /// Cycle/85: when `true`, response includes `query_classification` block
    /// with the deterministic-classifier output (type + features). Always
    /// off by default — production clients see no shape change.
    #[serde(default)]
    pub debug_query_classification: bool,
    /// Cycle/86: when `true`, each `SearchResult` gains a `signal_breakdown`
    /// block (5-channel RRF view per /85 weights + per-signal raw scores).
    /// Path A — additive only; the active fusion path is unchanged. Always
    /// off by default — production clients see no shape change.
    #[serde(default)]
    pub debug_signal_breakdown: bool,
    /// Cycle/012: when `true`, response includes a `channel_diagnostics`
    /// block with the pre-fusion top-N of every retrieval channel (BM25,
    /// vector, graph), the fused pool, the post-rank pool, and the rare-term
    /// lane decision trail. Diagnostic-only: the extra channel snapshots are
    /// computed alongside the pipeline without altering hot-path logic.
    /// Always off by default — production clients see no shape change.
    /// REST-only (not exposed through the MCP tool schema).
    #[serde(default)]
    pub debug_channels: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub took_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_sufficiency: Option<ContextSufficiency>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_metadata: Option<TraceMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub associations: Option<Vec<Association>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendations: Option<Vec<loomem_core::advisor::AdvisoryItem>>,
    /// Cycle/85: deterministic-classifier output (type, per-channel weights,
    /// parsed features). Surfaced only when the request set
    /// `debug_query_classification = true`. /86 RRF fusion is the consumer
    /// of this struct in the retrieval hot path; /85 just produces and logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_classification: Option<loomem_core::search::ClassifiedQuery>,
    /// Cycle/012: per-channel retrieval diagnostics (see
    /// `SearchRequest::debug_channels`). Present only when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_diagnostics: Option<ChannelDiagnostics>,
}

/// Cycle/012: one pre-fusion channel hit — `rank` is 1-indexed within its
/// channel, `score` is the channel-native score (BM25 raw, cosine
/// similarity, or graph proximity — scales are NOT comparable across
/// channels; that incomparability is exactly what the diagnostics exist to
/// make visible).
#[derive(Debug, Serialize)]
pub struct ChannelHit {
    pub rank: usize,
    pub id: String,
    pub score: f64,
}

/// Cycle/012: one fused-pool entry with its score components, capturing how
/// the weighted fusion + time decay shaped the candidate before (fused) and
/// after (post-rank) the boost stages.
#[derive(Debug, Serialize)]
pub struct FusedPoolHit {
    pub rank: usize,
    pub id: String,
    pub score: f64,
    pub bm25_score: f32,
    pub vector_score: f32,
    pub time_decay: f64,
}

/// Cycle/012: rare-term lane decision trail for one query.
#[derive(Debug, Serialize)]
pub struct RareTermLaneDiag {
    /// Corpus size used for the rarity threshold (stream-scoped when the
    /// query targets a single stream, index-global otherwise).
    pub n_docs: u64,
    pub df_threshold: u64,
    pub rare_tokens: Vec<loomem_core::search::RareToken>,
    /// Posting-list candidates (BM25-scored, capped at `candidate_cap`).
    pub candidates: Vec<ChannelHit>,
    /// Ids injected into the BM25 pool (candidates not already retrieved).
    pub injected_ids: Vec<String>,
}

/// Cycle/012: pre-fusion per-channel top-N + fused/post-rank pools + lane
/// trail. Every list is capped at a small N (20) — this is a debugging
/// surface, not a data export.
#[derive(Debug, Serialize, Default)]
pub struct ChannelDiagnostics {
    pub bm25_top: Vec<ChannelHit>,
    pub vector_top: Vec<ChannelHit>,
    pub graph_top: Vec<ChannelHit>,
    pub fused_pool: Vec<FusedPoolHit>,
    pub post_rank_top: Vec<FusedPoolHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<RareTermLaneDiag>,
}

#[derive(Debug, Serialize)]
pub struct ContextSufficiency {
    pub score: f64,
    pub coverage: f64,
    pub diversity: f64,
    pub confidence: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub score: f64,
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_info: Option<TraceInfo>,
    /// Cycle/86: per-channel RRF view (5 signal scores + ranks). Surfaced
    /// only when the request set `debug_signal_breakdown = true`. Path A —
    /// the active retrieval fusion is unchanged; this block is informational
    /// for `/87 per-type eval` to compare against the existing pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_breakdown: Option<loomem_core::search::SignalBreakdown>,
    /// Cycle/142 + /143 (ADR-017): content *form* of this result, hydrated from
    /// the sidecar keyspace (`content_type:<id>`) at response build time — Path
    /// A, additive. `None` for chunks not yet classified (no sidecar entry).
    /// `content_type_source` is always `llm` since /143 (the LLM is the sole
    /// classifier; the confidence band was dropped — `other` is the uncertainty
    /// bucket). Surfaced by **default** (the agent should not have to guess the
    /// chunk's role).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type_source: Option<String>,
}

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
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub status: String,
    pub uptime_secs: u64,
    pub config_summary: ConfigSummary,
    /// /157 S3: decode failures in the last full chunk scan
    /// (`None` until a scan has run since boot).
    pub undecodable_chunks: Option<usize>,
    /// /157 S3: windowed LLM failure counts per category.
    pub llm_failures_recent: loomem_core::llm_failures::LlmFailureCounts,
}

#[derive(Debug, Serialize)]
pub struct ConfigSummary {
    pub storage_enabled: bool,
    pub vector_enabled: bool,
    pub tantivy_enabled: bool,
    pub scheduler_enabled: bool,
    pub rocksdb_keys: u64,
    pub tantivy_docs: u64,
    pub embeddings_count: usize,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub struct BoostRequest {
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct BoostResponse {
    pub status: String,
    pub id: String,
    pub importance: f64,
}

#[derive(Debug, Serialize)]
pub struct NamespacesResponse {
    pub namespaces: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct RetagAllResponse {
    pub status: String,
    pub retagged_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct GenerateMemoryParams {
    pub user_id: Option<String>,
    pub stream: Option<String>,
    #[allow(dead_code)]
    pub max_sections: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub status: String,
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct PurgeNamespaceRequest {
    pub stream: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Serialize)]
pub struct PurgeNamespaceResponse {
    pub status: String,
    pub stream: String,
    pub dry_run: bool,
    pub deleted_count: usize,
    pub deleted_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct TagTierRequest {
    pub id: String,
    pub tier: String,
}

#[derive(Debug, Serialize)]
pub struct TagTierResponse {
    pub status: String,
    pub id: String,
    pub tier: String,
}

// Brief-compliant API types (new routes)
#[derive(Debug, Serialize)]
pub struct ApiDeleteResponse {
    pub deleted: bool,
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ApiUpdateMemoryRequest {
    pub content: Option<String>,
    pub confidence: Option<f64>,
    pub category: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiUpdateMemoryResponse {
    pub updated: bool,
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ApiPurgeRequest {
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Serialize)]
pub struct ApiPurgeResponse {
    pub namespace: String,
    pub count: usize,
    pub deleted: bool,
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReprocessLegacyRequest {
    #[serde(default = "default_reprocess_batch")]
    pub batch_size: usize,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_reprocess_limit")]
    pub limit: usize,
    /// Only process chunks from these sources (e.g. ["openclaw-memory", "api"]). Empty = all.
    pub sources: Option<Vec<String>>,
    /// Exclude chunks from these sources (e.g. ["longmemeval"]). Applied after `sources`.
    pub exclude_sources: Option<Vec<String>>,
    /// Force reprocess even if chunk already has extraction_meta.
    #[serde(default)]
    pub force: bool,
}

fn default_reprocess_batch() -> usize {
    50
}
fn default_reprocess_limit() -> usize {
    5000
}

#[derive(Debug, Deserialize)]
pub struct DreamRequest {
    pub stream: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContextPackRequest {
    pub query: Option<String>,
    pub stream: Option<String>,
    pub budget_tokens: Option<usize>,
    pub sections: Option<Vec<String>>,
    pub format: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ContextPackResponse {
    pub context: String,
    pub token_count: usize,
    pub sections_included: Vec<String>,
    pub sections_truncated: Vec<String>,
    pub sources: Vec<ContextSource>,
    pub coverage_score: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct ContextSource {
    pub chunk_id: String,
    pub section: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct WorkersPauseResponse {
    pub paused: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct WorkerControlResponse {
    pub name: String,
    pub paused: bool,
    pub message: String,
}

/// Snapshot of one background worker's state.
///
/// Note on `last_run_at` semantics: updated at START of work, not on success.
/// `last_success_at` is updated only when the task completes without error.
/// A "stuck/failing" badge can be derived from:
///   `last_success_at > 0 && last_run_at > 0 && (last_run_at - last_success_at) > 2 * interval_secs`
/// This avoids false positives on fresh-boot workers where `last_success_at == 0`.
///
/// When the scheduler is disabled (`config.scheduler.enabled = false`),
/// `last_run_at`, `last_success_at`, and `items_processed_total` will be 0
/// forever; pause toggles flip flags that no scheduler reads.
#[derive(Debug, Serialize)]
pub struct WorkerInfo {
    pub name: String,
    pub paused: bool,
    pub last_run_at: u64,
    pub last_success_at: u64,
    pub items_processed_total: u64,
    pub interval_secs: u64,
}

/// `paused` is `true` iff ALL workers are paused (derived field, not stored).
#[derive(Debug, Serialize)]
pub struct WorkersStatusResponse {
    pub paused: bool,
    pub workers: Vec<WorkerInfo>,
}

#[derive(Debug, Deserialize)]
pub struct StreamStatsParams {
    pub stream: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StreamStatsResponse {
    pub streams: HashMap<String, StreamStats>,
    pub total_streams: usize,
    pub generated_at: String,
}

#[derive(Debug, Serialize, Default)]
pub struct StreamStats {
    pub chunks: ChunkStats,
    pub consolidation: ConsolidationStats,
    pub graph: GraphStats,
    pub storage: StorageStats,
    pub activity: ActivityStats,
}

#[derive(Debug, Serialize, Default)]
pub struct ChunkStats {
    pub total: usize,
    pub by_level: HashMap<String, usize>,
    pub by_type: HashMap<String, usize>,
    pub dormant: usize,
    pub soft_deleted: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct ConsolidationStats {
    pub pending_l0: usize,
    pub consolidated_l1: usize,
    pub contradiction_count: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct GraphStats {
    pub entities: usize,
    pub edges: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct StorageStats {
    pub estimated_bytes: u64,
}

#[derive(Debug, Serialize, Default)]
pub struct ActivityStats {
    pub first_chunk_at: Option<String>,
    pub last_chunk_at: Option<String>,
    pub last_access_at: Option<String>,
}
