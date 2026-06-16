use serde::{Deserialize, Serialize};

/// Cycle/112: a single feedback rating event, append-only in RocksDB.
/// Storage key pattern: `feedback:<chunk_id>:<rated_at>:<event_id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEvent {
    /// uuid v4, server-generated
    pub event_id: String,
    /// foreign key to `Chunk.id`
    pub chunk_id: String,
    /// stream in which the chunk lives (for scoped query)
    pub stream: String,
    /// caller identity from `AuthContext` (stream_id of caller; later: distinct agent_id field)
    pub agent_id: String,
    /// self-reported by caller, e.g. "claude-sonnet-4-6"
    pub model_version: String,
    /// self-reported by caller, e.g. "loomem-feedback-v1"
    pub prompt_version: String,
    /// 0..4, validated server-side
    pub usefulness: u8,
    pub harmful: bool,
    /// 1..500 chars
    pub justification: String,
    /// optional task/trajectory grouping ID
    pub trajectory_id: Option<String>,
    /// unix ms, server-side (NOT client-supplied)
    pub rated_at: i64,
}

/// Build the RocksDB storage key for a feedback event.
/// Pattern: `feedback:<chunk_id>:<rated_at>:<event_id>`.
pub fn event_key(chunk_id: &str, rated_at: i64, event_id: &str) -> String {
    format!("feedback:{chunk_id}:{rated_at}:{event_id}")
}

/// RocksDB prefix used to scan all events for one chunk.
pub fn event_prefix_for_chunk(chunk_id: &str) -> String {
    format!("feedback:{chunk_id}:")
}
